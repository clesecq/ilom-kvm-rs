use std::{
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use native_tls::{Protocol, TlsConnector, TlsStream};
use sha2::{Digest, Sha256};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// How the SP certificate is trusted. ILOM uses self-signed certificates that
/// are often expired, so normal chain validation is not useful.
#[derive(Debug, Clone, Copy)]
pub enum CertPolicy {
    /// Accept only a certificate whose DER SHA-256 digest matches.
    Pinned([u8; 32]),
    /// Accept any certificate (only when the caller has no fingerprint).
    Insecure,
}

pub fn connect_tcp(host: &str, port: u16) -> Result<TcpStream> {
    let address = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolve {host}"))?
        .next()
        .ok_or_else(|| anyhow!("{host} did not resolve"))?;
    let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
        .with_context(|| format!("connect to {address}"))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

pub fn connect(host: &str, port: u16, policy: CertPolicy) -> Result<TlsStream<TcpStream>> {
    let tcp = connect_tcp(host, port)?;
    let connector = TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .min_protocol_version(Some(Protocol::Tlsv12))
        .build()
        .context("build TLS connector")?;
    let stream = connector
        .connect(host, tcp)
        .map_err(|error| anyhow!("TLS handshake with {host}:{port} failed: {error}"))?;

    if let CertPolicy::Pinned(expected) = policy {
        let certificate = stream
            .peer_certificate()
            .context("read peer certificate")?
            .ok_or_else(|| anyhow!("{host}:{port} sent no certificate"))?;
        let der = certificate.to_der().context("encode peer certificate")?;
        let actual: [u8; 32] = Sha256::digest(&der).into();
        if actual != expected {
            bail!(
                "certificate fingerprint mismatch on {host}:{port}: expected {}, got {}",
                format_fingerprint(&expected),
                format_fingerprint(&actual)
            );
        }
    }
    Ok(stream)
}

pub fn format_fingerprint(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}
