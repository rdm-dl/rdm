//! On-disk state for an in-progress download: one temp file per chunk plus a small
//! JSON state file, and final assembly of the chunk files into the target file.

use crate::chunks::{Chunk, ChunkMap};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const STATE_FILE: &str = "state.json";

/// Everything needed to resume a download after a restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedState {
    pub url: String,
    pub file_name: String,
    pub total: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    pub resumable: bool,
    pub single_connection: bool,
    pub map: ChunkMap,
}

/// The per-download temp folder.
#[derive(Debug, Clone)]
pub struct TempDir {
    pub path: PathBuf,
}

impl TempDir {
    pub fn new(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub fn chunk_path(&self, id: u32) -> PathBuf {
        self.path.join(format!("{id}.part"))
    }

    /// Open a chunk's temp file positioned at `offset` (truncating anything beyond it).
    pub fn open_chunk(&self, id: u32, offset: u64) -> io::Result<File> {
        let mut f = OpenOptions::new().create(true).read(true).write(true).truncate(false).open(self.chunk_path(id))?;
        f.set_len(offset)?;
        f.seek(SeekFrom::Start(offset))?;
        Ok(f)
    }

    pub fn chunk_len(&self, id: u32) -> u64 {
        fs::metadata(self.chunk_path(id)).map(|m| m.len()).unwrap_or(0)
    }

    /// Read up to `len` bytes of chunk `id` starting at `offset` within that chunk.
    pub fn read_chunk(&self, id: u32, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut f = File::open(self.chunk_path(id))?;
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0; len];
        let mut read = 0;
        while read < len {
            let n = f.read(&mut buf[read..])?;
            if n == 0 {
                break;
            }
            read += n;
        }
        buf.truncate(read);
        Ok(buf)
    }

    /// Write the state file atomically (temp file + rename).
    pub fn save(&self, state: &SavedState) -> io::Result<()> {
        let tmp = self.path.join("state.json.tmp");
        let data = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&data)?;
            f.sync_all()?;
        }
        fs::rename(tmp, self.path.join(STATE_FILE))
    }

    /// Load the saved state; each chunk's progress is taken from its temp file size.
    pub fn load(&self) -> io::Result<Option<SavedState>> {
        let p = self.path.join(STATE_FILE);
        if !p.exists() {
            return Ok(None);
        }
        let mut state: SavedState = serde_json::from_slice(&fs::read(p)?).map_err(io::Error::other)?;
        for c in &mut state.map.chunks {
            let span = c.end.saturating_sub(c.start);
            c.done = self.chunk_len(c.id).min(span);
        }
        Ok(Some(state))
    }

    /// Delete all chunk files and start from nothing.
    pub fn reset(&self) -> io::Result<()> {
        for e in fs::read_dir(&self.path)? {
            let p = e?.path();
            if p.extension().is_some_and(|x| x == "part") || p.file_name().is_some_and(|n| n == STATE_FILE) {
                fs::remove_file(p)?;
            }
        }
        Ok(())
    }

    pub fn remove(self) -> io::Result<()> {
        fs::remove_dir_all(&self.path)
    }
}

/// Useful length of chunk `i`: up to the next chunk's start (excluding the overlap tail).
fn useful_len(map: &ChunkMap, i: usize, total: u64) -> u64 {
    let c: &Chunk = &map.chunks[i];
    let end = map.chunks.get(i + 1).map_or(total, |n| n.start);
    end.saturating_sub(c.start)
}

/// Join the chunk files into `dest`: the first chunk file is renamed into place and
/// the others are appended in `block`-sized pieces. Returns the final path.
pub fn assemble(tmp: &TempDir, map: &ChunkMap, total: u64, dest: &Path, block: usize) -> io::Result<PathBuf> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let first = tmp.chunk_path(map.chunks[0].id);
    if fs::rename(&first, dest).is_err() {
        // Different volume: copy instead.
        fs::copy(&first, dest)?;
        fs::remove_file(&first)?;
    }
    let mut out = OpenOptions::new().write(true).open(dest)?;
    out.set_len(useful_len(map, 0, total))?;
    out.seek(SeekFrom::End(0))?;
    let mut buf = vec![0u8; block.max(64 * 1024)];
    for i in 1..map.chunks.len() {
        let want = useful_len(map, i, total);
        let mut f = File::open(tmp.chunk_path(map.chunks[i].id))?;
        let mut left = want;
        while left > 0 {
            let cap = left.min(buf.len() as u64) as usize;
            let n = f.read(&mut buf[..cap])?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("chunk {} is short", map.chunks[i].id)));
            }
            out.write_all(&buf[..n])?;
            left -= n as u64;
        }
    }
    out.sync_all()?;
    let len = out.metadata()?.len();
    if len != total {
        return Err(io::Error::other(format!("assembled size {len} != expected {total}")));
    }
    Ok(dest.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemble_skips_overlap_tails() {
        let base = std::env::temp_dir().join(format!("rdm_storage_{}", std::process::id()));
        let tmp = TempDir::new(base.join("t")).unwrap();
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let mut map = ChunkMap::new(1000);
        map.chunks[0].end = 1000;
        let b = map.split(0);
        // Splitting a 1000-byte chunk is refused (too small); build a map by hand instead.
        assert!(b.is_none());
        map.chunks = vec![
            Chunk { id: 0, start: 0, end: 564, done: 564 },
            Chunk { id: 1, start: 500, end: 1000, done: 500 },
        ];
        std::fs::write(tmp.chunk_path(0), &data[0..564]).unwrap();
        std::fs::write(tmp.chunk_path(1), &data[500..1000]).unwrap();
        let dest = base.join("out.bin");
        assemble(&tmp, &map, 1000, &dest, 64).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn save_and_load_uses_file_sizes() {
        let base = std::env::temp_dir().join(format!("rdm_state_{}", std::process::id()));
        let tmp = TempDir::new(&base).unwrap();
        let map = ChunkMap::new(10_000);
        std::fs::write(tmp.chunk_path(0), vec![0u8; 1234]).unwrap();
        let st = SavedState {
            url: "http://x/y".into(),
            file_name: "y".into(),
            total: Some(10_000),
            etag: None,
            last_modified: None,
            content_type: None,
            resumable: true,
            single_connection: false,
            map,
        };
        tmp.save(&st).unwrap();
        let back = tmp.load().unwrap().unwrap();
        assert_eq!(back.map.chunks[0].done, 1234);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
