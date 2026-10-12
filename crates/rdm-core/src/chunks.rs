//! Chunk map: the ordered list of file segments being downloaded.
//!
//! Each chunk is a contiguous byte range with its own temp file. New connections
//! are created by splitting the chunk with the most *predicted* remaining work
//! in half, so slow connections get help first and every socket stays busy.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Never split a chunk whose remaining work is below this many bytes.
pub const MIN_SPLIT_REMAINING: u64 = 23_363;
/// Bytes each chunk overlaps into the next one, used for the corruption check.
pub const OVERLAP: u64 = 64;
/// Seconds of look-ahead used when predicting a connection's remaining work.
pub const LOOKAHEAD_SECS: f64 = 1.0;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    /// Stable id; also the temp-file suffix (`name`, `name1`, `name2`, ...).
    pub id: u32,
    /// First byte of this chunk in the final file.
    pub start: u64,
    /// One past the last byte this chunk must fetch (includes the overlap tail).
    pub end: u64,
    /// Bytes already written to this chunk's temp file.
    pub done: u64,
}

impl Chunk {
    pub fn pos(&self) -> u64 {
        self.start + self.done
    }
    pub fn remaining(&self) -> u64 {
        self.end.saturating_sub(self.pos())
    }
    pub fn is_complete(&self) -> bool {
        self.pos() >= self.end
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChunkMap {
    pub total: u64,
    pub chunks: Vec<Chunk>,
    pub next_id: u32,
}

impl ChunkMap {
    /// A fresh map with a single chunk covering the whole file.
    pub fn new(total: u64) -> Self {
        Self { total, chunks: vec![Chunk { id: 0, start: 0, end: total, done: 0 }], next_id: 1 }
    }

    pub fn downloaded(&self) -> u64 {
        // Count each byte once: a chunk's useful bytes stop at the next chunk's start.
        let mut sum = 0;
        for (i, c) in self.chunks.iter().enumerate() {
            let limit = self.chunks.get(i + 1).map_or(c.end, |n| n.start.min(c.end));
            sum += c.pos().min(limit).saturating_sub(c.start);
        }
        sum
    }

    pub fn is_complete(&self) -> bool {
        self.chunks.iter().all(Chunk::is_complete)
    }

    pub fn get(&self, id: u32) -> Option<&Chunk> {
        self.chunks.iter().find(|c| c.id == id)
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut Chunk> {
        self.chunks.iter_mut().find(|c| c.id == id)
    }

    /// Predicted bytes still to fetch after `LOOKAHEAD_SECS` at the chunk's
    /// current speed. Unowned chunks use raw remaining.
    fn predicted_remaining(c: &Chunk, speed: Option<u64>) -> u64 {
        let ahead = speed.map_or(0, |s| (s as f64 * LOOKAHEAD_SECS) as u64);
        c.remaining().saturating_sub(ahead)
    }

    /// The chunk with the most predicted remaining work, and that amount.
    pub fn find_max_free(&self, speeds: &HashMap<u32, u64>) -> Option<(u32, u64)> {
        self.chunks
            .iter()
            .filter(|c| !c.is_complete())
            .map(|c| (c.id, Self::predicted_remaining(c, speeds.get(&c.id).copied())))
            .max_by_key(|&(_, r)| r)
    }

    /// Whether opening another connection is worthwhile:
    /// the best candidate must hold at least twice the minimum split size.
    pub fn need_new_connection(&self, speeds: &HashMap<u32, u64>) -> bool {
        self.find_max_free(speeds).is_some_and(|(_, r)| r >= 2 * MIN_SPLIT_REMAINING + 3)
    }

    /// Split chunk `id` in half: the new chunk starts at `end - remaining/2` and
    /// the old chunk is shortened to stop `OVERLAP` bytes past the new start.
    /// Returns the new chunk's id, or `None` when the chunk is too small.
    pub fn split(&mut self, id: u32) -> Option<u32> {
        let idx = self.chunks.iter().position(|c| c.id == id)?;
        let c = &self.chunks[idx];
        // Split the chunk's own bytes, not its overlap tail into the next chunk.
        let logical_end = self.chunks.get(idx + 1).map_or(c.end, |n| n.start.min(c.end));
        let remaining = logical_end.saturating_sub(c.pos());
        if remaining < MIN_SPLIT_REMAINING {
            return None;
        }
        let new_start = logical_end - remaining / 2;
        let new_end = c.end;
        let new_id = self.next_id;
        self.next_id += 1;
        self.chunks[idx].end = (new_start + OVERLAP).min(self.total);
        self.chunks.insert(idx + 1, Chunk { id: new_id, start: new_start, end: new_end, done: 0 });
        Some(new_id)
    }

    /// Absorb the chunk after `id` into `id` (its connection never started).
    /// Returns the absorbed chunk's id.
    pub fn merge_next(&mut self, id: u32) -> Option<u32> {
        let idx = self.chunks.iter().position(|c| c.id == id)?;
        let next = self.chunks.get(idx + 1)?.clone();
        if next.done > 0 {
            return None;
        }
        self.chunks[idx].end = next.end;
        self.chunks.remove(idx + 1);
        Some(next.id)
    }

    /// Give chunk `id` (no data yet) back to its left neighbour.
    pub fn merge_into_prev(&mut self, id: u32) -> Option<u32> {
        let idx = self.chunks.iter().position(|c| c.id == id)?;
        if idx == 0 || self.chunks[idx].done > 0 {
            return None;
        }
        let end = self.chunks[idx].end;
        let prev = self.chunks[idx - 1].id;
        self.chunks[idx - 1].end = end;
        self.chunks.remove(idx);
        Some(prev)
    }

    /// Pick the best chunk and split it for a new connection, if worthwhile.
    pub fn split_for_new_connection(&mut self, speeds: &HashMap<u32, u64>) -> Option<u32> {
        if !self.need_new_connection(speeds) {
            return None;
        }
        let (id, _) = self.find_max_free(speeds)?;
        self.split(id)
    }

    /// Chunks that are not complete and have no worker; used on resume.
    pub fn incomplete_ids(&self) -> Vec<u32> {
        self.chunks.iter().filter(|c| !c.is_complete()).map(|c| c.id).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_halves_remaining_and_overlaps() {
        let mut m = ChunkMap::new(1_000_000);
        let n = m.split(0).unwrap();
        let (a, b) = (m.get(0).unwrap().clone(), m.get(n).unwrap().clone());
        assert_eq!(b.start, 500_000);
        assert_eq!(b.end, 1_000_000);
        assert_eq!(a.end, 500_000 + OVERLAP);
    }

    #[test]
    fn repeated_halving_gives_quarters() {
        let mut m = ChunkMap::new(1_000_000);
        let none = HashMap::new();
        for _ in 0..3 {
            m.split_for_new_connection(&none).unwrap();
        }
        let starts: Vec<u64> = m.chunks.iter().map(|c| c.start).collect();
        assert_eq!(starts, vec![0, 250_000, 500_000, 750_000]);
    }

    #[test]
    fn slow_chunk_is_split_first() {
        let mut m = ChunkMap::new(1_000_000);
        let b = m.split(0).unwrap();
        let mut speeds = HashMap::new();
        speeds.insert(0, 400_000);
        speeds.insert(b, 10_000);
        assert_eq!(m.find_max_free(&speeds).unwrap().0, b);
    }

    #[test]
    fn small_chunks_are_not_split() {
        let mut m = ChunkMap::new(40_000);
        assert!(!m.need_new_connection(&HashMap::new()));
        assert!(m.split(0).is_some());
        let mut tiny = ChunkMap::new(20_000);
        assert!(tiny.split(0).is_none());
    }

    #[test]
    fn merge_next_takes_unstarted_chunk() {
        let mut m = ChunkMap::new(1_000_000);
        let b = m.split(0).unwrap();
        assert_eq!(m.merge_next(0), Some(b));
        assert_eq!(m.chunks.len(), 1);
        assert_eq!(m.chunks[0].end, 1_000_000);
    }

    #[test]
    fn merge_into_prev_returns_work() {
        let mut m = ChunkMap::new(1_000_000);
        let b = m.split(0).unwrap();
        assert_eq!(m.merge_into_prev(b), Some(0));
        assert_eq!(m.chunks[0].end, 1_000_000);
    }

    #[test]
    fn downloaded_counts_overlap_once() {
        let mut m = ChunkMap::new(1_000);
        m.total = 100_000;
        m.chunks[0].end = 100_000;
        let n = m.split(0).unwrap();
        let a_end = m.get(0).unwrap().end;
        m.get_mut(0).unwrap().done = a_end;
        let nb = m.get(n).unwrap().end - m.get(n).unwrap().start;
        m.get_mut(n).unwrap().done = nb;
        assert!(m.is_complete());
        assert_eq!(m.downloaded(), 100_000);
    }
}
