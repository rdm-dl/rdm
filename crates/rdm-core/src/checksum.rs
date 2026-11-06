//! Checking a finished file against the checksum its site publishes. The kind of checksum is
//! told from its length, so all a person has to do is paste it.

use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// `SHA-256` and the like.
    pub kind: &'static str,
    /// The file's own checksum.
    pub actual: String,
    pub matches: bool,
}

/// The checksum in pasted text, which may have a label or a file name around it, in lower case.
pub fn find_in(text: &str) -> Option<String> {
    let is_sum = |word: &&str| matches!(word.len(), 32 | 40 | 64 | 128) && word.bytes().all(|b| b.is_ascii_hexdigit());
    text.split(|c: char| !c.is_ascii_alphanumeric()).find(is_sum).map(str::to_ascii_lowercase)
}

fn name_of(sum: &str) -> &'static str {
    match sum.len() {
        32 => "MD5",
        40 => "SHA-1",
        64 => "SHA-256",
        _ => "SHA-512",
    }
}

fn hash<D: Digest>(path: &Path, read: &AtomicU64) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut digest = D::new();
    let mut block = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut block)?;
        if n == 0 {
            break;
        }
        digest.update(&block[..n]);
        read.fetch_add(n as u64, Ordering::Relaxed);
    }
    Ok(digest.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Compare `path` with the checksum in `text`. `read` counts the bytes done, for a progress bar.
/// Reads the whole file, so it belongs on a blocking thread.
pub fn check(path: &Path, text: &str, read: &AtomicU64) -> Result<Outcome, String> {
    let expected = find_in(text).ok_or("That doesn't look like a checksum. RDM knows MD5, SHA-1, SHA-256 and SHA-512.")?;
    let actual = match expected.len() {
        32 => hash::<Md5>(path, read),
        40 => hash::<Sha1>(path, read),
        64 => hash::<Sha256>(path, read),
        _ => hash::<Sha512>(path, read),
    };
    let actual = actual.map_err(|e| format!("The file can't be read: {e}"))?;
    Ok(Outcome { kind: name_of(&expected), matches: actual == expected, actual })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_are_found_in_what_sites_write() {
        let sum = "9F86D081884C7D659A2FEAA0C55AD015A3BF4F1B2B0B822CD15D6C15B0F00A08";
        assert_eq!(find_in(sum).as_deref(), Some(sum.to_ascii_lowercase().as_str()));
        assert_eq!(find_in(&format!("sha256: {sum}  file.iso")).as_deref(), Some(sum.to_ascii_lowercase().as_str()));
        assert_eq!(find_in("not a checksum"), None);
        assert_eq!(find_in("abc123"), None);
    }

    #[test]
    fn every_kind_is_checked() {
        let path = std::env::temp_dir().join(format!("rdm-checksum-{}", std::process::id()));
        std::fs::write(&path, "test").unwrap();
        let read = AtomicU64::new(0);
        for (kind, sum) in [
            ("MD5", "098f6bcd4621d373cade4e832627b4f6"),
            ("SHA-1", "a94a8fe5ccb19ba61c4c0873d391e987982fbbd3"),
            ("SHA-256", "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"),
            ("SHA-512", "ee26b0dd4af7e749aa1a8ee3c10ae9923f618980772e473f8819a5d4940e0db27ac185f8a0e1d5f84f88bc887fd67b143732c304cc5fa9ad8e6f57f50028a8ff"),
        ] {
            let outcome = check(&path, sum, &read).unwrap();
            assert_eq!(outcome.kind, kind);
            assert!(outcome.matches, "{kind}");
        }
        let wrong = check(&path, "098f6bcd4621d373cade4e832627b4f7", &read).unwrap();
        assert!(!wrong.matches);
        assert_eq!(wrong.actual, "098f6bcd4621d373cade4e832627b4f6");
        assert!(read.load(Ordering::Relaxed) > 0);
        std::fs::remove_file(&path).ok();
    }
}
