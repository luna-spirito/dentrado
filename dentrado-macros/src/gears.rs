//! The `#[gears]` module attribute + `#[gear]` fn marker: define a runtime's
//! gears as plain `async fn`s and let the macro generate the `GearId` /
//! `GearOut` / `GearCache` / `Group` enums, the `IsRuntime` impl, the
//! `GlobalHash` impl for `Group`, and the typed dependency accessors.
//!
//! Each gear is one function — its signature *is* the gear spec:
//!
//! ```ignore
//! #[gears(runtime = KolorinkoRT)]
//! pub(crate) mod gears {
//!     #[gear(timer(period = secs(900)), name = Repo)]
//!     pub(crate) async fn repo<S: Storage<KolorinkoRT>>(
//!         repo_meta: RepoMeta,   // id field → GearId::Repo(RepoMeta)
//!         tick: bool,            // timer tick
//!         cache: &mut RepoCache, // → GearCache::Repo(RepoCache)
//!     ) -> Arc<RepoData> {}      // → GearOut::RepoOut(Arc<RepoData>)
//! }
//! ```
//!
//! The `#[gear]` marker carries only the metadata the signature cannot express
//! (kind: timer/event, the period expression, and the enum *variant* base
//! name). The id fields, cache type, and output type are all read straight off
//! the `fn`, so there is no DSL restating them.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    FnArg, Ident, Item, ItemMod, Pat, ReturnType, Token, Type,
    parse::{Parse, ParseStream, Parser},
    punctuated::Punctuated,
};

/// `#[gear(...)]` marker. Expands to its input unchanged — it exists only so
/// the attribute is a known, importable name. The real work is done by
/// `#[gears]`, which reads each `#[gear]`-marked fn before this identity ever
/// runs.
pub(crate) fn gear_impl(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// `#[gears(runtime = <Type>)]` on a module.
pub(crate) fn gears_impl(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr: GearsAttr = syn::parse_macro_input!(attr);
    let item: ItemMod = syn::parse_macro_input!(item);
    expand(attr, item)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

// ── attribute arg parsing ─────────────────────────────────────────────────

/// `runtime = <Type>` (the only key for now).
struct GearsAttr {
    runtime: Type,
}

impl Parse for GearsAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let kw: Ident = input.parse()?;
        if kw != "runtime" {
            return Err(syn::Error::new(kw.span(), "expected `runtime =`"));
        }
        input.parse::<Token![=]>()?;
        let runtime: Type = input.parse()?;
        Ok(GearsAttr { runtime })
    }
}

enum GearArg {
    Timer { period: TokenStream2 },
    Event,
    Name(Ident),
}

impl Parse for GearArg {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let kw: Ident = input.parse()?;
        match kw.to_string().as_str() {
            "timer" => {
                let inner;
                syn::parenthesized!(inner in input);
                let p: Ident = inner.parse()?;
                if p != "period" {
                    return Err(syn::Error::new(p.span(), "expected `period =`"));
                }
                inner.parse::<Token![=]>()?;
                // Capture the period as a raw token stream rather than an
                // `Expr`: `Expr::parse` refuses an expression immediately
                // followed by a comma (e.g. `timer(period = …,)`), and a
                // period may be arbitrarily complex (closures, calls). Strip a
                // single trailing comma so it interpolates cleanly into the
                // generated `GearMeta::Timer { period: … }` literal.
                let period: TokenStream2 = inner.parse()?;
                Ok(GearArg::Timer {
                    period: strip_trailing_comma(period),
                })
            }
            "event" => Ok(GearArg::Event),
            "name" => {
                input.parse::<Token![=]>()?;
                let n: Ident = input.parse()?;
                Ok(GearArg::Name(n))
            }
            other => Err(syn::Error::new(
                kw.span(),
                format!("unknown gear key `{other}`"),
            )),
        }
    }
}

enum KindSpec {
    Timer { period: TokenStream2 },
    Event,
}

// ── per-gear spec extracted from a `#[gear]`-marked fn ─────────────────────

/// One param of a gear fn, classified by role. The order is preserved so the
/// generated `run_step` can call the fn with arguments in the right positions.
enum ParamRole {
    /// An id field — becomes a `GearId` variant field. The match binds it by
    /// this name and the run call passes it through.
    IdField { name: Ident, ty: Type },
    /// `tick: bool` (timer gears only). The match binds `tick` from `GearInput`.
    Tick,
    /// `ctx: &mut GearCtx<…>`. The run call forwards `run_step`'s `ctx`.
    Ctx,
    /// `cache: &mut T`. The match binds `&mut T` from `GearCache`.
    Cache { ty: Type },
}

