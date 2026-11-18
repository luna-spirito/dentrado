//! One cache row on its way in, and the sink the fold writes through.
//!
//! Live applies stage rows in the delta; replay writes them straight to the
//! cache. Both go through [`Sink`], so the fold that interprets a journal
//! record exists once.

use dentrado_cache::TableId;

/// One committed row: cache table, key, inline value.
pub(crate) struct Row {
    pub table: TableId,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

impl Row {
    pub(crate) fn new(table: TableId, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Self {
        Self {
            table,
            key: key.into(),
            value: value.into(),
        }
    }
}

/// Where an interpreted record's rows go.
pub(crate) trait Sink {
    /// Read a row through the sink: the delta first when live, the cache when
    /// replaying.
    async fn get(&mut self, table: TableId, key: &[u8]) -> Option<Vec<u8>>;

    /// Stage or write a record's rows.
    fn put(&mut self, rows: Vec<Row>);
}
