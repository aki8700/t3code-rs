use crate::types::{FollowUpBehavior, ModelSelection, ProviderId};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Cloudflare Access auth requires non-empty team_domain and audience")]
    InvalidCloudflareAccess,
    #[error("Cloudflare Access JWKS requires HTTPS or an explicit HTTP loopback URL")]
    InvalidJwksUrl,
    #[error("Tailscale auth requires at least one explicit --tailscale-login")]
    EmptyTailscaleAllowlist,
    #[error("Bearer auth requires a non-empty token of at least 16 bytes")]
    InvalidBearerToken,
    #[error("Non-loopback bind ({host}) is not allowed with auth mode '{mode}'")]
    UnsafeBindForAuth { host: String, mode: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub enum AuthConfig {
    /// Only accepts connections from loopback (127.0.0.1 / ::1).
    #[default]
    LocalOnly,
    /// Requires `Authorization: Bearer <token>` or `?token=<token>` (for WS).
    Bearer { token: String },
    /// Validates `Tailscale-User-Login` header, strictly from loopback proxy only.
    Tailscale {
        #[serde(default)]
        allowed_logins: Vec<String>,
    },
    /// Validates Cloudflare Access RS256 JWT from `Cf-Access-Jwt-Assertion` header
    /// or `CF_Authorization` cookie using team JWKS endpoint.
    CloudflareAccess {
        /// e.g. "myteam.cloudflareaccess.com"
        team_domain: String,
        /// Cloudflare Access Application Audience (AUD) tag
        audience: String,
        /// Optional email allowlist; empty means any valid email in the Access policy
        #[serde(default)]
        allowed_emails: Vec<String>,
        /// Optional override JWKS URL (for tests / custom certs endpoint)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        jwks_url_override: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderBinaryConfig {
    pub claude_bin: PathBuf,
    pub codex_bin: PathBuf,
    pub antigravity_bin: PathBuf,
    pub opencode_bin: PathBuf,
    /// Isolated data directory for Antigravity ACP/CLI (never touches official ~/.t3)
    pub antigravity_data_dir: Option<PathBuf>,
    /// Optional override endpoints for Antigravity quota probe (primarily for tests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub antigravity_quota_endpoints: Option<Vec<String>>,
}

impl Default for ProviderBinaryConfig {
    fn default() -> Self {
        Self {
            claude_bin: PathBuf::from("/opt/homebrew/bin/claude"),
            codex_bin: PathBuf::from("/opt/homebrew/bin/codex"),
            antigravity_bin: PathBuf::from("/opt/homebrew/bin/agy"),
            opencode_bin: PathBuf::from("/opt/homebrew/bin/opencode"),
            antigravity_data_dir: None,
            antigravity_quota_endpoints: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Compress TurnItem JSON payloads larger than this threshold (bytes).
    pub zstd_threshold_bytes: usize,
    /// zstd compression level (1..=19, default 3 for fast laptop speed).
    pub zstd_level: i32,
    /// Hard truncate single raw tool output blobs above this size (bytes) before compression.
    pub max_payload_bytes: usize,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            zstd_threshold_bytes: 4096,
            zstd_level: 3,
            max_payload_bytes: 4 * 1024 * 1024,
        }
    }
}

fn default_verify_max_auto_fixes() -> u32 {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub data_dir: PathBuf,
    pub auth: AuthConfig,
    pub providers: ProviderBinaryConfig,
    pub storage: StorageConfig,
    pub default_model: ModelSelection,
    pub follow_up_behavior: FollowUpBehavior,
    pub auto_resume_limited_threads: bool,
    pub rate_limit_max_retries: u32,
    pub rate_limit_initial_backoff_secs: u64,
    pub enabled_providers: Vec<ProviderId>,
    /// Opt-in direct Antigravity Cloud Code quota query (`retrieveUserQuotaSummary`).
    #[serde(default)]
    pub antigravity_quota_probe: bool,
    /// Ordered fallback models to try after a hard rate limit or quota exhaustion.
    #[serde(default)]
    pub fallback_models: Vec<ModelSelection>,
    /// Maximum automatic fix attempts when a project's verify command fails.
    #[serde(default = "default_verify_max_auto_fixes")]
    pub verify_max_auto_fixes: u32,
    /// Opt-in per-run env injection for a pre-registered Antigravity (`agy`) MCP entry.
    #[serde(default)]
    pub antigravity_mcp_enabled: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        let data_dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".t3-rs");
        Self {
            host: "127.0.0.1".to_string(),
            port: 3774,
            data_dir,
            auth: AuthConfig::LocalOnly,
            providers: ProviderBinaryConfig::default(),
            storage: StorageConfig::default(),
            default_model: ModelSelection::default(),
            follow_up_behavior: FollowUpBehavior::Steer,
            auto_resume_limited_threads: true,
            rate_limit_max_retries: 3,
            rate_limit_initial_backoff_secs: 15,
            enabled_providers: vec![
                ProviderId::Claude,
                ProviderId::Codex,
                ProviderId::Antigravity,
                ProviderId::OpenCode,
            ],
            antigravity_quota_probe: false,
            fallback_models: Vec::new(),
            verify_max_auto_fixes: default_verify_max_auto_fixes(),
            antigravity_mcp_enabled: false,
        }
    }
}

