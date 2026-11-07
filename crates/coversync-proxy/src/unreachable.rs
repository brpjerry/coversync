//! Why a server couldn't be reached, in words for people.

use std::error::Error as StdError;
use std::fmt;
use std::io;

use axum::http::StatusCode;
use rustls::CertificateError;

use crate::upstream::Upstream;

/// The server couldn't be reached, or answered the way a reverse proxy does
/// when the server behind it is down (502, 503 or 504).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unreachable {
    /// The server's URL.
    pub server: String,
    /// What went wrong, for people: `The connection was refused: nothing is
    /// listening at that address.`
    pub reason: String,
    /// The error underneath, when it says more than `reason` does: a name
    /// lookup's or a TLS library's own words.
    pub detail: Option<String>,
}

impl fmt::Display for Unreachable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Can't reach {}. {}", self.server, self.reason)
    }
}

impl StdError for Unreachable {}

impl Unreachable {
    pub(crate) fn new(upstream: &Upstream, reason: impl Into<String>) -> Self {
        Self {
            server: upstream.url(),
            reason: reason.into(),
            detail: None,
        }
    }

    pub(crate) fn from_error(upstream: &Upstream, err: &(dyn StdError + 'static)) -> Self {
        let (reason, detail) = describe(err);
        Self {
            detail,
            ..Self::new(upstream, reason)
        }
    }

    pub(crate) fn from_status(upstream: &Upstream, status: StatusCode) -> Option<Self> {
        matches!(
            status,
            StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
        )
        .then(|| {
            Self::new(
                upstream,
                format!("It answered {status}, so coversync behind it may be stopped."),
            )
        })
    }
}

/// Every error in the chain. io::Error's `source()` skips the error it
/// wraps, which is where rustls's errors are.
fn causes<'a>(err: &'a (dyn StdError + 'static)) -> Vec<&'a (dyn StdError + 'static)> {
    let mut causes = Vec::new();
    let mut next = Some(err);
    while let Some(err) = next {
        causes.push(err);
        next = match err.downcast_ref::<io::Error>().and_then(io::Error::get_ref) {
            Some(inner) => Some(inner),
            None => err.source(),
        };
    }
    causes
}

/// A sentence for people, and the innermost error's own words when they add
/// something.
fn describe(err: &(dyn StdError + 'static)) -> (String, Option<String>) {
    let causes = causes(err);
    let raw = causes.last().map(|err| err.to_string());
    let with_raw = |reason: &str| (reason.to_owned(), raw.clone());
    let alone = |reason: &str| (reason.to_owned(), None);

    if let Some(tls) = causes
        .iter()
        .find_map(|e| e.downcast_ref::<rustls::Error>())
    {
        return match tls {
            rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer) => with_raw(
                "The system doesn't trust its certificate. Install the CA that signed it in \
                 the system's certificate store.",
            ),
            rustls::Error::InvalidCertificate(
                CertificateError::Expired | CertificateError::ExpiredContext { .. },
            ) => with_raw("Its certificate has expired."),
            rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            ) => with_raw("Its certificate is for another name."),
            rustls::Error::InvalidCertificate(_) => with_raw("Its certificate isn't valid."),
            _ => with_raw("The secure connection failed."),
        };
    }
    // hyper-util's name for a failed name lookup.
    if causes.iter().any(|e| e.to_string() == "dns error") {
        return with_raw("No server by that name was found.");
    }
    let kind = causes
        .iter()
        .find_map(|e| e.downcast_ref::<io::Error>())
        .map(io::Error::kind);
    match kind {
        Some(io::ErrorKind::ConnectionRefused) => {
            alone("The connection was refused: nothing is listening at that address.")
        }
        Some(io::ErrorKind::TimedOut) => alone("It didn't answer in time."),
        Some(io::ErrorKind::NetworkUnreachable | io::ErrorKind::HostUnreachable) => {
            alone("The network has no way to it.")
        }
        Some(
            io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof,
        ) => with_raw("The connection closed before it answered."),
        _ => with_raw("The connection failed."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Named(&'static str, Option<io::Error>);

    impl fmt::Display for Named {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }

    impl StdError for Named {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            self.1.as_ref().map(|e| e as _)
        }
    }

    #[test]
    fn says_what_went_wrong() {
        let refused = io::Error::from(io::ErrorKind::ConnectionRefused);
        let (reason, detail) = describe(&Named("tcp connect error", Some(refused)));
        assert!(reason.starts_with("The connection was refused"));
        assert_eq!(detail, None, "the OS's words add nothing");

        let lookup =
            io::Error::other("failed to lookup address information: Name or service not known");
        let (reason, detail) = describe(&Named("dns error", Some(lookup)));
        assert_eq!(reason, "No server by that name was found.");
        assert_eq!(
            detail.as_deref(),
            Some("failed to lookup address information: Name or service not known")
        );

        let timeout = io::Error::new(io::ErrorKind::TimedOut, "deadline has elapsed");
        assert_eq!(describe(&timeout).0, "It didn't answer in time.");
    }

    #[test]
    fn explains_certificates() {
        // As hyper-rustls hands rustls's error on: an io::Error inside another.
        let tls = |err: CertificateError| {
            io::Error::other(io::Error::new(
                io::ErrorKind::InvalidData,
                rustls::Error::InvalidCertificate(err),
            ))
        };
        let (reason, detail) = describe(&tls(CertificateError::UnknownIssuer));
        assert!(reason.contains("certificate store"), "{reason}");
        assert_eq!(
            detail.as_deref(),
            Some("invalid peer certificate: UnknownIssuer")
        );
        assert_eq!(
            describe(&tls(CertificateError::Expired)).0,
            "Its certificate has expired."
        );
        assert_eq!(
            describe(&tls(CertificateError::NotValidForName)).0,
            "Its certificate is for another name."
        );
    }
}
