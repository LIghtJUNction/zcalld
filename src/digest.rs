//! SHA-256 for release integrity and local change detection.
use sha2::{Digest, Sha256};

pub fn sha256(bytes: &[u8]) -> String {
    let mut encoded = vec![0; 64];
    zhex::encode_lower(&Sha256::digest(bytes), &mut encoded)
        .expect("SHA-256 output fits in 64 hexadecimal bytes");
    String::from_utf8(encoded).expect("hexadecimal output is ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn standard_vectors() {
        assert_eq!(
            sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
