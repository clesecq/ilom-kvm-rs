use std::{
    io::{Read, Write},
    net::TcpStream,
};

use anyhow::{Context, Result, bail};
use native_tls::TlsStream;
use tracing::debug;

use crate::tls::{self, CertPolicy};

pub const PORT: u16 = 5556;
pub const TOKEN_LEN: usize = 20;
pub const CHALLENGE_LEN: usize = 32;
pub const SESSION_HANDLE_LEN: usize = 4;

const USERNAME_FIELD: usize = 16;
const PASSWORD_FIELD: usize = 32;
const GET_TOKEN: u8 = 1;
const GET_CHALLENGE: u8 = 2;
const AUTH_OK: u8 = 1;

/// Session with the SP token daemon. It trades the one-time JNLP secret for
/// redirection tokens and derives per-channel encryption challenges.
pub struct Tokend {
    stream: TlsStream<TcpStream>,
}

impl Tokend {
    pub fn connect(host: &str, policy: CertPolicy, username: &str, secret: &str) -> Result<Self> {
        let mut stream = tls::connect(host, PORT, policy).context("connect to tokend")?;
        let mut login = fixed_field(username, USERNAME_FIELD);
        login.extend(fixed_field(secret, PASSWORD_FIELD));
        stream.write_all(&login).context("send tokend credentials")?;
        let mut result = [0_u8; 1];
        stream
            .read_exact(&mut result)
            .context("read tokend authentication result")?;
        debug!(result = result[0], "tokend authentication");
        if result[0] != AUTH_OK {
            bail!(
                "tokend rejected {username} (result {}); the JNLP secret is single-use, fetch a fresh one",
                result[0]
            );
        }
        Ok(Self { stream })
    }

    pub fn redirection_token(&mut self) -> Result<[u8; TOKEN_LEN]> {
        self.stream.write_all(&[GET_TOKEN])?;
        let mut token = [0_u8; TOKEN_LEN];
        self.stream
            .read_exact(&mut token)
            .context("read redirection token")?;
        Ok(token)
    }

    pub fn challenge_data(
        &mut self,
        session_handle: &[u8; SESSION_HANDLE_LEN],
    ) -> Result<[u8; CHALLENGE_LEN]> {
        let mut request = vec![GET_CHALLENGE];
        request.extend_from_slice(session_handle);
        self.stream.write_all(&request)?;
        let mut challenge = [0_u8; CHALLENGE_LEN];
        self.stream
            .read_exact(&mut challenge)
            .context("read tokend challenge data")?;
        Ok(challenge)
    }

    pub fn close(mut self) {
        let _ = self.stream.shutdown();
    }
}

/// Zero-padded field that truncates overlong values, as the vendor client does.
fn fixed_field(value: &str, len: usize) -> Vec<u8> {
    let mut field = vec![0_u8; len];
    let bytes = value.as_bytes();
    let copy = bytes.len().min(len);
    field[..copy].copy_from_slice(&bytes[..copy]);
    field
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_fields_pad_and_truncate() {
        assert_eq!(fixed_field("ab", 4), b"ab\0\0");
        assert_eq!(fixed_field("abcdef", 4), b"abcd");
    }
}
