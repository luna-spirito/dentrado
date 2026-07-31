//! Proc-macro support for [`dentrado`].
//!
//! Currently provides [`derive@Localizable`], which generates an
//! [`impl dentrado::types::Localizable`] that localizes every field of a struct
//! or enum recursively. See [`dentrado::types::Localizable`] for the trait.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Fields, Ident, parse_macro_input, parse_quote};

mod gears;

/// Derive [`dentrado::types::Localizable`].
///
/// Generates an impl that calls `.localize(r).await?` on every field. Use
/// `#[localizable(skip)]` on a field to leave it untouched (for plain-data
/// leaves that are not — and should not be — `Localizable`, e.g. a cache
/// payload wrapped in `Arc`).
///
/// Every type parameter gets a `T: Localizable` bound on the generated impl.
#[proc_macro_derive(Localizable, attributes(localizable))]
pub fn derive_localizable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(&input)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// Define a runtime's gears declaratively. See [`gears`].
#[proc_macro_attribute]
pub fn gears(attr: TokenStream, item: TokenStream) -> TokenStream {
    gears::gears_impl(attr, item)
}

/// `#[gear]` marker attribute on a gear fn. Consumed by [`gears`]; expands to
/// its input unchanged so the attribute is a known, importable name while the
/// real codegen happens in the [`gears`] aggregator.
#[proc_macro_attribute]
pub fn gear(_attr: TokenStream, item: TokenStream) -> TokenStream {
    gears::gear_impl(_attr, item)
}

fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let name = &input.ident;

    // Add a `T: Localizable` bound for every type parameter (simple, uniform
    // rule; our current derive sites are monomorphic anyway).
    let mut generics = input.generics.clone();
    {
        let where_clause = generics.make_where_clause();
        for tp in input.generics.type_params() {
            let ident = tp.ident.clone();
            where_clause
                .predicates
                .push(parse_quote! { #ident: dentrado::types::Localizable });
        }
    }
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let body: TokenStream2 = match &input.data {
        Data::Struct(s) => {
            // Struct-level `#[localizable(skip)]` ⇒ whole struct is identity.
            if is_skipped(&input.attrs) {
                quote! { { Ok(self) } }
            } else {
                let plans = plan_fields(&s.fields, false);
                let (pat, ctor) = field_parts(&plans);
                if matches!(s.fields, Fields::Unit) {
                    quote! { { Ok(self) } }
                } else {
                    quote! {
                        {
                            let Self #pat = self;
                            Ok(Self #ctor)
                        }
                    }
                }
            }
        }
        Data::Enum(e) => {
            let arms = e.variants.iter().map(|v| {
                let vident = &v.ident;
                // Variant-level `#[localizable(skip)]` ⇒ all fields passthrough.
                let variant_skip = is_skipped(&v.attrs);
                let plans = plan_fields(&v.fields, variant_skip);
                let (pat, ctor) = field_parts(&plans);
                if matches!(v.fields, Fields::Unit) {
                    quote! { Self::#vident => Ok(Self::#vident), }
                } else {
                    quote! { Self::#vident #pat => Ok(Self::#vident #ctor), }
                }
            });
            quote! {
                {
                    match self {
                        #( #arms )*
                    }
                }
            }
        }
        Data::Union(_) => {
            return Err(syn::Error::new_spanned(
                input,
                "Localizable cannot be derived for unions",
            ));
        }
    };

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics dentrado::types::Localizable for #name #ty_generics #where_clause {
            async fn localize<R: dentrado::types::Remapper>(
                self,
                r: &mut R,
            ) -> ::core::result::Result<Self, R::Err> {
                #body
            }
        }
    })
}

/// Does this attribute list carry `#[localizable(skip)]`?
///
/// Recognized on fields (skip just that field), variants (skip every field of
/// the variant), and the whole struct/enum (whole type is identity).
fn is_skipped(attrs: &[syn::Attribute]) -> bool {
    for attr in attrs {
        if attr.path().is_ident("localizable")
            && let syn::Meta::List(ref ml) = attr.meta
            && let Ok(ident) = syn::parse2::<Ident>(ml.tokens.clone())
            && ident == "skip"
        {
            return true;
        }
    }
    false
}

/// Per-field reconstruction plan: the binding ident, the value expression to
/// reconstruct from it, and (for named fields) the field name.
struct FieldPlan {
    binding: Ident,
    value: TokenStream2,
    named: Option<Ident>,
}

fn plan_fields(fields: &Fields, parent_skip: bool) -> Vec<FieldPlan> {
    match fields {
        Fields::Named(named) => named
            .named
            .iter()
            .map(|f| {
                let field_name = f.ident.clone().expect("named field has an ident");
                let skipped = parent_skip || is_skipped(&f.attrs);
                let value = if skipped {
                    quote! { #field_name }
                } else {
                    quote! { #field_name.localize(r).await? }
                };
                FieldPlan {
                    binding: field_name.clone(),
                    value,
                    named: Some(field_name),
                }
            })
            .collect(),
        Fields::Unnamed(unnamed) => unnamed
            .unnamed
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let binding = format_ident!("f{}", i);
                let skipped = parent_skip || is_skipped(&f.attrs);
                let value = if skipped {
                    quote! { #binding }
                } else {
                    quote! { #binding.localize(r).await? }
                };
                FieldPlan {
                    binding: binding.clone(),
                    value,
                    named: None,
                }
            })
            .collect(),
        Fields::Unit => vec![],
    }
}

/// Render `(pattern_fragment, constructor_fragment)` for a set of fields.
/// - named:   `{ a, b }` / `{ a: <a-value>, b: <b-value> }`
/// - unnamed: `(f0, f1)` / `(<f0-value>, <f1-value>)`
/// - empty:   `` / `` (so unit variants emit `Self::V => Ok(Self::V)`)
fn field_parts(plans: &[FieldPlan]) -> (TokenStream2, TokenStream2) {
    if plans.is_empty() {
        return (quote! {}, quote! {});
    }
    let named = plans[0].named.is_some();
    if named {
        let bindings = plans.iter().map(|p| &p.binding);
        let names = plans.iter().map(|p| p.named.as_ref().expect("named"));
        let values = plans.iter().map(|p| &p.value);
        let pat = quote! { { #( #bindings ),* } };
        let ctor = quote! { { #( #names : #values ),* } };
        (pat, ctor)
    } else {
        let bindings = plans.iter().map(|p| &p.binding);
        let values = plans.iter().map(|p| &p.value);
        let pat = quote! { ( #( #bindings ),* ) };
        let ctor = quote! { ( #( #values ),* ) };
        (pat, ctor)
    }
}
