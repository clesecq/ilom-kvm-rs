use anyhow::{Context, Result, bail};

/// Launch parameters that the ILOM web UI embeds in `jnlpgenerator-*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleArgs {
    /// Requested colour depth. 0 selects the serial console instead of video.
    pub color_depth: u8,
    pub host: String,
    /// Per-launch service processor account, e.g. `root-sp-1`.
    pub username: String,
    /// One-time secret that authenticates `username` against tokend.
    pub secret: String,
    /// SHA-256 fingerprint of the SP certificate (32 bytes).
    pub fingerprint: Option<[u8; 32]>,
    pub certificate_pem: Option<String>,
}

pub fn parse(xml: &str) -> Result<ConsoleArgs> {
    let document = roxmltree::Document::parse(xml).context("parse JNLP XML")?;
    let arguments: Vec<&str> = document
        .descendants()
        .find(|node| node.has_tag_name("application-desc"))
        .context("JNLP has no <application-desc>")?
        .children()
        .filter(|node| node.has_tag_name("argument"))
        .map(|node| node.text().unwrap_or("").trim())
        .collect();
    from_arguments(&arguments)
}

/// Interprets the console's positional arguments for a single session:
/// `depth host user secret [fingerprint [pem]]`.
pub fn from_arguments(arguments: &[&str]) -> Result<ConsoleArgs> {
    let arguments: Vec<&str> = match arguments.last() {
        Some(&"CMM") => arguments[..arguments.len() - 1].to_vec(),
        _ => arguments.to_vec(),
    };
    if arguments.len() < 4 {
        bail!("expected at least 4 console arguments, got {}", arguments.len());
    }
    if arguments.len() > 6 {
        bail!("multi-session (blade) JNLP files are not supported");
    }
    let color_depth = arguments[0]
        .parse()
        .with_context(|| format!("invalid colour depth {:?}", arguments[0]))?;
    let fingerprint = arguments
        .get(4)
        .map(|value| parse_fingerprint(value))
        .transpose()?;
    let certificate_pem = arguments.get(5).map(|pem| pem.to_string());
    if let Some(pem) = &certificate_pem
        && !pem.contains("BEGIN CERTIFICATE")
    {
        bail!("sixth console argument is not a PEM certificate");
    }
    Ok(ConsoleArgs {
        color_depth,
        host: arguments[1].to_string(),
        username: arguments[2].to_string(),
        secret: arguments[3].to_string(),
        fingerprint,
        certificate_pem,
    })
}

pub fn parse_fingerprint(value: &str) -> Result<[u8; 32]> {
    let digits: String = value.chars().filter(|c| *c != ':').collect();
    let bytes = hex::decode(&digits).with_context(|| format!("invalid fingerprint {value:?}"))?;
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| anyhow::anyhow!("fingerprint has {} bytes, expected 32", bytes.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<jnlp spec="1.0+" codebase="https://192.0.2.1:443/">
  <resources><jar href="Java/JavaRConsole.jar"/></resources>
  <application-desc>
    <argument>16</argument>
            <argument>192.0.2.1</argument>
    <argument>root-sp-1</argument>
    <argument>secret123</argument>
    <argument>40:c9:a9:be:9b:e0:e0:dc:04:76:76:af:5a:db:c5:9e:a1:44:b2:5e:e4:84:94:e7:10:34:b3:bb:28:e0:ff:b0</argument>
    <argument>-----BEGIN CERTIFICATE-----
MIIB
-----END CERTIFICATE-----
</argument>
  </application-desc>
</jnlp>"#;

    #[test]
    fn parses_single_session_jnlp() {
        let args = parse(SAMPLE).unwrap();
        assert_eq!(args.color_depth, 16);
        assert_eq!(args.host, "192.0.2.1");
        assert_eq!(args.username, "root-sp-1");
        assert_eq!(args.secret, "secret123");
        let fingerprint = args.fingerprint.unwrap();
        assert_eq!(fingerprint[0], 0x40);
        assert_eq!(fingerprint[31], 0xb0);
        assert!(args.certificate_pem.unwrap().starts_with("-----BEGIN CERTIFICATE-----"));
    }

    #[test]
    fn accepts_arguments_without_pinning() {
        let args = from_arguments(&["16", "h", "u", "p"]).unwrap();
        assert!(args.fingerprint.is_none());
        assert!(from_arguments(&["16", "h", "u"]).is_err());
    }
}