struct GearSpec {
    /// Base enum variant name (`Repo`, `Load`).
    name: Ident,
    /// The fn's own name (`repo`, `load_page`).
    fn_name: Ident,
    is_async: bool,
    params: Vec<ParamRole>,
    out_ty: Type,
    kind: KindSpec,
}

impl GearSpec {
    fn id_fields(&self) -> Vec<(Ident, Type)> {
        self.params
            .iter()
            .filter_map(|p| match p {
                ParamRole::IdField { name, ty } => Some((name.clone(), ty.clone())),
                _ => None,
            })
            .collect()
    }

    fn cache_ty(&self) -> Option<&Type> {
        self.params.iter().find_map(|p| match p {
            ParamRole::Cache { ty } => Some(ty),
            _ => None,
        })
    }
}

// ── type-pattern helpers (classify a fn param by its declared type) ────────

/// If `ty` is `&mut U`, return `U` (the borrowed element, with the `Box`
/// peeled off).
fn as_mut_ref_elem(ty: &Type) -> Option<&Type> {
    if let Type::Reference(r) = ty {
        r.mutability.is_some().then_some(r.elem.as_ref())
    } else {
        None
    }
}

/// Last path segment ident of a type, if it is a (possibly generic) path.
fn path_last_ident(ty: &Type) -> Option<&Ident> {
    if let Type::Path(tp) = ty {
        tp.path.segments.last().map(|s| &s.ident)
    } else {
        None
    }
}

/// Drop a single trailing top-level comma from a token stream. Used for the
/// `timer(period = …)` payload, which may carry a trailing comma (e.g.
/// `timer(period = EXPR,)`); `Expr::parse` rejects an expression followed by a
/// comma, so we keep the payload as a raw token stream and trim it before
/// interpolating into the generated `GearMeta::Timer { period: … }` literal.
fn strip_trailing_comma(ts: TokenStream2) -> TokenStream2 {
    let mut trees: Vec<proc_macro2::TokenTree> = ts.into_iter().collect();
    if let Some(proc_macro2::TokenTree::Punct(p)) = trees.last()
        && p.as_char() == ','
    {
        trees.pop();
    }
    trees.into_iter().collect()
}

/// Does this attribute read as `#[gear]` / `#[dentrado::gear]` (matched by the
/// last path segment so callers may use a bare or fully-qualified path)?
fn is_gear_attr(attr: &syn::Attribute) -> bool {
    attr.path()
        .segments
        .last()
        .is_some_and(|s| s.ident == "gear")
}

/// Extract a [`GearSpec`] from a `#[gear(...)]`-marked fn.
fn extract_gear(fn_item: &syn::ItemFn) -> syn::Result<GearSpec> {
    // Find the `#[gear(...)]` attribute and parse its args.
    let gear_attr = fn_item
        .attrs
        .iter()
        .find(|a| is_gear_attr(a))
        .ok_or_else(|| syn::Error::new_spanned(fn_item, "expected `#[gear(...)]`"))?;
    let tokens = match &gear_attr.meta {
        syn::Meta::List(ml) => ml.tokens.clone(),
        _ => {
            return Err(syn::Error::new_spanned(
                gear_attr,
                "expected `#[gear(...)]`",
            ));
        }
    };
    let args: Punctuated<GearArg, Token![,]> =
        Punctuated::<GearArg, Token![,]>::parse_terminated.parse2(tokens)?;

    let mut name: Option<Ident> = None;
    let mut kind: Option<KindSpec> = None;
    for arg in args {
        match arg {
            GearArg::Timer { period } => kind = Some(KindSpec::Timer { period }),
            GearArg::Event => kind = Some(KindSpec::Event),
            GearArg::Name(n) => name = Some(n),
        }
    }
    let name = name.ok_or_else(|| {
        syn::Error::new_spanned(gear_attr, "`#[gear(...)]` must set `name = <Variant>`")
    })?;
    let kind = kind.ok_or_else(|| {
        syn::Error::new_spanned(gear_attr, "`#[gear(...)]` must set `timer(...)` or `event`")
    })?;

    let is_async = fn_item.sig.asyncness.is_some();
    let out_ty = match &fn_item.sig.output {
        ReturnType::Type(_, ty) => ty.as_ref().clone(),
        ReturnType::Default => {
            return Err(syn::Error::new_spanned(
                &fn_item.sig,
                "gear fn must return a value (the gear output type)",
            ));
        }
    };

    let mut params = Vec::new();
    for arg in &fn_item.sig.inputs {
        let FnArg::Typed(pt) = arg else {
            return Err(syn::Error::new_spanned(arg, "gear fn cannot take `self`"));
        };
        let ty = pt.ty.as_ref();
        if let Some(elem) = as_mut_ref_elem(ty) {
            if path_last_ident(elem) == Some(&format_ident!("GearCtx")) {
                params.push(ParamRole::Ctx);
            } else {
                params.push(ParamRole::Cache { ty: elem.clone() });
            }
        } else {
            // By value: either a `tick: bool` or a named id field.
            let Pat::Ident(pi) = pt.pat.as_ref() else {
                return Err(syn::Error::new_spanned(
                    pt,
                    "gear fn by-value params must be named (`tick` or an id field)",
                ));
            };
            if pi.ident == "tick" {
                params.push(ParamRole::Tick);
            } else {
                params.push(ParamRole::IdField {
                    name: pi.ident.clone(),
                    ty: ty.clone(),
                });
            }
        }
    }

    // Validate kind-specific expectations.
    match &kind {
        KindSpec::Timer { .. } if !params.iter().any(|p| matches!(p, ParamRole::Tick)) => {
            return Err(syn::Error::new_spanned(
                &fn_item.sig,
                "timer gear fn must take a `tick: bool` param",
            ));
        }
        KindSpec::Event if params.iter().any(|p| matches!(p, ParamRole::Tick)) => {
            return Err(syn::Error::new_spanned(
                &fn_item.sig,
                "event gear fn must not take a `tick` param",
            ));
        }
        _ => {}
    }

    // Every gear needs a cache.
    if !params.iter().any(|p| matches!(p, ParamRole::Cache { .. })) {
        return Err(syn::Error::new_spanned(
            &fn_item.sig,
            "gear fn must take a `&mut <CacheType>` param",
        ));
    }

    Ok(GearSpec {
        name,
        fn_name: fn_item.sig.ident.clone(),
        is_async,
        params,
        out_ty,
        kind,
    })
}

