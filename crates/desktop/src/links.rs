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
    fn app_url_requires_https_or_explicit_http_loopback() {
        for raw in [
            "https://example.com/path",
            "http://127.0.0.1:3774",
            "http://localhost",
            "http://localhost:3774/path",
        ] {
            assert!(app_url(raw).is_some(), "{raw}");
        }
        for raw in [
            "http://example.com",
            "http://localhost.example.com",
            "http://localhost@example.com",
            "http://user@localhost",
            "http://localhost:password@127.0.0.1",
            "http://localhost:/path",
            "http://127.1:3774",
            "http://[::1]:3774",
        ] {
            assert!(app_url(raw).is_none(), "{raw}");
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
use std::{
    process::{Command, Stdio},
    sync::{OnceLock, mpsc},
};

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

pub(super) fn app_url(raw: &str) -> Option<Url> {
    let url = http_url(raw)?;
    if url.scheme() == "https" {
        return Some(url);
    }

    // Check the original authority before URL parsing can normalize IPv4 aliases.
    let authority = raw.strip_prefix("http://")?.split(['/', '?', '#']).next()?;
    let host = match authority.split_once(':') {
        Some((host, port))
            if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            host
        }
        Some(_) => return None,
        None => authority,
    };
    (matches!(url.host_str(), Some("127.0.0.1" | "localhost"))
        && (host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost")))
    .then_some(url)
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
    if !matches!(url.scheme(), "http" | "https") {
        return;
    }
    static OPENER: OnceLock<mpsc::SyncSender<Url>> = OnceLock::new();
    let sender = OPENER.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel::<Url>(1);
        std::thread::spawn(move || {
            for url in receiver {
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
            }
        });
        sender
    });
    // Keep one pending request while the worker opens a link.
    let _ = sender.try_send(url);
}
