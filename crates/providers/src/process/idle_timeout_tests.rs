use super::{DEFAULT_IDLE_STDOUT_TIMEOUT, parse_idle_timeout};
use std::time::Duration;

#[test]
fn default_idle_timeout_is_fifteen_minutes() {
    assert_eq!(DEFAULT_IDLE_STDOUT_TIMEOUT, Duration::from_secs(900));
}

#[test]
fn parse_idle_timeout_falls_back_when_unset_or_invalid() {
    for raw in [
        None,
        Some(""),
        Some("   "),
        Some("not-a-number"),
        Some("-60"),
        Some("120.5"),
        Some("18446744073709551616"),
    ] {
        assert_eq!(
            parse_idle_timeout(raw),
            DEFAULT_IDLE_STDOUT_TIMEOUT,
            "expected default for {raw:?}"
        );
    }
}

#[test]
fn parse_idle_timeout_parses_and_clamps_range() {
    assert_eq!(parse_idle_timeout(Some("0")), Duration::from_secs(60));
    assert_eq!(parse_idle_timeout(Some("15")), Duration::from_secs(60));
    assert_eq!(parse_idle_timeout(Some("59")), Duration::from_secs(60));
    assert_eq!(parse_idle_timeout(Some("60")), Duration::from_secs(60));
    assert_eq!(parse_idle_timeout(Some("300")), Duration::from_secs(300));
    assert_eq!(
        parse_idle_timeout(Some(" 1200 \n")),
        Duration::from_secs(1200)
    );
    assert_eq!(parse_idle_timeout(Some("3600")), Duration::from_secs(3600));
    assert_eq!(parse_idle_timeout(Some("3601")), Duration::from_secs(3600));
    assert_eq!(parse_idle_timeout(Some("99999")), Duration::from_secs(3600));
}
