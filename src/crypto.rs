use anyhow::{Result, anyhow};
use md5::{Digest, Md5};

/// Length of the Unix MD5-crypt string (`$1$` + 8-byte salt + `$` + 22).
pub const MD5_CRYPT_LEN: usize = 34;

const DES_ALPHABET: &[u8] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Salt as sent by the SP: MD5-crypt salts keep their `$1$...$` form (up to
/// the first NUL), anything else is the first two raw bytes, used for DES.
pub fn normalize_salt(raw: &[u8]) -> Vec<u8> {
    if raw.starts_with(b"$1$") {
        let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
        raw[..end].to_vec()
    } else {
        raw.iter()
            .copied()
            .chain(std::iter::repeat(0))
            .take(2)
            .collect()
    }
}

/// Unix crypt of `password`, zero-padded or truncated to `len` bytes.
///
/// DES salts may contain bytes outside the crypt alphabet (the SP sends NULs).
/// Like the vendor client, such bytes count as salt value 0 but are copied
/// unchanged into the first two bytes of the result.
#[allow(deprecated)] // Legacy crypt schemes are what the SP expects.
pub fn unix_hash(password: &str, salt: &[u8], len: usize) -> Result<Vec<u8>> {
    let mut hashed = if salt.starts_with(b"$1$") {
        let salt = std::str::from_utf8(salt)?;
        pwhash::md5_crypt::hash_with(salt, password)
            .map_err(|error| anyhow!("MD5 crypt with salt {salt:?} failed: {error}"))?
            .into_bytes()
    } else {
        let mapped: String = salt
            .iter()
            .take(2)
            .map(|b| {
                if DES_ALPHABET.contains(b) {
                    *b as char
                } else {
                    '.'
                }
            })
            .collect();
        let mut hashed = pwhash::unix_crypt::hash_with(mapped.as_str(), password)
            .map_err(|error| anyhow!("DES crypt failed: {error}"))?
            .into_bytes();
        hashed[..2].copy_from_slice(&salt[..2]);
        hashed
    };
    hashed.resize(len, 0);
    Ok(hashed)
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
        let hash = unix_hash("password", b"$1$saltsalt$", MD5_CRYPT_LEN).unwrap();
        assert_eq!(hash, b"$1$saltsalt$qjXMvbEw8oaL.CzflDtaK/");
    }

    #[test]
    fn des_salt_with_nul_bytes_behaves_like_dots() {
        // Vector from `openssl passwd -crypt -salt .. token` (DES).
        let hash = unix_hash("token", &[0, 0], 13).unwrap();
        let reference = unix_hash("token", b"..", 13).unwrap();
        assert_eq!(&hash[..2], &[0, 0]);
        assert_eq!(&hash[2..], &reference[2..]);
    }

    #[test]
    fn salts_are_normalized() {
        assert_eq!(normalize_salt(b"$1$abcdefgh$\0"), b"$1$abcdefgh$");
        assert_eq!(normalize_salt(b"xyzzy\0\0\0"), b"xy");
        assert_eq!(normalize_salt(&[0; 12]), vec![0, 0]);
    }
}

/// Salt the SP uses for its (non-standard) SHA-512 "crypt" in HID key
/// derivation.
const HID_SHA512_SALT: &str = "$6$I27m0w5z15um9P5f";
const HID_HASH_FIELD: usize = 129;

/// The vendor "SHA-512 crypt": a single SHA-512 over the password, the `$6$`
/// magic and at most 13 salt characters. Not the glibc algorithm.
fn vendor_sha512_crypt(password: &str, salt: &str) -> [u8; 64] {
    use sha2::{Digest as _, Sha512};
    let end = salt.len().min(16);
    let mut digest = Sha512::new();
    digest.update(password.as_bytes());
    digest.update(&salt.as_bytes()[..3]);
    digest.update(&salt.as_bytes()[3..end]);
    digest.finalize().into()
}

/// AES-128 key for HID encryption level 2, derived from the session username
/// and the 32-byte challenge data returned by tokend.
pub fn hid_aes_key(username: &str, challenge_data: &[u8]) -> [u8; 16] {
    use sha2::{Digest as _, Sha512};
    let mut hash = vendor_sha512_crypt(username, HID_SHA512_SALT).to_vec();
    hash.resize(HID_HASH_FIELD, 0);
    let mut digest = Sha512::new();
    digest.update(&hash);
    digest.update(challenge_data);
    let digest = digest.finalize();
    digest[..16].try_into().unwrap()
}

/// AES-128-CBC with PKCS#7 padding, restarting from `iv` for every message.
pub fn aes_cbc_encrypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
    cbc::Encryptor::<aes::Aes128>::new(key.into(), iv.into()).encrypt_padded_vec_mut::<Pkcs7>(data)
}

#[cfg(test)]
mod hid_tests {
    use super::*;

    #[test]
    fn aes_output_is_padded_to_blocks() {
        let key = hid_aes_key("root-sp-1", &[7; 32]);
        assert_eq!(aes_cbc_encrypt(&key, &[0; 16], &[0; 8]).len(), 16);
        assert_eq!(aes_cbc_encrypt(&key, &[0; 16], &[0; 16]).len(), 32);
    }
}
