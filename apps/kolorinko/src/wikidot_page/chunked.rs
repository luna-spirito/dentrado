//! Chunked blobs over the flat gear view.
//!
//! A [`GearView`] value must fit one leaf record (`8 + key + value <=
//! 4088`), and a whole-corpus image does not. This module stores a blob as
//! a manifest record (`key`: the block count, little-endian `u32`) plus
//! numbered, zstd-compressed block records (`key/b/<8 hex digits>`). It is
//! a kolorinko-side layer over the base view API — dentrado itself never
//! chunks.
//!
//! Reads are the view's old world, so [`load`] sees the last committed
//! image; [`store`] stages a fresh block set and deletes whatever the old
//! one left behind.

use dentrado::core::storage::GearView;

/// Raw bytes per block, before zstd. The key is short (`repo/b/00000000`)
/// and zstd's framing is tens of bytes, so this stays well under the
/// `4088 - 8 - key` ceiling.
const BLOCK: usize = 3800;

fn block_prefix(key: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(key.len() + 3);
    k.extend_from_slice(key);
    k.extend_from_slice(b"/b/");
    k
}

fn block_key(key: &[u8], index: usize) -> Vec<u8> {
    let mut k = block_prefix(key);
    k.extend_from_slice(format!("{index:08x}").as_bytes());
    k
}

/// Stage `bytes` as a fresh chunked image under `key`.
pub(super) async fn store<V: GearView>(view: &mut V, key: &[u8], bytes: &[u8]) {
    // The previous image may have had a different block count; drop it all.
    for (old, _) in view.scan(&block_prefix(key)).await {
        view.del(&old);
    }
    let blocks: Vec<Vec<u8>> = bytes
        .chunks(BLOCK)
        .map(|chunk| zstd::stream::encode_all(chunk, 0).expect("zstd encode"))
        .collect();
    let count = u32::try_from(blocks.len()).expect("block count fits u32");
    view.put(key, &count.to_le_bytes());
    for (index, block) in blocks.iter().enumerate() {
        view.put(&block_key(key, index), block);
    }
}

/// Load and decompress the image under `key`. `None` when the manifest or
/// any block is missing or unreadable — the caller cold-starts.
pub(super) async fn load<V: GearView>(view: &mut V, key: &[u8]) -> Option<Vec<u8>> {
    let count = view.get(key).await?;
    let count = u32::from_le_bytes(count.as_slice().try_into().ok()?);
    let mut out = Vec::new();
    for index in 0..count as usize {
        let block = view.get(&block_key(key, index)).await?;
        out.extend_from_slice(&zstd::stream::decode_all(&block[..]).ok()?);
    }
    Some(out)
}
