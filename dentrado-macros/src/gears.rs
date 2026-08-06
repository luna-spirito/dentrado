//! The `#[gears]` module attribute + `#[gear]` fn marker: define a runtime's
//! gears as plain `async fn`s and let the macro generate the `GearId` /
//! `GearOut` / `GearCache` / `Group` enums, the `IsRuntime` impl, the
//! `GlobalHash` impl for `Group`, and the per-gear `GearQuery` builders (the
//! typed dependency layer).
//!
//! Each gear is one function — its signature *is* the gear spec:
//!
//! ```ignore
//! #[gears(runtime = KolorinkoRT)]
//! pub(crate) mod gears {
//!     #[gear(timer(period = secs(900)), local, name = Repo)]
//!     pub(crate) async fn repo<S: Storage<KolorinkoRT>>(
//!         repo_meta: RepoMeta,   // id field → GearId::Repo(RepoMeta)
//!         tick: bool,            // timer tick
//!         cache: &mut RepoCache, // → GearCache::Repo(RepoCache)
//!     ) -> Arc<RepoData> {}      // → GearOutLocal::RepoOut(Arc<RepoData>)
//! }
//! ```
//!
//! The `#[gear]` marker carries only the metadata the signature cannot express
//! (kind: timer/event/follow, the period expression, `local`, and the enum
//! *variant* base name). The id fields, cache type, and output type are all read
//! straight off the `fn`, so there is no DSL restating them.
//!
//! `local` marks a gear whose output is pinned to its owning core: it goes to
//! `GearOutLocal` (not `Send`/`Localizable`), gets no `GearQuery` builder, and
//! is readable only through a `follow` gear placed on the same core.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    FnArg, Ident, Item, ItemMod, LitStr, Pat, ReturnType, Token, Type, Visibility,
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

/// `runtime = <Type>` (required by `#[gears]`) and `file = "…"` (reads the
/// gear declarations from a shared file instead of the inline module body).
/// `#[gears_schema]` uses only `file = "…"`.
struct GearsAttr {
    runtime: Option<Type>,
    file: Option<LitStr>,
}

impl Parse for GearsAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut runtime = None;
        let mut file = None;
        while !input.is_empty() {
            let kw: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match kw.to_string().as_str() {
                "runtime" => runtime = Some(input.parse()?),
                "file" => file = Some(input.parse()?),
                _ => {
                    return Err(syn::Error::new(
                        kw.span(),
                        "expected `runtime =` or `file =`",
                    ));
                }
            }
            let _ = input.parse::<Token![,]>();
        }
        Ok(GearsAttr { runtime, file })
    }
}

