use anyhow::{Result, anyhow};
use md5::{Digest, Md5};

/// Length of the Unix MD5-crypt string (`$1$` + 8-byte salt + `$` + 22).
pub const MD5_CRYPT_LEN: usize = 34;

/// Salt as sent by the SP: MD5-crypt salts keep their `$1$...$` form,
/// anything else is a two-character DES salt.
pub fn normalize_salt(raw: &[u8]) -> String {
    let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
    let salt = String::from_utf8_lossy(&raw[..end]).into_owned();
    if salt.starts_with("$1$") {
        salt
    } else {
        salt.chars().take(2).collect()
    }
}

/// Unix crypt of `password`, zero-padded or truncated to `len` bytes.
#[allow(deprecated)] // Legacy crypt schemes are what the SP expects.
pub fn unix_hash(password: &str, salt: &str, len: usize) -> Result<Vec<u8>> {
    let hashed = if salt.starts_with("$1$") {
        pwhash::md5_crypt::hash_with(salt, password)
    } else {
        pwhash::unix_crypt::hash_with(salt, password)
    }
    .map_err(|error| anyhow!("crypt with salt {salt:?} failed: {error}"))?;
    let mut out = vec![0_u8; len];
    let copy = hashed.len().min(len);
    out[..copy].copy_from_slice(&hashed.as_bytes()[..copy]);
    Ok(out)
}

/// Challenge response: MD5 over the padded Unix hash followed by the challenge.
pub fn challenge_digest(unix_hash: &[u8], challenge: &[u8]) -> [u8; 16] {
    let mut digest = Md5::new();
    digest.update(unix_hash);
    digest.update(challenge);
    digest.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_crypt_matches_reference_vector() {
        // Vector from `openssl passwd -1 -salt saltsalt password`.
        let hash = unix_hash("password", "$1$saltsalt$", MD5_CRYPT_LEN).unwrap();
        assert_eq!(hash, b"$1$saltsalt$qjXMvbEw8oaL.CzflDtaK/");
    }

    #[test]
    fn salts_are_normalized() {
        assert_eq!(normalize_salt(b"$1$abcdefgh$\0"), "$1$abcdefgh$");
        assert_eq!(normalize_salt(b"xyzzy\0\0\0"), "xy");
    }
}
