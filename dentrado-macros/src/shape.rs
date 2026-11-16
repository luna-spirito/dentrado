//! `#[derive(Shape)]` — the textual identity of a serialized layout.
//!
//! The shape walks the type definition: names, field order, field types. It
//! follows the *encoding*, not the source syntax — `#[serde(skip)]` omits,
//! `#[serde(transparent)]` collapses, and names are ignored because postcard
//! encodes none. An attribute the walk cannot follow is a compile error: a
//! custom encoding belongs in a hand-written `Serialize`/`Deserialize` pair
//! with a hand-written `Shape` impl beside it, never in a per-field override.
//!
//! See `dentrado_types::Shape` for the grammar, the hand impls, and the
//! faithfulness rule.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::punctuated::Punctuated;
use syn::{Attribute, Data, DeriveInput, Field, Fields, Meta, Token, Type};

/// Expand `#[derive(Shape)]`.
pub(crate) fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let name = &input.ident;
    let container = Options::parse(&input.attrs)?;

    // The head renders every type parameter, which needs its shape.
    let mut generics = input.generics.clone();
    let where_clause = generics.make_where_clause();
    for tp in input.generics.type_params() {
        let ident = tp.ident.clone();
        where_clause
            .predicates
            .push(syn::parse_quote! { #ident: ::dentrado_types::Shape });
    }
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    impl_for(
        input,
        &quote! { #impl_generics },
        &quote! { #ty_generics },
        &quote! { #where_clause },
        &container,
        name,
    )
}

fn impl_for(
    input: &DeriveInput,
    impl_generics: &TokenStream2,
    ty_generics: &TokenStream2,
    where_clause: &TokenStream2,
    container: &Options,
    name: &syn::Ident,
) -> syn::Result<TokenStream2> {
    let body = if container.transparent {
        let ty = transparent_field(input)?;
        quote! { <#ty as ::dentrado_types::Shape>::shape() }
    } else {
        let head = head(input, name);
        let append = match &input.data {
            Data::Struct(s) => fields_shape(&s.fields)?,
            Data::Enum(e) => variants_shape(e)?,
            Data::Union(_) => {
                return Err(syn::Error::new_spanned(
                    input,
                    "Shape cannot be derived for unions",
                ));
            }
        };
        quote! {
            #head
            #append
            out
        }
    };

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics ::dentrado_types::Shape for #name #ty_generics #where_clause {
            fn shape() -> ::std::string::String {
                #body
            }
        }
    })
}

/// `let mut out = String::from("Name");` plus `Name<T, N>` for the generics.
fn head(input: &DeriveInput, name: &syn::Ident) -> TokenStream2 {
    let name = name.to_string();
    let mut params: Vec<TokenStream2> = input
        .generics
        .type_params()
        .map(|tp| {
            let ident = &tp.ident;
            quote! { out.push_str(&<#ident as ::dentrado_types::Shape>::shape()); }
        })
        .collect();
    params.extend(input.generics.const_params().map(|cp| {
        let ident = &cp.ident;
        quote! { out.push_str(&::std::format!("{}", #ident)); }
    }));

    let mut rendered = Vec::new();
    for (at, param) in params.iter().enumerate() {
        if at > 0 {
            rendered.push(quote! { out.push(','); });
        }
        rendered.push(param.clone());
    }

    if params.is_empty() {
        quote! { let mut out = ::std::string::String::from(#name); }
    } else {
        quote! {
            let mut out = ::std::string::String::from(#name);
            out.push('<');
            #( #rendered )*
            out.push('>');
        }
    }
}

fn transparent_field(input: &DeriveInput) -> syn::Result<&Type> {
    let Data::Struct(s) = &input.data else {
        return Err(syn::Error::new_spanned(
            input,
            "`#[serde(transparent)]` is only meaningful on a struct",
        ));
    };
    let mut live = Vec::new();
    for field in &s.fields {
        if !Options::parse(&field.attrs)?.skip {
            live.push(&field.ty);
        }
    }
    match live.as_slice() {
        [ty] => Ok(ty),
        [] => Err(syn::Error::new_spanned(
            input,
            "`#[serde(transparent)]` needs one field",
        )),
        _ => Err(syn::Error::new_spanned(
            input,
            "`#[serde(transparent)]` allows exactly one field",
        )),
    }
}

fn fields_shape(fields: &Fields) -> syn::Result<TokenStream2> {
    let parts = field_parts(fields)?;
    Ok(match fields {
        Fields::Unit => quote! {},
        Fields::Unnamed(_) => quote! {
            out.push('(');
            #( #parts )*
            out.push(')');
        },
        Fields::Named(_) => quote! {
            out.push('{');
            #( #parts )*
            out.push('}');
        },
    })
}

/// One fragment per serialized field, with the separators already between
/// them — a skipped field is absent rather than empty.
fn field_parts(fields: &Fields) -> syn::Result<Vec<TokenStream2>> {
    let mut parts = Vec::new();
    for field in fields {
        let options = Options::parse(&field.attrs)?;
        if options.transparent {
            return Err(syn::Error::new_spanned(
                field,
                "`#[serde(transparent)]` belongs on the container, not a field",
            ));
        }
        if options.skip {
            continue;
        }
        if !parts.is_empty() {
            parts.push(quote! { out.push(','); });
        }
        if let Some(name) = &field.ident {
            let name = name.to_string();
            parts.push(quote! { out.push_str(#name); out.push(':'); });
        }
        parts.push(field_shape(field));
    }
    Ok(parts)
}

fn field_shape(field: &Field) -> TokenStream2 {
    let ty = &field.ty;
    quote! { out.push_str(&<#ty as ::dentrado_types::Shape>::shape()); }
}

fn variants_shape(data: &syn::DataEnum) -> syn::Result<TokenStream2> {
    let mut parts = Vec::new();
    for variant in &data.variants {
        let options = Options::parse(&variant.attrs)?;
        if options.skip {
            continue;
        }
        if options.transparent {
            return Err(syn::Error::new_spanned(
                variant,
                "`#[serde(transparent)]` is not supported on variants",
            ));
        }
        if !parts.is_empty() {
            parts.push(quote! { out.push(','); });
        }
        let name = variant.ident.to_string();
        parts.push(quote! { out.push_str(#name); });
        parts.push(fields_shape(&variant.fields)?);
    }
    Ok(quote! {
        out.push('{');
        #( #parts )*
        out.push('}');
    })
}

/// What a single `#[serde(...)]` site says about layout.
#[derive(Default)]
struct Options {
    skip: bool,
    transparent: bool,
}

impl Options {
    fn parse(attrs: &[Attribute]) -> syn::Result<Self> {
        let mut options = Self::default();
        for attr in attrs {
            if attr.path().is_ident("serde") {
                options.parse_serde(attr)?;
            }
        }
        Ok(options)
    }

    fn parse_serde(&mut self, attr: &Attribute) -> syn::Result<()> {
        for meta in attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)? {
            let Some(ident) = meta.path().get_ident() else {
                return Err(syn::Error::new_spanned(
                    &meta,
                    "unsupported `serde` attribute",
                ));
            };
            match ident.to_string().as_str() {
                "skip" => self.skip = true,
                "transparent" => self.transparent = true,
                // Layout-neutral: postcard encodes no names, no defaults and
                // no decode-only aliases.
                "rename"
                | "rename_all"
                | "rename_all_fields"
                | "default"
                | "alias"
                | "borrow"
                | "bound"
                | "deny_unknown_fields"
                | "crate"
                | "other" => {}
                other => {
                    return Err(syn::Error::new_spanned(
                        &meta,
                        format!(
                            "`#[serde({other})]` changes the encoding and is not derivable; \
                             implement `Shape` manually for this type, beside its serde impl"
                        ),
                    ));
                }
            }
        }
        Ok(())
    }
}
