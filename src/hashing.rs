use std::{
    fs::File,
    io::{BufReader, Read},
    path::Path,
};

use anyhow::{Context, Result};
use xxhash_rust::xxh3::Xxh3;

pub fn hash_bytes(data: &[u8]) -> u128 {
    let mut hasher = Xxh3::new();
    hasher.update(data);
    hasher.digest128()
}

pub fn hash_file(path: &Path) -> Result<u128> {
    let file =
        File::open(path).with_context(|| format!("open file for hash: {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Xxh3::new();
    let mut buf = vec![0_u8; 1024 * 1024];

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(hasher.digest128())
}

pub fn format_digest(v: u128) -> String {
    format!("{v:032x}")
}

#[cfg(test)]
mod tests {
    use super::{format_digest, hash_bytes};

    #[test]
    fn digest_is_stable() {
        let digest = hash_bytes(b"abc");
        assert_eq!(format_digest(digest), "06b05ab6733a618578af5f94892f3950");
    }
}
