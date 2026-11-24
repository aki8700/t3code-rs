use serde::{Deserialize, Serialize};

/// Compile-time & runtime privacy invariants for `t3code-rs`.
pub const TELEMETRY_ENABLED: bool = false;

/// Paths and domains that `t3code-rs` MUST NEVER read or contact.
/// Used by `t3d audit-privacy` and automated privacy tests.
pub const FORBIDDEN_CREDENTIAL_PATHS: &[&str] = &[
    ".codex/auth.json",
    ".claude.json",
    ".t3/userdata/clerk",
    "clerk-tokens.json",
    "anonymous-id",
];

pub const FORBIDDEN_TELEMETRY_DOMAINS: &[&str] = &[
    "posthog.com",
    "t3.tools",
    "ping.gg",
    "clerk.accounts.dev",
    "clerk.com",
    "ingest.sentry.io",
    "segment.io",
    "mixpanel.com",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivacyAuditReport {
    pub telemetry_enabled: bool,
    pub reads_provider_credentials: bool,
    pub allowed_outbound_urls: Vec<String>,
    pub auth_mode: String,
    pub bind_host: String,
    pub passed: bool,
    pub notes: Vec<String>,
}
