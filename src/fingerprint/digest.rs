// SPDX-License-Identifier: MIT OR Apache-2.0

//! Byte-level digests. Hex encoding is written out rather than pulled in as a
//! dependency: it is nine lines, it cannot fail, and it keeps the crate's
//! dependency set limited to things that actually need a crate.

use sha2::{Digest as _, Sha256};

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Lowercase hex encoding, two characters per byte.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX_DIGITS[(byte >> 4) as usize] as char);
        out.push(HEX_DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// SHA-256 as lowercase hex, without an algorithm prefix.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encodes_lowercase_zero_padded_bytes() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }

    #[test]
    fn hex_encodes_empty_input_as_empty_string() {
        assert_eq!(hex(&[]), "");
    }

    #[test]
    fn sha256_matches_the_published_vector_for_hello_world() {
        // Vector from the RustCrypto sha2 crate documentation.
        assert_eq!(
            sha256_hex(b"hello world"),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn sha256_of_empty_input_matches_the_published_vector() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