// ── codegen ───────────────────────────────────────────────────────────────

fn expand(attr: GearsAttr, mut item_mod: ItemMod) -> syn::Result<TokenStream2> {
    let runtime = &attr.runtime;

    // Walk the module's items, collecting gear specs from `#[gear]`-marked
    // fns. The fns themselves are left in place (the `#[gear]` identity macro
    // strips its own attribute after this aggregator runs). Only an immutable
    // borrow is needed to read the signatures; it ends with this block,
    // before the mutable extend below.
    let mut specs: Vec<GearSpec> = Vec::new();
    {
        let content = item_mod.content.as_ref().ok_or_else(|| {
            syn::Error::new_spanned(&item_mod, "`#[gears]` needs an inline module body")
        })?;
        for item in &content.1 {
            if let Item::Fn(fn_item) = item
                && fn_item.attrs.iter().any(is_gear_attr)
            {
                specs.push(extract_gear(fn_item)?);
            }
        }
    }

    if specs.is_empty() {
        return Err(syn::Error::new_spanned(
            &item_mod,
            "`#[gears]` module must contain at least one `#[gear]` fn",
        ));
    }

    let id_variants = specs.iter().map(|g| {
        let v = &g.name;
        let fields = g.id_fields();
        if fields.len() == 1 {
            let (_, ty) = &fields[0];
            quote! { #v(#ty) }
        } else {
            let fs = fields.iter().map(|(f, t)| quote! { #f: #t });
            quote! { #v { #( #fs ),* } }
        }
    });

    let out_variants = specs.iter().map(|g| {
        let v = format_ident!("{}Out", g.name);
        let t = &g.out_ty;
        quote! { #[localizable(skip)] #v(#t) }
    });

    let cache_variants = specs.iter().map(|g| {
        let v = &g.name;
        let t = g.cache_ty().expect("validated above");
        quote! { #v(#t) }
    });

    // meta() arms.
    let meta_arms = specs.iter().map(|g| {
        let id_variant = &g.name;
        let fields = g.id_fields();
        let (id_pat, field_names): (TokenStream2, Vec<Ident>) = if fields.len() == 1 {
            let (n, _) = &fields[0];
            (quote! { #id_variant(#n) }, vec![n.clone()])
        } else {
            let names: Vec<Ident> = fields.iter().map(|(n, _)| n.clone()).collect();
            let pat = quote! { #id_variant { #( #names ),* } };
            (pat, names)
        };
        let group_hashes = field_names.iter().map(|f| {
            quote! { ::core::hash::Hash::hash(#f, &mut hasher); }
        });
        match &g.kind {
            KindSpec::Timer { period } => quote! {
                GearId::#id_pat => {
                    let mut hasher = ::std::collections::hash_map::DefaultHasher::new();
                    #( #group_hashes )*
                    ::dentrado::core::gear::GearMeta::Timer {
                        group: Group::Phantom(::core::hash::Hasher::finish(&mut hasher) as u32),
                        period: #period,
                    }
                }
            },
            KindSpec::Event => quote! {
                GearId::#id_pat => {
                    let mut hasher = ::std::collections::hash_map::DefaultHasher::new();
                    #( #group_hashes )*
                    ::dentrado::core::gear::GearMeta::Event {
                        msg_type: PHANTOM_MSG,
                        group: Group::Phantom(::core::hash::Hasher::finish(&mut hasher) as u32),
                    }
                }
            },
        }
    });

    // make_cache arms.
    let make_cache_arms = specs.iter().map(|g| {
        let id_variant = &g.name;
        let cv = &g.name;
        let ct = g.cache_ty().expect("validated above");
        let fields = g.id_fields();
        let pat: TokenStream2 = if fields.len() == 1 {
            quote! { GearId::#id_variant(..) }
        } else {
            quote! { GearId::#id_variant { .. } }
        };
        quote! { #pat => GearCache::#cv(#ct::default()) }
    });

    // run_step arms.
    let run_arms = specs.iter().map(|g| {
        let id_variant = &g.name;
        let fields = g.id_fields();
        let (id_pat, _): (TokenStream2, ()) = if fields.len() == 1 {
            let (n, _) = &fields[0];
            (quote! { GearId::#id_variant(#n) }, ())
        } else {
            let names: Vec<Ident> = fields.iter().map(|(n, _)| n.clone()).collect();
            (quote! { GearId::#id_variant { #( #names ),* } }, ())
        };
        let cv = &g.name;
        let cache_binding = format_ident!("{}_cache", g.name.to_string().to_lowercase());
        let out_v = format_ident!("{}Out", g.name);

        // Build the call argument list in the fn's declared param order.
        let call_args = g.params.iter().map(|p| match p {
            ParamRole::IdField { name, .. } => quote! { #name },
            ParamRole::Tick => quote! { tick },
            ParamRole::Ctx => quote! { ctx },
            ParamRole::Cache { .. } => quote! { #cache_binding },
        });
        let fn_name = &g.fn_name;
        let await_dot = g.is_async.then(|| quote! { .await });
        let run_call = quote! { #fn_name(#( #call_args ),*) #await_dot };

        let tick_bind = matches!(g.kind, KindSpec::Timer { .. }).then(|| {
            quote! { let tick = ::core::matches!(input, ::dentrado::core::gear::GearInput::Timer { tick: true }); }
        });

        quote! {
            (#id_pat, GearCache::#cv(#cache_binding)) => {
                #tick_bind
                GearOut::#out_v(#run_call)
            }
        }
    });

    // Typed dep accessors (one free fn per gear).
    let dep_fns = specs.iter().map(|g| {
        let fn_name = format_ident!("dep_{}", g.name.to_string().to_lowercase());
        let out_v = format_ident!("{}Out", g.name);
        let out_t = &g.out_ty;
        let id_variant = &g.name;
        let fields = g.id_fields();
        let param_decls = fields.iter().map(|(f, t)| quote! { #f: #t });
        let param_names: Vec<Ident> = fields.iter().map(|(f, _)| f.clone()).collect();
        let id_construct: TokenStream2 = if fields.len() == 1 {
            quote! { GearId::#id_variant(#( #param_names ),*) }
        } else {
            quote! { GearId::#id_variant { #( #param_names ),* } }
        };
        let msg = format!("{} gear produces GearOut::{}", g.name, out_v);
        quote! {
            pub(crate) async fn #fn_name<S: ::dentrado::core::storage::Storage<#runtime>>(
                ctx: &::dentrado::core::core_ctx::GearCtx<#runtime, S>,
                #( #param_decls ),*
            ) -> #out_t {
                match ctx.secondary_get(#id_construct).await {
                    GearOut::#out_v(out) => out,
                    _ => ::core::unreachable!(#msg),
                }
            }
        }
    });

    let generated = quote! {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, dentrado::types::Localizable)]
        pub(crate) enum GearId {
            #( #id_variants, )*
        }

        #[derive(Debug, Clone, dentrado::types::Localizable)]
        pub(crate) enum GearOut {
            #( #out_variants, )*
        }

        #[derive(Debug, Clone, PartialEq, Eq, Hash, dentrado::types::Localizable)]
        pub(crate) enum Group {
            Phantom(u32),
        }

        impl dentrado::types::GlobalHash for Group {
            fn global_hash(
                &self,
                _resolver: &dyn dentrado::types::GlobalResolver,
            ) -> ::core::result::Result<[u8; 32], dentrado::types::GroupRouteError> {
                let mut hash = [0u8; 32];
                let Group::Phantom(x) = *self;
                hash[..4].copy_from_slice(&x.to_le_bytes());
                ::core::result::Result::Ok(hash)
            }
        }

        const PHANTOM_MSG: ::dentrado::types::LocMsgTypeId = ::dentrado::types::LocMsgTypeId(0);

        #[derive(Debug, Clone)]
        pub(crate) enum GearCache {
            #( #cache_variants, )*
        }

        impl ::dentrado::core::storage::CacheSer for GearCache {
            fn page_roots(&self) -> &[::dentrado::core::storage::PageId] {
                &[]
            }
        }

        impl ::dentrado::core::gear::IsRuntime for #runtime {
            type GearId = GearId;
            type GearOut = GearOut;
            type Module = ();
            type Group = Group;
            type Body = ();
            type Data = ();
            type GearCache<W>
                = GearCache
            where
                W: ::core::fmt::Debug + Clone + 'static;

            fn meta(gear: &Self::GearId) -> ::dentrado::core::gear::GearMeta<Self> {
                match gear {
                    #( #meta_arms )*
                }
            }

            fn make_cache<Watermark: ::core::fmt::Debug + Clone + ::core::default::Default + 'static>(
                gear: &Self::GearId,
            ) -> Self::GearCache<Watermark> {
                match gear {
                    #( #make_cache_arms ),*
                }
            }

            async fn run_step<S: ::dentrado::core::storage::Storage<Self>>(
                ctx: &mut ::dentrado::core::core_ctx::GearCtx<Self, S>,
                input: ::dentrado::core::gear::GearInput,
                cache: &mut Self::GearCache<S::Watermark>,
            ) -> Self::GearOut {
                match (ctx.gear().clone(), cache) {
                    #( #run_arms )*
                    _ => ::core::unreachable!("gear id and cache variants always agree"),
                }
            }
        }

        #( #dep_fns )*
    };

    // Append the generated items to the module body, right after the user's
    // own items (fns, `use`s, …). The `#[gear]` identity macro will clean the
    // `#[gear]` attributes off the fns in a later expansion pass.
    let file: syn::File = syn::parse2(generated)?;
    if let Some(content) = item_mod.content.as_mut() {
        content.1.extend(file.items);
    }

    Ok(quote! { #item_mod })
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    fn parse_gear_args(src: &str) -> syn::Result<Punctuated<GearArg, Token![,]>> {
        Punctuated::<GearArg, Token![,]>::parse_terminated.parse_str(src)
    }

    #[test]
    fn gear_arg_timer_with_trailing_comma() {
        // `timer(period = EXPR,)` — trailing comma inside the `timer(...)` parens.
        // This used to fail because `Expr::parse` rejects an expression
        // immediately followed by a comma; the period is now captured as a
        // raw token stream with the trailing comma trimmed.
        let src = "timer(period = std::num::NonZero::new(u64::from(repo_meta.interval())).unwrap_or_else(|| std::num::NonZero::new(900).expect(\"900 != 0\")),)";
        let arg: GearArg =
            syn::parse_str(src).unwrap_or_else(|e| panic!("trailing-comma failed: {e}"));
        assert!(matches!(arg, GearArg::Timer { .. }));
    }

    #[test]
    fn gear_arg_timer_no_trailing_comma() {
        let src = "timer(period = std::num::NonZero::new(u64::from(repo_meta.interval())).unwrap_or_else(|| std::num::NonZero::new(900).expect(\"900 != 0\")))";
        let arg: GearArg =
            syn::parse_str(src).unwrap_or_else(|e| panic!("no-trailing-comma failed: {e}"));
        assert!(matches!(arg, GearArg::Timer { .. }));
    }

    #[test]
    fn gear_args_full() {
        // The full attribute payload: `timer(...), name = Repo,` (with the
        // trailing comma after `Repo` too).
        let src = "timer(period = std::num::NonZero::new(u64::from(repo_meta.interval())).unwrap_or_else(|| std::num::NonZero::new(900).expect(\"900 != 0\")),), name = Repo,";
        let args = parse_gear_args(src).unwrap_or_else(|e| panic!("full args failed: {e}"));
        assert_eq!(args.len(), 2);
    }
}
