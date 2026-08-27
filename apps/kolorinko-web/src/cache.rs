//! Persistent gear-output cache over IndexedDB — stale-while-revalidate for
//! gear queries.
//!
//! One object store keyed by the wire `GearId`'s JSON, values the wire
//! `GearOut` JSON plus the server's content hash — the very bytes and hash a
//! push carries, replayed verbatim. [`get`] is the *stale* half: a fresh
//! subscription renders the cached entry before its request goes out and
//! echoes the cached hash in `Subscribe`, so the server pushes only what
//! changed since the cached content (unchanged content crosses the wire
//! zero times, exactly like the hash echo on reconnects). [`put`] is the
//! *revalidate* half: every push refreshes the entry, and [`seed_ssr`] primes
//! it from the SSR state a hydrated page already rendered from.
//!
//! The database is one memoized open promise: a browser where IndexedDB is
//! unusable (some private modes) fails the open once and the session runs
//! cacheless — every operation is an ignored error away from a no-op.
//!
//! No eviction: entries are one rendered page each, the store is
//! origin-scoped, and IndexedDB's quota is the browser's to manage. A future
//! size limit would live here and nowhere else.

use std::cell::OnceCell;

use kolorinko_rt::SsrState;
use kolorinko_rt::wire::{GearId, GearOut};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{IdbDatabase, IdbRequest, IdbTransactionMode};

const DB_NAME: &str = "kolorinko-gears";
const DB_VERSION: u32 = 1;
const STORE: &str = "gears";

/// One cached entry, stored as a JSON string: a wire output plus the content
/// hash the server computed over it (the hash `Subscribe` echoes).
#[derive(serde::Serialize, serde::Deserialize)]
struct Entry {
    out: GearOut,
    hash: String,
}

thread_local! {
    /// The memoized open-database promise — one connection per page, shared
    /// by every operation. A failed open stays memoized too: the session
    /// runs cacheless rather than retrying a broken store on every push.
    static DB: OnceCell<js_sys::Promise> = const { OnceCell::new() };
}

/// The shared connection: the memoized open promise, settled. `None` when
/// IndexedDB is unusable or the open failed — the caller's plain flow.
async fn db() -> Option<IdbDatabase> {
    let promise = DB.with(|slot| slot.get_or_init(open).clone());
    JsFuture::from(promise).await.ok()?.dyn_into().ok()
}

/// Open the database, creating the store on the first-ever open (the store's
/// shape never changes at this version; a future bump would create or
/// migrate it here). Settles to the connection — `upgradeneeded` always
/// precedes `success`, so the store exists before anyone touches it — or
/// rejects once.
fn open() -> js_sys::Promise {
    js_sys::Promise::new(&mut |resolve, reject| {
        let Some(factory) = (|| web_sys::window()?.indexed_db().ok().flatten())() else {
            let _ = reject.call1(&JsValue::UNDEFINED, &JsValue::from_str("no IndexedDB"));
            return;
        };
        let req = match factory.open_with_u32(DB_NAME, DB_VERSION) {
            Ok(r) => r,
            Err(e) => {
                let _ = reject.call1(&JsValue::UNDEFINED, &e);
                return;
            }
        };
        let creating = req.clone();
        let upgrade = Closure::once_into_js(move |_: JsValue| {
            if let Ok(v) = creating.result()
                && let Ok(db) = v.dyn_into::<IdbDatabase>()
            {
                let _ = db.create_object_store(STORE);
            }
        });
        req.set_onupgradeneeded(Some(upgrade.unchecked_ref()));
        let settled = req.clone();
        let ok = Closure::once_into_js(move |_: JsValue| {
            // The upgrade handler fired on the way here or never will;
            // dropping it now also drops the never-fired closure.
            settled.set_onupgradeneeded(None);
            let _ = resolve.call1(&JsValue::UNDEFINED, &settled.result().unwrap_or_default());
        });
        req.set_onsuccess(Some(ok.unchecked_ref()));
        let bad = Closure::once_into_js(move |e: JsValue| {
            let _ = reject.call1(&JsValue::UNDEFINED, &e);
        });
        req.set_onerror(Some(bad.unchecked_ref()));
    })
}

/// One IndexedDB request as a promise of its `result`, rejecting with the
/// error event — callers treat any failure as a miss / lost write.
fn request(req: IdbRequest) -> js_sys::Promise {
    js_sys::Promise::new(&mut |resolve, reject| {
        let settled = req.clone();
        let ok = Closure::once_into_js(move |_: JsValue| {
            let _ = resolve.call1(&JsValue::UNDEFINED, &settled.result().unwrap_or_default());
        });
        let bad = Closure::once_into_js(move |e: JsValue| {
            let _ = reject.call1(&JsValue::UNDEFINED, &e);
        });
        req.set_onsuccess(Some(ok.unchecked_ref()));
        req.set_onerror(Some(bad.unchecked_ref()));
    })
}

/// A gear's cached output and content hash, if the cache holds one. Any
/// failure — no IndexedDB, a broken store, an undecodable entry, a plain
/// miss — is just `None`.
pub(crate) async fn get(id: &GearId) -> Option<(GearOut, String)> {
    let db = db().await?;
    let tx = db
        .transaction_with_str_and_mode(STORE, IdbTransactionMode::Readonly)
        .ok()?;
    let store = tx.object_store(STORE).ok()?;
    let key = serde_json::to_string(id).ok()?;
    let hit = JsFuture::from(request(store.get(&JsValue::from_str(&key)).ok()?))
        .await
        .ok()?;
    let Entry { out, hash } = serde_json::from_str(&hit.as_string()?).ok()?;
    Some((out, hash))
}

/// Remember a gear output under its id. Fire-and-forget: the write rides its
/// own transaction, and a lost write only means the next push rewrites the
/// entry.
pub(crate) fn put(id: &GearId, out: &GearOut, hash: &str) {
    let id = id.clone();
    let out = out.clone();
    let hash = hash.to_owned();
    spawn_local(async move {
        let Some(db) = db().await else {
            return;
        };
        let Ok(tx) = db.transaction_with_str_and_mode(STORE, IdbTransactionMode::Readwrite) else {
            return;
        };
        let Ok(store) = tx.object_store(STORE) else {
            return;
        };
        let (Ok(key), Ok(value)) = (
            serde_json::to_string(&id),
            serde_json::to_string(&Entry { out, hash }),
        ) else {
            return;
        };
        let Ok(req) = store.put_with_key(&JsValue::from_str(&value), &JsValue::from_str(&key))
        else {
            return;
        };
        let _ = JsFuture::from(request(req)).await;
    });
}

/// Prime the cache from the SSR state a hydrated page already rendered: the
/// page and its shell, under the very ids their subscriptions key — so the
/// first navigation back to either is served from the cache, not the
/// network.
pub(crate) fn seed_ssr(state: &SsrState) {
    put(
        &GearId::ArticleLatest {
            space: state.space,
            local: state.local,
        },
        &GearOut::ArticleLatestOut(state.page.clone()),
        &state.page_hash,
    );
    put(
        &GearId::Shell(state.space),
        &GearOut::ShellOut(state.shell.clone()),
        &state.shell_hash,
    );
}
