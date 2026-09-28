//! ILOM web UI login, used to obtain a fresh console JNLP.
//!
//! The ILOM web server emits malformed HTTP (it repeats the status line), so
//! this uses a tiny HTTP/1.0 client over TLS instead of a full HTTP stack.

use std::io::{Read, Write};

use anyhow::{Context, Result, anyhow, bail};
use tracing::{debug, info, warn};

use crate::{
    jnlp::{self, ConsoleArgs},
    known_certs::KnownCerts,
    tls::{self, CertPolicy, FingerprintMismatch},
};

pub const HTTPS_PORT: u16 = 443;
const LOGIN_COOKIE: &str = "ORA_ILOM_LOGIN";
const SESSION_COOKIE: &str = "ORA_ILOM_SESSION_SP";

struct Response {
    status: u16,
    cookies: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn cookie(&self, name: &str) -> Option<&str> {
        self.cookies
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Authenticated ILOM web session.
pub struct WebSession {
    host: String,
    policy: CertPolicy,
    session: String,
    /// SHA-256 of the web server certificate, compared with the JNLP later.
    fingerprint: [u8; 32],
}

impl WebSession {
    pub fn login(host: &str, policy: CertPolicy, username: &str, password: &str) -> Result<Self> {
        let (page, fingerprint) = request(host, policy, "GET", "/iPages/i_login.asp", &[], None)?;
        let login_cookie = page
            .cookie(LOGIN_COOKIE)
            .ok_or_else(|| anyhow!("login page did not set {LOGIN_COOKIE}"))?
            .to_string();
        let login_token = extract_login_token(&page.text())
            .ok_or_else(|| anyhow!("login page has no loginToken"))?;

        let body = form_encode(&[
            ("sclink", ""),
            ("loginToken", &login_token),
            ("username", username),
            ("password", password),
            ("button", "Log In"),
        ]);
        let cookies = [(LOGIN_COOKIE, login_cookie.as_str()), ("ilom", "1")];
        let (reply, _) = request(
            host,
            policy,
            "POST",
            "/iPages/loginProcessor.asp",
            &cookies,
            Some(body.as_bytes()),
        )?;
        let text = reply.text();
        let Some(session) = reply.cookie(SESSION_COOKIE) else {
            let code = text
                .split("msg=")
                .nth(1)
                .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
                .unwrap_or("?");
            return Err(LoginRejected {
                username: username.to_string(),
                code: code.to_string(),
            }
            .into());
        };
        info!(host, username, "ILOM web login succeeded");
        Ok(Self {
            host: host.to_string(),
            policy,
            session: session.to_string(),
            fingerprint,
        })
    }

    /// SHA-256 of the web server certificate seen at login.
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// Asks the SP for a new console launch file. Each one carries a fresh
    /// single-use tokend secret.
    pub fn console_args(&self) -> Result<ConsoleArgs> {
        let xml = self.jnlp_xml()?;
        let args = jnlp::parse(&xml).context("parse JNLP from ILOM")?;
        if let Some(expected) = args.fingerprint
            && expected != self.fingerprint
        {
            warn!(
                web = %tls::format_fingerprint(&self.fingerprint),
                jnlp = %tls::format_fingerprint(&expected),
                "web server certificate differs from the console certificate"
            );
        }
        Ok(args)
    }

    pub fn jnlp_xml(&self) -> Result<String> {
        let (reply, _) = request(
            &self.host,
            self.policy,
            "GET",
            "/cgi-bin/jnlpgenerator-16",
            &[(SESSION_COOKIE, &self.session)],
            None,
        )?;
        let text = reply.text();
        if reply.status != 200 || !text.contains("<jnlp") {
            bail!("ILOM did not return a JNLP (HTTP {})", reply.status);
        }
        Ok(text)
    }

    pub fn logout(self) -> Result<()> {
        request(
            &self.host,
            self.policy,
            "GET",
            "/logout.asp",
            &[(SESSION_COOKIE, &self.session)],
            None,
        )?;
        debug!("ILOM web session closed");
        Ok(())
    }
}

/// The ILOM refused the username or password. Retrying would only count
/// more failed logins against the account.
#[derive(Debug, thiserror::Error)]
#[error("ILOM web login failed for {username} (msg={code})")]
pub struct LoginRejected {
    pub username: String,
    pub code: String,
}

/// Logs in, mints a JNLP and logs out again (ILOM has few web slots).
///
/// The web certificate is pinned trust-on-first-use through [`KnownCerts`]:
/// the first successful login records it, and later logins refuse a
/// different certificate before the password is sent.
pub fn fetch_console_args(host: &str, username: &str, password: &str) -> Result<ConsoleArgs> {
    let mut known = KnownCerts::load_default()?;
    let pinned = known.get(host);
    let policy = match pinned {
        Some(fingerprint) => CertPolicy::Pinned(fingerprint),
        None => CertPolicy::Insecure,
    };
    let web = match WebSession::login(host, policy, username, password) {
        Ok(web) => web,
        Err(error) if error.chain().any(|cause| cause.is::<FingerprintMismatch>()) => {
            return Err(error.context(format!(
                "the ILOM certificate changed since the last login; if this is \
                 expected (new certificate), run `ilom-kvm forget-cert {host}` or \
                 remove the line from {}",
                known.path().display()
            )));
        }
        Err(error) => return Err(error),
    };
    if pinned.is_none() {
        let fingerprint = web.fingerprint();
        warn!(
            host,
            fingerprint = %tls::format_fingerprint(&fingerprint),
            "first login to this ILOM: trusting its certificate from now on"
        );
        if let Err(error) = known.insert(host, fingerprint) {
            warn!(%error, "cannot save the certificate fingerprint");
        }
    }
    let args = web.console_args();
    if let Err(error) = web.logout() {
        warn!(%error, "ILOM web logout failed");
    }
    args
}

fn request(
    host: &str,
    policy: CertPolicy,
    method: &str,
    path: &str,
    cookies: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Result<(Response, [u8; 32])> {
    let mut stream = tls::connect(host, HTTPS_PORT, policy)?;
    let fingerprint = tls::peer_fingerprint(&stream)?;
    let mut head = format!("{method} {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: ilom-kvm\r\n");
    if !cookies.is_empty() {
        let joined: Vec<String> = cookies.iter().map(|(k, v)| format!("{k}={v}")).collect();
        head.push_str(&format!("Cookie: {}\r\n", joined.join("; ")));
    }
    if let Some(body) = body {
        head.push_str("Content-Type: application/x-www-form-urlencoded\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if let Some(body) = body {
        stream.write_all(body)?;
    }
    stream.flush()?;

    let mut raw = Vec::new();
    match stream.read_to_end(&mut raw) {
        Ok(_) => {}
        // The SP often drops the connection without a TLS close_notify.
        Err(error) if !raw.is_empty() => debug!(%error, "HTTP read ended"),
        Err(error) => return Err(error).with_context(|| format!("read {path}")),
    }
    debug!(path, bytes = raw.len(), head = %String::from_utf8_lossy(&raw[..raw.len().min(60)]), "HTTP raw");
    let response = parse_response(&raw).with_context(|| format!("parse response to {path}"))?;
    debug!(
        path,
        status = response.status,
        bytes = response.body.len(),
        "HTTP"
    );
    Ok((response, fingerprint))
}

fn parse_response(raw: &[u8]) -> Result<Response> {
    // Pages differ in line endings: accept both CRLF and bare LF.
    let (split, terminator) = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|at| (at, 4))
        .into_iter()
        .chain(
            raw.windows(2)
                .position(|window| window == b"\n\n")
                .map(|at| (at, 2)),
        )
        .min_by_key(|(at, _)| *at)
        .ok_or_else(|| anyhow!("HTTP response has no header terminator"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut status = 0;
    let mut cookies = Vec::new();
    for line in head.lines() {
        if let Some(rest) = line.strip_prefix("HTTP/") {
            // The ILOM may send several status lines; keep the last one.
            status = rest
                .split_whitespace()
                .nth(1)
                .and_then(|code| code.parse().ok())
                .unwrap_or(0);
        } else if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("set-cookie")
        {
            let pair = value.trim().split(';').next().unwrap_or("");
            if let Some((key, value)) = pair.split_once('=') {
                // Expired cookies are deletions.
                if !value.is_empty() && !line.contains("expires=Thu, 01-Jan-70") {
                    cookies.push((key.trim().to_string(), value.trim().to_string()));
                }
            }
        }
    }
    Ok(Response {
        status,
        cookies,
        body: raw[split + terminator..].to_vec(),
    })
}

fn extract_login_token(page: &str) -> Option<String> {
    let rest = page.split("setElementValue(\"loginToken\",").nth(1)?;
    let start = rest.find('"')? + 1;
    let len = rest[start..].find('"')?;
    Some(rest[start..start + len].to_string())
}

fn form_encode(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repeated_status_lines_and_cookies() {
        let raw = b"HTTP/1.0 200 OK\r\nHTTP/1.0 200 OK\r\nSet-Cookie: ORA_ILOM_SESSION_SP=abc; path=/; secure\r\nSet-Cookie: ORA_ILOM_LOGIN=x; path=/; expires=Thu, 01-Jan-70 00:00:01 GMT\r\n\r\n<jnlp/>";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.cookie(SESSION_COOKIE), Some("abc"));
        assert_eq!(response.cookie(LOGIN_COOKIE), None);
        assert_eq!(response.body, b"<jnlp/>");
        let lf = parse_response(b"HTTP/1.0 303 \nLocation: /x\n\nbody").unwrap();
        assert_eq!(lf.status, 303);
        assert_eq!(lf.body, b"body");
    }

    #[test]
    fn extracts_login_token() {
        let page = r#"    setElementValue("loginToken", "qVUmpxpCkU");"#;
        assert_eq!(extract_login_token(page).as_deref(), Some("qVUmpxpCkU"));
    }

    #[test]
    fn percent_encodes_form_values() {
        assert_eq!(form_encode(&[("a", "b c&d")]), "a=b+c%26d");
    }
}
