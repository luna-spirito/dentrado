//! Startup: open the page store, replay the journal tail, and materialize the
//! sequencer.
//!
//! Replay stops at the image's applied cursor, so a warm image visits no
//! records. A stale image (another log or runtime) is wiped and refilled from
//! the journal; the result is always a cache at the journal's head.

use std::{marker::PhantomData, path::Path, rc::Rc, time::Instant};

use dentrado_cache::{Storage, disk::IsFilesystem};
use dentrado_journal::{Cursor, Journal};

use super::{
    delta::Sequencer,
    live::JournalState,
    replay,
    tables::{self, MetaState, Tables},
};
use crate::core::{
    api::RuntimeTypes,
    storage::{journal::schema, store::Store},
};

/// The materialized cache before the disk is erased into [`super::live::Disk`].
pub(super) struct Materialized<R: RuntimeTypes, D: IsFilesystem> {
    pub(super) cache: Rc<Storage<D>>,
    pub(super) tables: Tables,
    pub(super) seq: Rc<Sequencer>,
    pub(super) journal: Option<JournalState>,
    pub(super) fingerprint: [u8; 32],
    pub(super) replayed: usize,
    pub(super) _r: PhantomData<fn() -> R>,
}

/// Open the page store and bring it to the journal's head, replaying the tail
/// past the image's applied cursor.
pub(super) async fn materialize<R: RuntimeTypes, D: IsFilesystem + Default + Clone>(
    store: &Store<D>,
    journal_dir: Option<&Path>,
) -> Result<Materialized<R, D>, String> {
    let fingerprint = tables::fingerprint::<R>();
    let journal = match journal_dir {
        Some(dir) => Some(open_journal::<R>(dir).await?),
        None => None,
    };

    store.configure(tables::layout::<R>());
    let mut cache = store.get().await;
    let mut tables = Tables::resolve::<R, _>(&cache);
    let identity = journal.as_ref().map_or([0; 16], |j| j.identity);
    let head = journal.as_ref().map_or(Cursor::START, |j| j.log.head());
    let mut meta = tables::read_meta(&cache, &tables, identity, fingerprint).await;
    if meta == MetaState::Stale || meta.start() > head {
        if meta == MetaState::Stale {
            log::warn!("cache belongs to another log or runtime; refilling from the journal");
        } else {
            log::warn!("cache is applied past the journal head; refilling from the journal");
        }
        drop(cache);
        store.reset().await;
        cache = store.get().await;
        tables = Tables::resolve::<R, _>(&cache);
        meta = MetaState::Fresh;
    }
    let valid = matches!(meta, MetaState::Valid(_));
    let from = meta.start();
    let next = match meta {
        MetaState::Valid(meta) => meta.next(),
        MetaState::Fresh | MetaState::Stale => [0; 4],
    };

    // Only the tail past the image's cursor is visited; a warm image visits
    // none.
    let replay_started = Instant::now();
    let seq = Rc::new(Sequencer::new(next));
    let replayed = match &journal {
        Some(journal) => {
            replay::replay::<R, D>(&journal.log, &journal.schemas, from, &cache, &tables, &seq)
                .await
                .map_err(|e| format!("replay: {e}"))?
        }
        None => 0,
    };
    let elapsed = replay_started.elapsed();

    if !valid || from != head {
        let mut session = cache.session();
        tables::write_meta(
            &mut session,
            &tables,
            identity,
            fingerprint,
            head,
            seq.next_values(),
        )
        .map_err(|e| format!("stage cache meta: {e:?}"))?;
        session
            .commit()
            .map_err(|e| format!("commit cache meta: {e:?}"))?;
        if journal.is_some() {
            cache
                .flush()
                .await
                .map_err(|e| format!("flush the materialized cache: {e:?}"))?;
        }
    }

    log::info!("storage: head {head}, {replayed} records replayed from {from} in {elapsed:?}");

    Ok(Materialized {
        cache,
        tables,
        seq,
        journal,
        fingerprint,
        replayed,
        _r: PhantomData,
    })
}

/// Open the log and intern this build's record schemas.
async fn open_journal<R: RuntimeTypes>(dir: &Path) -> Result<JournalState, String> {
    let mut log = Journal::open(dir)
        .await
        .map_err(|e| format!("open {}: {e}", dir.display()))?;
    if log.truncated() > 0 {
        log::warn!(
            "journal {}: repaired {} bytes of torn tail",
            dir.display(),
            log.truncated()
        );
    }
    let identity = log
        .identity()
        .await
        .map_err(|e| format!("identity of {}: {e}", dir.display()))?;
    let schemas = schema::Schemas::declare::<R>(&mut log)
        .await
        .map_err(|e| format!("declare schemas in {}: {e}", dir.display()))?;
    Ok(JournalState {
        log,
        schemas,
        identity,
    })
}
