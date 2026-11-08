//! The CA certificate for apps: the public certificate of the CA that signed
//! the server's certificate (mkcert's `rootCA.pem`, say). coversyncd serves
//! it at `GET /api/hello/ca` and puts its SHA-256 in the pairing QR code, so
//! the Android app can trust that CA for this server alone, having checked
//! it against a fingerprint from a logged-in browser.
//!
//! A file holding a private key is refused, whatever else it holds: the
//! CA's key often sits beside its certificate (`rootCA-key.pem`) and is
//! readable by the same user.

use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine;
use sha2::{Digest, Sha256};

/// Bigger files aren't a CA certificate.
const MAX_BYTES: u64 = 256 * 1024;

const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
const END: &str = "-----END CERTIFICATE-----";

/// A CA certificate that passed the checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Certificate {
    der: Vec<u8>,
}

impl Certificate {
    /// The SHA-256 of the certificate, in lowercase hex: what the QR code's
    /// `ca=` holds.
    pub fn fingerprint(&self) -> String {
        Sha256::digest(&self.der)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// The fingerprint as `openssl x509 -noout -fingerprint -sha256` prints it.
    pub fn fingerprint_colons(&self) -> String {
        Sha256::digest(&self.der)
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// The certificate alone, in PEM: nothing else the file holds is served.
    pub fn pem(&self) -> String {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&self.der);
        let mut pem = format!("{BEGIN}\n");
        for line in b64.as_bytes().chunks(64) {
            pem.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
            pem.push('\n');
        }
        pem.push_str(END);
        pem.push('\n');
        pem
    }
}

/// The setting as typed: trimmed, `~/` for the home directory, and absolute.
/// `Ok(None)` when it's empty.
pub fn setting(typed: Option<&str>) -> Result<Option<PathBuf>, String> {
    let Some(typed) = typed.map(str::trim).filter(|t| !t.is_empty()) else {
        return Ok(None);
    };
    let path = match typed.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => return Err("There's no home directory to read ~ from.".into()),
        },
        None => PathBuf::from(typed),
    };
    if !path.is_absolute() {
        return Err(
            "Give the certificate's full path, like /home/you/.local/share/mkcert/rootCA.pem."
                .into(),
        );
    }
    Ok(Some(path))
}

/// Read and check the certificate at `path`. The error says what's wrong,
/// for Sync setup.
pub fn load(path: &Path) -> Result<Certificate, String> {
    let file = std::fs::File::open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => "There's no file at that path.".to_owned(),
        std::io::ErrorKind::PermissionDenied => {
            "coversyncd isn't allowed to read that file.".to_owned()
        }
        _ => format!("That file can't be read: {e}."),
    })?;
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Err("That path isn't a file.".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("That file can't be read: {e}."))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("That file is too big to be a CA certificate.".into());
    }
    check(&bytes)
}

/// Check a file's contents: one certificate, in PEM, and no private key.
pub fn check(bytes: &[u8]) -> Result<Certificate, String> {
    // First, whatever else is wrong: a key is never accepted.
    if contains(bytes, b"PRIVATE KEY") {
        return Err(
            "That file holds a private key. Give the CA's public certificate \
             (mkcert's rootCA.pem), never its key."
                .into(),
        );
    }
    let not_pem = || "That file isn't a PEM certificate (-----BEGIN CERTIFICATE-----).".to_owned();
    let text = std::str::from_utf8(bytes).map_err(|_| not_pem())?;
    let mut certificates = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(BEGIN) {
        let body = &rest[start + BEGIN.len()..];
        let end = body
            .find(END)
            .ok_or("A certificate in that file has no -----END CERTIFICATE----- line.")?;
        certificates.push(&body[..end]);
        rest = &body[end + END.len()..];
    }
    let body = match certificates[..] {
        [one] => one,
        [] => return Err(not_pem()),
        _ => {
            return Err(format!(
                "That file holds {} certificates. Give the CA's own, alone.",
                certificates.len()
            ));
        }
    };
    let b64: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|_| "The certificate in that file isn't valid base64.".to_owned())?;
    if !der_sequence(&der) {
        return Err("The certificate in that file is damaged.".into());
    }
    Ok(Certificate { der })
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Whether `der` is exactly one DER SEQUENCE, as a certificate is: a cheap
/// check that it wasn't cut short. The app parses it in full.
fn der_sequence(der: &[u8]) -> bool {
    let [0x30, first, rest @ ..] = der else {
        return false;
    };
    let (len, rest) = match *first {
        n @ 0..=0x7f => (usize::from(n), rest),
        n @ 0x81..=0x84 => {
            let size = usize::from(n & 0x7f);
            if rest.len() < size {
                return false;
            }
            let len = rest[..size]
                .iter()
                .fold(0usize, |len, &b| len << 8 | usize::from(b));
            (len, &rest[size..])
        }
        _ => return false,
    };
    rest.len() == len
}

#[cfg(test)]
mod tests {
    use super::*;

    const CA: &str = include_str!("testdata/ca.pem");
    /// `openssl x509 -noout -fingerprint -sha256 -in testdata/ca.pem`
    const OPENSSL: &str = "26:43:AD:A5:72:9B:B0:FE:6C:D0:50:0D:65:BB:48:C9:12:02:37:66:0E:00:0B:0C:9D:84:6C:4C:7D:48:0C:D4";
    /// Not a real key: only the armor matters.
    const KEY: &str = "-----BEGIN PRIVATE KEY-----\nTm90IGEga2V5\n-----END PRIVATE KEY-----\n";

