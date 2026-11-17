//! SHA1 哈希（info_hash / pieces_hash）。
//!
//! golden 值对齐 Python tests/helpers.py（INDEX §11）。

use sha1::{Digest, Sha1};

pub fn sha1_hex(data: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(40);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PIECES_HASH_GOLDEN（Python tests/helpers.py，SHA1(b"\x00"*20)）
    #[test]
    fn pieces_hash_golden() {
        assert_eq!(
            sha1_hex(&[0u8; 20]),
            "6768033e216468247bd031a0a2d9876d79818f8f"
        );
    }

    #[test]
    fn sha1_known_vectors() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }
}
