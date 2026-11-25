#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_links_require_absolute_http_urls() {
        for url in ["https://example.com/path?q=1", "http://127.0.0.1:3774/"] {
            assert!(http_url(url).is_some(), "{url}");
        }
        for url in [
            "file:///tmp/example",
            "javascript:alert(1)",
            "-flag",
            "//example.com",
            "https:example.com",
            "https:///example.com",
            "https://",
            " https://example.com",
            "https://example.com/\npath",
        ] {
            assert!(http_url(url).is_none(), "{url}");
        }
    }

    #[test]
    fn navigation_checks_scheme_host_and_effective_port() {
        let app = Url::parse("http://127.0.0.1:3774/settings")
            .unwrap()
            .origin();
        assert_eq!(navigation("about:blank", &app), Navigation::Allow);
        assert_eq!(
            navigation("http://127.0.0.1:3774/thread#message", &app),
            Navigation::Allow
        );
        for raw in [
            "https://127.0.0.1:3774/",
            "http://127.0.0.1:3775/",
            "http://localhost:3774/",
            "http://127.0.0.1:3774@example.com/",
        ] {
            assert_eq!(
                navigation(raw, &app),
                Navigation::External(Url::parse(raw).unwrap())
            );
        }
        for raw in [
            "file:///tmp/index.html",
            "javascript:void(0)",
            "about:srcdoc",
        ] {
            assert_eq!(navigation(raw, &app), Navigation::Block);
        }

        let remote = Url::parse("https://example.com/").unwrap().origin();
        assert_eq!(
            navigation("https://EXAMPLE.com:443/thread", &remote),
            Navigation::Allow
        );
    }
}
use std::process::{Command, Stdio};

use url::{Origin, Url};

pub(super) fn http_url(raw: &str) -> Option<Url> {
    let authority = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))?;
    if authority.is_empty()
        || authority.starts_with(['/', '?', '#'])
        || raw.contains('\\')
        || raw.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return None;
    }

    Url::parse(raw).ok().filter(Url::has_host)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Navigation {
    Allow,
    External(Url),
    Block,
}

pub(super) fn navigation(raw: &str, app_origin: &Origin) -> Navigation {
    if raw == "about:blank" {
        return Navigation::Allow;
    }
    match http_url(raw) {
        Some(url) if url.origin() == *app_origin => Navigation::Allow,
        Some(url) => Navigation::External(url),
        None => Navigation::Block,
    }
}

pub(super) fn open_external(url: Url) {
    std::thread::spawn(move || {
        let result = Command::new("/usr/bin/open")
            .arg("--")
            .arg(url.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !matches!(result, Ok(status) if status.success()) {
            eprintln!("T3 Code RS: could not open the link in the system browser");
        }
    });
}