    #[test]
    fn the_fingerprint_is_openssls() {
        let cert = check(CA.as_bytes()).unwrap();
        assert_eq!(cert.fingerprint_colons(), OPENSSL);
        assert_eq!(cert.fingerprint(), OPENSSL.replace(':', "").to_lowercase());
        // Served as it was given, without anything around it.
        assert_eq!(cert.pem(), CA);
        let noisy = format!(
            "Subject: coversync test CA\r\n{}\r\n",
            CA.replace('\n', "\r\n")
        );
        assert_eq!(check(noisy.as_bytes()).unwrap(), cert);
    }

    /// The same, from openssl itself, where it's installed.
    #[test]
    fn openssl_agrees() {
        let dir = std::env::temp_dir().join(format!("coversyncd-ca-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rootCA.pem");
        std::fs::write(&path, CA).unwrap();
        let Ok(out) = std::process::Command::new("openssl")
            .args(["x509", "-noout", "-fingerprint", "-sha256", "-in"])
            .arg(&path)
            .output()
        else {
            eprintln!("openssl isn't installed; skipped");
            return;
        };
        let printed = String::from_utf8(out.stdout).unwrap();
        let theirs = printed.trim().rsplit('=').next().unwrap();
        assert_eq!(theirs, load(&path).unwrap().fingerprint_colons());
    }

    #[test]
    fn private_keys_are_refused() {
        for text in [
            KEY.to_owned(),
            format!("{CA}{KEY}"),
            format!("{KEY}{CA}"),
            KEY.replace("PRIVATE KEY", "EC PRIVATE KEY"),
            KEY.replace("PRIVATE KEY", "ENCRYPTED PRIVATE KEY"),
            KEY.replace("PRIVATE KEY", "OPENSSH PRIVATE KEY"),
        ] {
            let err = check(text.as_bytes()).unwrap_err();
            assert!(err.contains("private key"), "{err}");
        }
        // Even in a file that isn't text.
        let mut binary = vec![0xff, 0xfe];
        binary.extend_from_slice(KEY.as_bytes());
        assert!(check(&binary).unwrap_err().contains("private key"));
    }

    #[test]
    fn anything_but_one_certificate_is_refused() {
        let cut = &CA[..CA.len() - 40];
        let damaged = CA.replacen("MIIB", "MIIC", 1);
        let cases = [
            ("", "isn't a PEM certificate"),
            ("hello", "isn't a PEM certificate"),
            (cut, "no -----END"),
            (&damaged, "damaged"),
        ];
        for (text, says) in cases {
            let err = check(text.as_bytes()).unwrap_err();
            assert!(err.contains(says), "{text:?}: {err}");
        }
        let two = format!("{CA}{CA}");
        assert!(
            check(two.as_bytes())
                .unwrap_err()
                .contains("2 certificates")
        );
        let der = base64::engine::general_purpose::STANDARD
            .decode(
                CA.lines()
                    .filter(|l| !l.starts_with("-----"))
                    .collect::<String>(),
            )
            .unwrap();
        assert!(check(&der).unwrap_err().contains("isn't a PEM"));
        let bad_b64 = CA.replacen("MIIB", "MI!B", 1);
        assert!(check(bad_b64.as_bytes()).unwrap_err().contains("base64"));
    }

    #[test]
    fn the_setting_takes_absolute_paths_and_home() {
        assert_eq!(setting(None), Ok(None));
        assert_eq!(setting(Some("  ")), Ok(None));
        assert_eq!(
            setting(Some(" /etc/ssl/ca.pem ")),
            Ok(Some(PathBuf::from("/etc/ssl/ca.pem")))
        );
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(
            setting(Some("~/.local/share/mkcert/rootCA.pem")),
            Ok(Some(home.join(".local/share/mkcert/rootCA.pem")))
        );
        assert!(setting(Some("rootCA.pem")).is_err());
    }

    #[test]
    fn files_are_read_with_care() {
        let dir = std::env::temp_dir().join(format!("coversyncd-ca-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(
            load(&dir.join("missing.pem"))
                .unwrap_err()
                .contains("no file")
        );
        assert!(load(&dir).unwrap_err().contains("isn't a file"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let locked = dir.join("locked.pem");
            std::fs::write(&locked, CA).unwrap();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
            // Root reads it anyway (CI's package build doesn't run as root).
            if std::fs::read(&locked).is_err() {
                assert!(load(&locked).unwrap_err().contains("isn't allowed"));
            }
        }
        let big = dir.join("big.pem");
        std::fs::write(&big, vec![b'a'; MAX_BYTES as usize + 1]).unwrap();
        assert!(load(&big).unwrap_err().contains("too big"));
        let key = dir.join("rootCA-key.pem");
        std::fs::write(&key, KEY).unwrap();
        assert!(load(&key).unwrap_err().contains("private key"));
        let ca = dir.join("rootCA.pem");
        std::fs::write(&ca, CA).unwrap();
        assert_eq!(load(&ca).unwrap().fingerprint_colons(), OPENSSL);
    }
}
