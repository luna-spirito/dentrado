use dentrado::{
    core::db::{Db, DbConfig, Doorbell},
    types::NodeId,
};
use log::warn;
use std::{
    collections::HashMap,
    env::{VarError, var},
    iter,
    num::NonZero,
    sync::Arc,
    thread::available_parallelism,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::runtime::KolorinkoRT;
mod runtime;
mod safe_path;
mod wikidot_page;
pub mod wikidot_parser;

#[derive(Clone, Copy)]
pub struct Timestamp(pub u32);

pub fn impure_now() -> Timestamp {
    Timestamp(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as u32,
    )
}

pub fn main() -> anyhow::Result<()> {
    env_logger::init();

    let cores = match var("NUM_CORES").map(|x| x.parse()) {
        Ok(Ok(x)) => x,
        e => {
            if !matches!(e, Err(VarError::NotPresent)) {
                warn!("NUM_CORES ignored: couldn't parse as number")
            }
            NonZero::new(
                available_parallelism()
                    .map(|x| u32::try_from(x.get()).unwrap())
                    .unwrap_or(4),
            )
            .unwrap()
        }
    };
    let config = DbConfig::<KolorinkoRT> {
        num_cores: cores,
        node_id: NodeId(0),
        module: Arc::new(()),
        peers: HashMap::new(),
        doorbells: iter::repeat_with(Doorbell::new)
            .take(cores.get() as usize)
            .collect(),
    };

    Db::start(config)?;
    Ok(())
}
