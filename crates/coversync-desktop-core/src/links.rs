//! Links a page opens in a new window (`target="_blank"`): the apps have one
//! window, so the default browser opens them instead.

use url::Url;

/// `uri`, if it's one to hand to the default browser: http and https only,
/// so a page can't make the system open a file or another app's scheme.
pub fn for_browser(uri: &str) -> Option<Url> {
    let url = Url::parse(uri).ok()?;
    matches!(url.scheme(), "http" | "https").then_some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_links_go_to_the_browser() {
        let open = |uri| for_browser(uri).map(String::from);
        assert_eq!(
            open("https://musicbrainz.org/release/x").as_deref(),
            Some("https://musicbrainz.org/release/x")
        );
        assert_eq!(
            open("http://acoustid.org/new-application").as_deref(),
            Some("http://acoustid.org/new-application")
        );
        for refused in [
            "file:///etc/passwd",
            "coversync://app/",
            // A pairing link: the desktop would hand it back to the app.
            "coversync://pair?server=https%3A%2F%2Fevil.example%2F&code=K7QM4ZTD",
            "mailto:someone@example.com",
            "steam://run/1",
            "javascript:1",
            "about:blank",
            "",
        ] {
            assert_eq!(open(refused), None, "{refused}");
        }
    }
}
