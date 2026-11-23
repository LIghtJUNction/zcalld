use sha2::Digest;
use std::{fs::File, io::Read, path::Path};

pub struct Sha256(sha2::Sha256);
impl Sha256 {
    pub fn new() -> Self {
        Self(sha2::Sha256::new())
    }
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
    pub fn finish(self) -> String {
        let mut encoded = vec![0; 64];
        zhex::encode_lower(&self.0.finalize(), &mut encoded)
            .expect("SHA-256 output fits in 64 hexadecimal bytes");
        String::from_utf8(encoded).expect("hexadecimal output is ASCII")
    }
}
pub fn sha256(bytes: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(bytes);
    hash.finish()
}
pub fn sha256_file(path: &Path, limit: u64) -> Result<String, String> {
    let mut file = File::open(path).map_err(|_| "cannot read installation file")?;
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|_| "cannot read installation file")?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n as u64)
            .ok_or("installation file is too large")?;
        if total > limit {
            return Err("installation file is too large".into());
        }
        hash.update(&buffer[..n]);
    }
    Ok(hash.finish())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn standard_vectors_across_chunk_boundaries() {
        assert_eq!(
            sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let mut hash = Sha256::new();
        for _ in 0..1000 {
            hash.update(&[b'a'; 1000]);
        }
        assert_eq!(
            hash.finish(),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }
}