impl ServerConfig {
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("state.sqlite")
    }

    pub fn attachments_dir(&self) -> PathBuf {
        self.data_dir.join("attachments")
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        self.data_dir.join("worktrees")
    }

    pub fn antigravity_dir(&self) -> PathBuf {
        self.providers
            .antigravity_data_dir
            .clone()
            .unwrap_or_else(|| self.data_dir.join("providers").join("antigravity"))
    }

    pub fn is_loopback_host(&self) -> bool {
        matches!(self.host.as_str(), "127.0.0.1" | "::1" | "localhost")
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        match &self.auth {
            AuthConfig::LocalOnly => {
                if !self.is_loopback_host() {
                    return Err(ConfigError::UnsafeBindForAuth {
                        host: self.host.clone(),
                        mode: "local-only".into(),
                    });
                }
            }
            AuthConfig::Tailscale { allowed_logins } => {
                if !self.is_loopback_host() {
                    return Err(ConfigError::UnsafeBindForAuth {
                        host: self.host.clone(),
                        mode: "tailscale".into(),
                    });
                }
                if allowed_logins.iter().all(|login| login.trim().is_empty()) {
                    return Err(ConfigError::EmptyTailscaleAllowlist);
                }
            }
            AuthConfig::Bearer { token } => {
                if token.len() < 16 {
                    return Err(ConfigError::InvalidBearerToken);
                }
            }
            AuthConfig::CloudflareAccess {
                team_domain,
                audience,
                jwks_url_override,
                ..
            } => {
                if team_domain.trim().is_empty() || audience.trim().is_empty() {
                    return Err(ConfigError::InvalidCloudflareAccess);
                }
                if let Some(url) = jwks_url_override {
                    parse_jwks_override(url)?;
                }
            }
        }
        Ok(())
    }

    /// Returns the exact set of outbound URL prefixes permitted by this configuration.
    /// Strict zero-telemetry invariant: empty unless Cloudflare Access auth or opt-in
    /// Antigravity quota probing is enabled.
    pub fn allowed_outbound_urls(&self) -> Vec<String> {
        let mut urls = match &self.auth {
            AuthConfig::CloudflareAccess {
                team_domain,
                jwks_url_override,
                ..
            } => {
                if let Some(override_url) = jwks_url_override {
                    vec![override_url.clone()]
                } else {
                    let domain = team_domain
                        .trim_start_matches("https://")
                        .trim_start_matches("http://")
                        .trim_end_matches('/');
                    vec![format!("https://{domain}/cdn-cgi/access/certs")]
                }
            }
            _ => Vec::new(),
        };
        if self.antigravity_quota_probe {
            let endpoints = self
                .providers
                .antigravity_quota_endpoints
                .as_deref()
                .unwrap_or(&[]);
            if endpoints.is_empty() {
                urls.push(
                    "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary"
                        .into(),
                );
                urls.push(
                    "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary"
                        .into(),
                );
            } else {
                for ep in endpoints {
                    urls.push(format!(
                        "{}/v1internal:retrieveUserQuotaSummary",
                        ep.trim_end_matches('/')
                    ));
                }
            }
        }
        urls
    }
}

