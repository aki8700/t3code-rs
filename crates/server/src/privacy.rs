use crate::auth::cloudflare_endpoint;
use t3_core::{AuthConfig, PrivacyAuditReport, ServerConfig, TELEMETRY_ENABLED};

pub fn audit_privacy(config: &ServerConfig) -> PrivacyAuditReport {
    let mut notes = Vec::new();
    if let Err(error) = config.validate() {
        notes.push(error.to_string());
    }
    let auth_mode = match &config.auth {
        AuthConfig::LocalOnly => "local-only",
        AuthConfig::Bearer { .. } => "bearer",
        AuthConfig::Tailscale { .. } => "tailscale",
        AuthConfig::CloudflareAccess {
            team_domain,
            jwks_url_override,
            ..
        } => {
            if let Err(error) = cloudflare_endpoint(team_domain, jwks_url_override.as_deref()) {
                notes.push(error.to_string());
            }
            "cloudflare-access"
        }
    };

    PrivacyAuditReport {
        telemetry_enabled: TELEMETRY_ENABLED,
        reads_provider_credentials: false,
        allowed_outbound_urls: config.allowed_outbound_urls(),
        auth_mode: auth_mode.into(),
        bind_host: config.host.clone(),
        passed: notes.is_empty() && !TELEMETRY_ENABLED,
        notes,
    }
}
