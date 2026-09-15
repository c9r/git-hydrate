//! Streaming sha256, which is the identity of every object.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

/// An in-progress sha256.
pub struct Hasher {
    inner: Sha256,
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Hasher {
    pub fn new() -> Hasher {
        Hasher { inner: Sha256::new() }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.inner.update(bytes);
    }

    /// The digest as lowercase hex, which is the form pointers carry.
    pub fn finish(self) -> String {
        hex::encode(self.inner.finalize())
    }
}

/// The size of the buffer streaming reads go through.
const BUFFER: usize = 1 << 20;

/// Hashes a file from disk, returning its oid and size.
pub fn hash_file(path: &Path) -> io::Result<(String, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = Hasher::new();
    let mut buf = vec![0u8; BUFFER];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((hasher.finish(), size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn hashes_known_values() {
        let mut hasher = Hasher::new();
        hasher.update(b"abc");
        assert_eq!(hasher.finish(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(Hasher::new().finish(), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn hashes_a_file_in_pieces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let content = vec![0xabu8; BUFFER * 3 + 17];
        File::create(&path).unwrap().write_all(&content).unwrap();
        let mut hasher = Hasher::new();
        hasher.update(&content);
        let expected = hasher.finish();
        assert_eq!(hash_file(&path).unwrap(), (expected, content.len() as u64));
    }
}