pub fn parse_jwks_override(value: &str) -> Result<Url, ConfigError> {
    let url = Url::parse(value).map_err(|_| ConfigError::InvalidJwksUrl)?;
    if !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::InvalidJwksUrl);
    }
    if url.scheme() == "https" {
        return Ok(url);
    }
    // The URL parser normalizes alternate IPv4 spellings. Check the original authority.
    let literal_loopback = value.split_once("://").is_some_and(|(_, rest)| {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        ["localhost", "127.0.0.1", "[::1]"].iter().any(|host| {
            authority.eq_ignore_ascii_case(host)
                || authority.rsplit_once(':').is_some_and(|(name, port)| {
                    name.eq_ignore_ascii_case(host) && port.parse::<u16>().is_ok()
                })
        })
    });
    if url.scheme() == "http" && literal_loopback {
        Ok(url)
    } else {
        Err(ConfigError::InvalidJwksUrl)
    }
}

/// Create missing directories with access limited to the current user.
pub fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid_and_zero_outbound() {
        let cfg = ServerConfig::default();
        assert!(cfg.validate().is_ok());
        assert!(cfg.allowed_outbound_urls().is_empty());
    }

    #[test]
    fn tailscale_rejects_non_loopback_bind() {
        let cfg = ServerConfig {
            host: "0.0.0.0".into(),
            auth: AuthConfig::Tailscale {
                allowed_logins: vec![],
            },
            ..Default::default()
        };
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::UnsafeBindForAuth { .. })
        ));
    }

    #[test]
    fn cloudflare_access_allows_only_jwks_url() {
        let cfg = ServerConfig {
            auth: AuthConfig::CloudflareAccess {
                team_domain: "myteam.cloudflareaccess.com".into(),
                audience: "aud-123".into(),
                allowed_emails: vec!["me@example.com".into()],
                jwks_url_override: None,
            },
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
        assert_eq!(
            cfg.allowed_outbound_urls(),
            vec!["https://myteam.cloudflareaccess.com/cdn-cgi/access/certs"]
        );
    }

    #[test]
    fn jwks_override_requires_https_or_literal_http_loopback() {
        for (url, allowed) in [
            ("https://team.example.test/certs", true),
            ("http://localhost:8000/certs", true),
            ("http://127.0.0.1/certs", true),
            ("http://[::1]:8000/certs", true),
            ("http://team.example.test/certs", false),
            ("http://localhost.example.test/certs", false),
            ("http://127.0.0.2/certs", false),
            ("http://127.1/certs", false),
            ("http://localhost@example.test/certs", false),
            ("file:///certs", false),
        ] {
            let cfg = ServerConfig {
                auth: AuthConfig::CloudflareAccess {
                    team_domain: "team.example.test".into(),
                    audience: "fixture".into(),
                    allowed_emails: vec![],
                    jwks_url_override: Some(url.into()),
                },
                ..Default::default()
            };
            assert_eq!(cfg.validate().is_ok(), allowed, "{url}");
        }
    }

    #[test]
    fn omitted_tier4_fields_deserialize_to_defaults() {
        let mut json = serde_json::to_value(ServerConfig::default()).unwrap();
        let obj = json.as_object_mut().unwrap();
        obj.remove("fallback_models");
        obj.remove("verify_max_auto_fixes");
        obj.remove("antigravity_mcp_enabled");

        let parsed: ServerConfig = serde_json::from_value(json).unwrap();
        assert!(parsed.fallback_models.is_empty());
        assert_eq!(parsed.verify_max_auto_fixes, 2);
        assert!(!parsed.antigravity_mcp_enabled);

        let defaults = ServerConfig::default();
        assert!(defaults.fallback_models.is_empty());
        assert_eq!(defaults.verify_max_auto_fixes, 2);
        assert!(!defaults.antigravity_mcp_enabled);
    }
}
