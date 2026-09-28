//! VNC Authentication (RFB security type 2): DES of a 16-byte challenge,
//! keyed with the password's first 8 bytes, each with its bits reversed.
//!
//! The scheme is weak (DES, 8 characters). It keeps other local users out of
//! the console, nothing more; tunnel remote access through SSH.

use des::{
    Des,
    cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray},
};

pub fn challenge() -> std::io::Result<[u8; 16]> {
    let mut challenge = [0_u8; 16];
    getrandom::fill(&mut challenge).map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(challenge)
}

/// Expected client response to `challenge` for `password`.
pub fn response(password: &[u8], challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = [0_u8; 8];
    for (slot, byte) in key.iter_mut().zip(password) {
        *slot = byte.reverse_bits();
    }
    let cipher = Des::new(GenericArray::from_slice(&key));
    let mut out = *challenge;
    for block in out.chunks_exact_mut(8) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    out
}

/// Compares without an early exit.
pub fn matches(expected: &[u8; 16], received: &[u8; 16]) -> bool {
    expected
        .iter()
        .zip(received)
        .fold(0, |diff, (a, b)| diff | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn des_known_answer_with_bit_reversed_key() {
        // Classic DES example: key 133457799BBCDFF1, 0123456789ABCDEF
        // encrypts to 85E813540F0AB405.
        let key = [0x13, 0x34, 0x57, 0x79, 0x9b, 0xbc, 0xdf, 0xf1];
        let password: Vec<u8> = key.iter().map(|byte: &u8| byte.reverse_bits()).collect();
        let block = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        let mut challenge = [0_u8; 16];
        challenge[..8].copy_from_slice(&block);
        challenge[8..].copy_from_slice(&block);
        let out = response(&password, &challenge);
        let expected = [0x85, 0xe8, 0x13, 0x54, 0x0f, 0x0a, 0xb4, 0x05];
        assert_eq!(out[..8], expected);
        assert_eq!(out[8..], expected);
    }

    #[test]
    fn only_eight_password_bytes_count() {
        let challenge = [7_u8; 16];
        assert_eq!(
            response(b"12345678", &challenge),
            response(b"123456789", &challenge)
        );
        assert!(matches(&challenge, &challenge));
        assert!(!matches(&challenge, &[0; 16]));
    }
}