enum GearArg {
    Timer {
        period: TokenStream2,
    },
    Event,
    Follow {
        target: TokenStream2,
    },
    Local,
    /// `wire_skip(a, b, …)` — id fields excluded from the wire schema (server
    /// config the client never supplies, e.g. `repo_meta`). Ignored by the
    /// runtime expansion; the schema expansion drops these fields.
    WireSkip(Vec<Ident>),
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
            "follow" => {
                let inner;
                syn::parenthesized!(inner in input);
                let t: Ident = inner.parse()?;
                if t != "target" {
                    return Err(syn::Error::new(t.span(), "expected `target =`"));
                }
                inner.parse::<Token![=]>()?;
                // Capture as a raw token stream (like `timer(period = …)`):
                // the target may be arbitrarily complex, and a trailing comma
                // would trip `Expr::parse`.
                let target: TokenStream2 = strip_trailing_comma(inner.parse()?);
                Ok(GearArg::Follow { target })
            }
            "local" => Ok(GearArg::Local),
            "wire_skip" => {
                let inner;
                syn::parenthesized!(inner in input);
                let names = Punctuated::<Ident, Token![,]>::parse_terminated(&inner)?;
                Ok(GearArg::WireSkip(names.into_iter().collect()))
            }
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
    Follow { target: TokenStream2 },
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
    /// The followed gear's output (follow gears only). Bound from
    /// [`GearInput::Follow`] and destructured to its typed inner value.
    /// Reclassified from a tentative [`ParamRole::IdField`] during the
    /// Follow-resolution pass (matched by the target gear's output type);
    /// only the binding name is needed thereafter.
    FollowOut { name: Ident },
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
    /// The fn's declared visibility — reused for the generated `GearQuery`
    /// builder so it matches the gear fn's exposure.
    vis: Visibility,
    is_async: bool,
    /// `#[gear(local)]`: the output is pinned to the owning core — it goes to
    /// `GearOutLocal` (never `Send`/`Localizable`) and is reachable only
    /// through the `follow` mechanism. No `GearQuery` builder is emitted.
    is_local: bool,
    params: Vec<ParamRole>,
    out_ty: Type,
    kind: KindSpec,
    /// Follow gears only: the target's `GearOut` variant (`RepoOut`), used to
    /// destructure the followed output in `run_step`. `None` for non-follow
    /// gears; set during the Follow-resolution pass for follow gears.
    follow_out_variant: Option<Ident>,
    /// Follow gears only: whether the followed gear is `#[gear(local)]`. When
    /// true, the followed output arrives in `GearInput::Follow` as
    /// `GearResult::Local(GearOutLocal::<Variant>(…))` instead of
    /// `GearResult::Ship(GearOut::<Variant>(…))`.
    follow_out_local: bool,
    /// Id-field names excluded from the wire schema (`#[gear(wire_skip(…))]`).
    /// The runtime expansion keeps them as real id fields; the schema expansion
    /// drops them.
    wire_skip: std::collections::HashSet<String>,
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
    let mut is_local = false;
    let mut wire_skip = std::collections::HashSet::new();
    for arg in args {
        match arg {
            GearArg::Timer { period } => kind = Some(KindSpec::Timer { period }),
            GearArg::Event => kind = Some(KindSpec::Event),
            GearArg::Follow { target } => kind = Some(KindSpec::Follow { target }),
            GearArg::Local => is_local = true,
            GearArg::WireSkip(v) => {
                wire_skip = v.into_iter().map(|i| i.to_string()).collect();
            }
            GearArg::Name(n) => name = Some(n),
        }
    }
    let name = name.ok_or_else(|| {
        syn::Error::new_spanned(gear_attr, "`#[gear(...)]` must set `name = <Variant>`")
    })?;
    let kind = kind.ok_or_else(|| {
        syn::Error::new_spanned(
            gear_attr,
            "`#[gear(...)]` must set `timer(...)`, `event`, or `follow(target = …)`",
        )
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
        KindSpec::Follow { .. } if params.iter().any(|p| matches!(p, ParamRole::Tick)) => {
            return Err(syn::Error::new_spanned(
                &fn_item.sig,
                "follow gear fn must not take a `tick` param",
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
        vis: fn_item.vis.clone(),
        is_async,
        is_local,
        params,
        out_ty,
        kind,
        follow_out_variant: None,
        follow_out_local: false,
        wire_skip,
    })
}

/// Extract the `GearId` variant name from a `follow(target = …)` payload. The
/// target must be a `GearId::<Variant>(…)` / `GearId::<Variant> { … }`
/// construction; we read the variant off the call/struct path so the
/// Follow-resolution pass can look up the target gear's output type.
fn follow_target_variant(target: &TokenStream2) -> syn::Result<Ident> {
    let expr: syn::Expr = syn::parse2(target.clone())?;
    let path = match &expr {
        syn::Expr::Call(c) => match c.func.as_ref() {
            syn::Expr::Path(p) => &p.path,
            _ => {
                return Err(syn::Error::new_spanned(
                    c,
                    "follow target must be a `GearId::<Variant>(…)` construction",
                ));
            }
        },
        syn::Expr::Struct(s) => &s.path,
        _ => {
            return Err(syn::Error::new_spanned(
                expr,
                "follow target must be a `GearId::<Variant>(…)` construction",
            ));
        }
    };
    path.segments
        .last()
        .map(|s| s.ident.clone())
        .ok_or_else(|| syn::Error::new_spanned(path, "follow target must name a `GearId` variant"))
}

/// Collect every identifier appearing in `ts`, recursing into groups so that
/// names nested in `(…)`, `{…}`, `[…]` (e.g. the field inside `GearId::Repo(repo)`)
/// are seen. Used to learn which id-field names a `follow(target = …)` expression
/// actually references.
fn collect_ident_strings(ts: &TokenStream2) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    collect_ident_strings_into(ts.clone(), &mut out);
    out
}

fn collect_ident_strings_into(ts: TokenStream2, out: &mut std::collections::HashSet<String>) {
    for tt in ts {
        match tt {
            proc_macro2::TokenTree::Ident(i) => {
                out.insert(i.to_string());
            }
            proc_macro2::TokenTree::Group(g) => {
                collect_ident_strings_into(g.stream(), out);
            }
            _ => {}
        }
    }
}

// ── source resolution (shared by runtime + schema expansions) ───────────

/// Load and parse a gear-declaration file (relative to `CARGO_MANIFEST_DIR`),
/// returning its items plus a rebuild-tracking token. The same file is read by
/// both `#[gears]` (runtime) and `#[gears_schema]` (wire schema), so a gear is
/// declared exactly once.
///
/// Rebuild tracking uses an emitted `include_str!` of the file's absolute path
/// rather than the nightly `proc_macro::tracked::path` API: the latter is not
/// implemented by the rust-analyzer proc-macro server and crashes its
/// expansion. The emitted const is read by both rustc (which rebuilds on
/// change) and rust-analyzer.
fn load_gear_file(rel: &LitStr) -> syn::Result<(Vec<Item>, TokenStream2)> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .map_err(|_| syn::Error::new(rel.span(), "`file = …` requires `CARGO_MANIFEST_DIR`"))?;
    let full = std::path::Path::new(&manifest).join(rel.value());
    if !full.exists() {
        return Err(syn::Error::new(
            rel.span(),
            format!("gear file not found: {}", full.display()),
        ));
    }
    let src = std::fs::read_to_string(&full)
        .map_err(|e| syn::Error::new(rel.span(), format!("read {}: {e}", full.display())))?;
    let items = syn::parse_file(&src)
        .map_err(|e| syn::Error::new(rel.span(), format!("parse {}: {e}", full.display())))?
        .items;
    let abs = full.to_string_lossy().into_owned();
    let track = quote! { const _: &str = include_str!(#abs); };
    Ok((items, track))
}

/// Extract gear specs from the `#[gear]`-marked fns in `items`, then resolve
/// every `follow` target (reclassifying the followed-output param so it is no
/// longer treated as a user-supplied id field). Shared by the runtime and
/// wire-schema expansions.
fn collect_specs(items: &[Item]) -> syn::Result<Vec<GearSpec>> {
    let mut specs: Vec<GearSpec> = Vec::new();
    for item in items {
        if let Item::Fn(fn_item) = item
            && fn_item.attrs.iter().any(is_gear_attr)
        {
            specs.push(extract_gear(fn_item)?);
        }
    }
    if specs.is_empty() {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "no `#[gear]` fn found",
        ));
    }

    // Follow resolution: each follow gear's `target` must name a known gear
    // variant; exactly one of its by-value params must then match the target's
    // output type and is reclassified as the followed-output binding. Also
    // rejects direct self-follow (which would infinitely recurse in `meta`).
    let target_info: std::collections::HashMap<String, (Type, Ident, bool)> = specs
        .iter()
        .map(|g| {
            (
                g.name.to_string(),
                (g.out_ty.clone(), format_ident!("{}Out", g.name), g.is_local),
            )
        })
        .collect();
    for g in &mut specs {
        let KindSpec::Follow { target } = &g.kind else {
            continue;
        };
        let variant = follow_target_variant(target)?;
        if variant == g.name {
            return Err(syn::Error::new_spanned(
                &variant,
                "a gear cannot follow itself",
            ));
        }
        let (out_ty, out_variant, target_local) =
            target_info.get(&variant.to_string()).ok_or_else(|| {
                syn::Error::new_spanned(
                    &variant,
                    format!("follow target `GearId::{variant}` is not a known gear"),
                )
            })?;
        // Compare types by their stringified token streams: neither `syn::Type`
        // nor `proc_macro2::TokenStream` has `PartialEq` without extra feature
        // flags, but two identical type spellings stringify identically.
        let out_ty_q = quote!(#out_ty).to_string();
        let matches: Vec<usize> = g
            .params
            .iter()
            .enumerate()
            .filter_map(|(i, p)| match p {
                ParamRole::IdField { ty, .. } if quote!(#ty).to_string() == out_ty_q => Some(i),
                _ => None,
            })
            .collect();
        let idx = match matches.as_slice() {
            [] => {
                return Err(syn::Error::new_spanned(
                    &g.fn_name,
                    "follow gear must take a param of the target gear's output type",
                ));
            }
            [single] => *single,
            _ => {
                return Err(syn::Error::new_spanned(
                    &g.fn_name,
                    "follow gear has multiple params matching the target output type \
                     — disambiguate the types",
                ));
            }
        };
        let ParamRole::IdField { name, .. } = &g.params[idx] else {
            unreachable!("matched an IdField above");
        };
        g.params[idx] = ParamRole::FollowOut { name: name.clone() };
        g.follow_out_variant = Some(out_variant.clone());
        g.follow_out_local = *target_local;
    }

    Ok(specs)
}

/// Rename each `#[gear]` fn to `<name>_step`, freeing the gear's name for the
/// generated `GearQuery` builder. The `#[gear]` attribute is left in place for
/// the identity macro to strip in its own expansion pass.
fn rename_gear_steps(items: &mut [Item]) {
    for item in items {
        if let Item::Fn(fn_item) = item
            && fn_item.attrs.iter().any(is_gear_attr)
        {
            fn_item.sig.ident = format_ident!("{}_step", fn_item.sig.ident);
        }
    }
}

/// Resolve gear specs from either an inline module body or a `file = …`, and
/// in the file case inject the (renamed) declared fns into the module so the
/// generated `run_step` can call them.
fn resolve_specs(
    attr: &GearsAttr,
    item_mod: &mut ItemMod,
) -> syn::Result<(Vec<GearSpec>, TokenStream2)> {
    Ok(match &attr.file {
        Some(rel) => {
            let (mut file_items, track) = load_gear_file(rel)?;
            let specs = collect_specs(&file_items)?;
            rename_gear_steps(&mut file_items);
            if let Some(content) = item_mod.content.as_mut() {
                content.1.extend(file_items);
            } else {
                item_mod.content = Some((syn::token::Brace::default(), file_items));
            }
            (specs, track)
        }
        None => {
            let content = item_mod.content.as_ref().ok_or_else(|| {
                syn::Error::new_spanned(
                    &*item_mod,
                    "`#[gears]` needs an inline module body or `file = …`",
                )
            })?;
            let specs = collect_specs(&content.1)?;
            if let Some(content) = item_mod.content.as_mut() {
                rename_gear_steps(&mut content.1);
            }
            (specs, quote! {})
        }
    })
}

// ── codegen ───────────────────────────────────────────────────────────────

fn expand(attr: GearsAttr, mut item_mod: ItemMod) -> syn::Result<TokenStream2> {
    let runtime = attr.runtime.as_ref().ok_or_else(|| {
        syn::Error::new(
            proc_macro2::Span::call_site(),
            "`#[gears]` requires `runtime = <Type>`",
        )
    })?;

    let (specs, track) = resolve_specs(&attr, &mut item_mod)?;

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

    let out_variants = specs.iter().filter(|g| !g.is_local).map(|g| {
        let v = format_ident!("{}Out", g.name);
        let t = &g.out_ty;
        quote! { #[localizable(skip)] #v(#t) }
    });

    // `GearOutLocal` variants: exactly the `#[gear(local)]` gears. This enum is
    // deliberately NOT `Localizable`/`Send` — that's the whole point of the
    // local-output family. `GearOut` (shippable) only ever carries the non-local
    // gears' variants, so a local gear has no `GearOut` variant and no
    // `GearQuery` builder: the typed `secondary_get` layer cannot reach it.
    let out_local_variants = specs.iter().filter(|g| g.is_local).map(|g| {
        let v = format_ident!("{}Out", g.name);
        let t = &g.out_ty;
        quote! { #v(#t) }
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
            KindSpec::Follow { target } => {
                // `meta` receives `&GearId`, so a plain match binds id fields by
                // reference — but the target expression constructs the target
                // `GearId` by *value* (e.g. `GearId::Repo(repo)`). Re-bind an
                // owned clone and match only the fields the target names (+`..`),
                // so the user never needs `repo.clone()` and unused fields don't
                // trip unused-variable warnings.
                let referenced: std::collections::HashSet<String> = collect_ident_strings(target);
                let bound: Vec<&Ident> = field_names
                    .iter()
                    .filter(|f| referenced.contains(&f.to_string()))
                    .collect();
                let outer_pat = if fields.len() == 1 {
                    quote! { #id_variant(..) }
                } else {
                    quote! { #id_variant { .. } }
                };
                let inner_pat = if fields.len() == 1 {
                    quote! { #id_variant(#( #bound ),*) }
                } else {
                    quote! { #id_variant { #( #bound ),*, .. } }
                };
                quote! {
                    GearId::#outer_pat => {
                        // Co-located with the target: the group *is* the target's
                        // own group (resolved via its `meta`), so this gear
                        // routes to the same core and can follow it locally. The
                        // id fields are not hashed here — `baked_group` is
                        // authoritative.
                        let __g = (*gear).clone();
                        let GearId::#inner_pat = __g else { ::core::unreachable!() };
                        let __target = #target;
                        ::dentrado::core::gear::GearMeta::Follow {
                            baked_group: ::dentrado::core::gear::GearMeta::group(
                                &<Self as ::dentrado::core::gear::IsRuntime>::meta(&__target),
                            )
                            .clone(),
                            gear: __target,
                        }
                    }
                }
            }
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
            ParamRole::FollowOut { name, .. } => quote! { #name },
            ParamRole::Ctx => quote! { ctx },
            ParamRole::Cache { .. } => quote! { #cache_binding },
        });
        let step_ident = format_ident!("{}_step", g.fn_name);
        let await_dot = g.is_async.then(|| quote! { .await });
        let run_call = quote! { #step_ident(#( #call_args ),*) #await_dot };

        let tick_bind = matches!(g.kind, KindSpec::Timer { .. }).then(|| {
            quote! { let tick = ::core::matches!(input, ::dentrado::core::gear::GearInput::Timer { tick: true }); }
        });

        // For follow gears, pull the followed gear's output out of `input` and
        // destructure it to the typed inner value, binding it to the param the
        // Follow-resolution pass reclassified. The followed value lives in
        // `GearResult::Local` iff the *target* is `#[gear(local)]`.
        let follow_bind = if let (KindSpec::Follow { .. }, Some(out_v)) =
            (&g.kind, &g.follow_out_variant)
        {
            let name = g
                .params
                .iter()
                .find_map(|p| match p {
                    ParamRole::FollowOut { name, .. } => Some(name.clone()),
                    _ => None,
                })
                .expect("resolved above");
            let followed_out = if g.follow_out_local {
                quote! { ::dentrado::core::gear::GearResult::Local(GearOutLocal::#out_v(__followed)) }
            } else {
                quote! { ::dentrado::core::gear::GearResult::Ship(GearOut::#out_v(__followed)) }
            };
            quote! {
                let #name = match input {
                    ::dentrado::core::gear::GearInput::Follow { out } => match out {
                        #followed_out => __followed,
                        _ => ::core::unreachable!(
                            "follow target produced an unexpected output"
                        ),
                    },
                    _ => ::core::unreachable!("follow gear received a non-Follow input"),
                };
            }
        } else {
            quote! {}
        };

        let result = if g.is_local {
            quote! { ::dentrado::core::gear::GearResult::Local(GearOutLocal::#out_v(#run_call)) }
        } else {
            quote! { ::dentrado::core::gear::GearResult::Ship(GearOut::#out_v(#run_call)) }
        };

        quote! {
            (#id_pat, GearCache::#cv(#cache_binding)) => {
                #tick_bind
                #follow_bind
                #result
            }
        }
    });

    // One `GearQuery` builder per **shippable** gear: `#vis fn <fn_name>(<id
    // fields>) -> GearQuery<R, Out>`. The async gear impl is renamed
    // `<fn_name>_step` (see the rename pass above), so the builder takes the
    // gear's own name. Local gears get no builder — `GearQuery::secondary_get`
    // is the shippable-only typed layer, so a local output has no query handle
    // by construction (it is read through `follow` gears instead). The
    // per-variant extraction lives in a nested fn so the module surface is
    // exactly one builder per gear.
    let builders = specs.iter().filter(|g| !g.is_local).map(|g| {
        let builder = &g.fn_name;
        let vis = &g.vis;
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
        let getter = format_ident!("__dentrado_get_{}", g.fn_name);
        let msg = format!("{} gear produces GearOut::{}", g.name, out_v);
        quote! {
            #vis fn #builder(#( #param_decls ),*)
                -> ::dentrado::core::gear::GearQuery<#runtime, #out_t>
            {
                fn #getter(
                    out: <#runtime as ::dentrado::core::gear::IsRuntime>::GearOut,
                ) -> #out_t {
                    match out {
                        GearOut::#out_v(__o) => __o,
                        _ => ::core::unreachable!(#msg),
                    }
                }
                ::dentrado::core::gear::GearQuery {
                    id: #id_construct,
                    getter: #getter,
                }
            }
        }
    });

    let generated = quote! {
        #track
        // The concrete `GearId` is an internal implementation detail of the
        // runtime: it is not re-exported (the `gears` module is private), so
        // callers can never name it — gear ids are constructed only through the
        // per-gear `GearQuery` builders below. It stays `pub(crate)`-declared
        // because `IsRuntime`'s associated type is reached through pub(crate)
        // APIs (`Core::subscribe_gear`, `GearResult`, …).
        #[derive(Debug, Clone, PartialEq, Eq, Hash, dentrado_types::Localizable)]
        pub(crate) enum GearId {
            #( #id_variants, )*
        }

        #[derive(Debug, Clone, dentrado_types::Localizable)]
        pub(crate) enum GearOut {
            #( #out_variants, )*
        }

        // Core-local outputs (never serialized, never sent across a thread):
        // exactly the `#[gear(local)]` gears. `GearOut` (shippable) has no
        // variant for them, so `GearResult::Local` is reachable only on-core.
        // Same `pub(crate)`-declared-but-unreachable status as `GearId`.
        #[derive(Debug, Clone)]
        pub(crate) enum GearOutLocal {
            #( #out_local_variants, )*
        }

        #[derive(Debug, Clone, PartialEq, Eq, Hash, dentrado_types::Localizable)]
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
            type GearOutLocal = GearOutLocal;
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
                input: ::dentrado::core::gear::GearInput<Self>,
                cache: &mut Self::GearCache<S::Watermark>,
            ) -> ::dentrado::core::gear::GearResult<Self> {
                match (ctx.gear().clone(), cache) {
                    #( #run_arms )*
                    _ => ::core::unreachable!("gear id and cache variants always agree"),
                }
            }
        }

        #( #builders )*
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

/// Emit a **wasm-safe, dentrado-free** wire schema — serde `GearId` / `GearOut`
/// / `GearQuery` — from the same gear-declaration file `#[gears]` reads, so a
/// gear is declared exactly once. `#[gear(local)]` gears (core-pinned, no
/// shippable output) and `#[gear(wire_skip(…))]` id fields are omitted: the
/// client never supplies server config like `repo_meta`.
pub(crate) fn gears_schema_impl(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr: GearsAttr = syn::parse_macro_input!(attr);
    let item: ItemMod = syn::parse_macro_input!(item);
    expand_schema(attr, item)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// Id fields of `g` with its `wire_skip` names removed. The followed-output
/// param is already reclassified out of `IdField` by [`collect_specs`], so it
/// is excluded here for free.
fn wire_fields(g: &GearSpec) -> Vec<(Ident, Type)> {
    g.id_fields()
        .into_iter()
        .filter(|(n, _)| !g.wire_skip.contains(&n.to_string()))
        .collect()
}

fn expand_schema(attr: GearsAttr, mut item_mod: ItemMod) -> syn::Result<TokenStream2> {
    let rel = attr.file.as_ref().ok_or_else(|| {
        syn::Error::new(
            proc_macro2::Span::call_site(),
            "`#[gears_schema]` requires `file = \"…\"`",
        )
    })?;
    let (file_items, track) = load_gear_file(rel)?;
    let specs = collect_specs(&file_items)?;
    let shippable: Vec<&GearSpec> = specs.iter().filter(|g| !g.is_local).collect();

    let id_variants = shippable.iter().map(|g| {
        let v = &g.name;
        let fields = wire_fields(g);
        if fields.len() == 1 {
            let (_, ty) = &fields[0];
            quote! { #v(#ty) }
        } else {
            let fs = fields.iter().map(|(f, t)| quote! { #f: #t });
            quote! { #v { #( #fs ),* } }
        }
    });

    let out_variants = shippable.iter().map(|g| {
        let v = format_ident!("{}Out", g.name);
        let t = &g.out_ty;
        quote! { #v(#t) }
    });

    let builders = shippable.iter().map(|g| {
        let builder = &g.fn_name;
        let out_v = format_ident!("{}Out", g.name);
        let out_t = &g.out_ty;
        let id_variant = &g.name;
        let fields = wire_fields(g);
        let param_decls = fields.iter().map(|(f, t)| quote! { #f: #t });
        let param_names: Vec<Ident> = fields.iter().map(|(f, _)| f.clone()).collect();
        let id_construct: TokenStream2 = if fields.len() == 1 {
            quote! { GearId::#id_variant(#( #param_names ),*) }
        } else {
            quote! { GearId::#id_variant { #( #param_names ),* } }
        };
        let getter = format_ident!("__getter_{}", g.fn_name);
        let msg = format!("{} gear produces GearOut::{}", g.name, out_v);
        quote! {
            pub fn #builder(#( #param_decls ),*) -> GearQuery<#out_t> {
                fn #getter(out: GearOut) -> #out_t {
                    match out {
                        GearOut::#out_v(__o) => __o,
                        _ => ::core::unreachable!(#msg),
                    }
                }
                GearQuery { id: #id_construct, getter: #getter }
            }
        }
    });

    let generated = quote! {
        #track
        #[derive(Debug, Clone, PartialEq, Eq, Hash, ::serde::Serialize, ::serde::Deserialize)]
        pub enum GearId {
            #( #id_variants, )*
        }

        #[derive(Debug, Clone, ::serde::Serialize, ::serde::Deserialize)]
        pub enum GearOut {
            #( #out_variants, )*
        }

        #[derive(Clone)]
        pub struct GearQuery<Out> {
            pub id: GearId,
            pub getter: fn(GearOut) -> Out,
        }

        #( #builders )*
    };

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
    fn gear_arg_local() {
        let src = "local, name = Repo,";
        let args = parse_gear_args(src).unwrap_or_else(|e| panic!("local failed: {e}"));
        assert_eq!(args.len(), 2);
        assert!(matches!(args[0], GearArg::Local));
        assert!(matches!(args[1], GearArg::Name(_)));
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
