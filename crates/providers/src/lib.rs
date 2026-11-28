//! Local CLI providers and turn orchestration for t3code-rs.

mod catalog;
mod cli_drivers;
mod mock;
mod parsers;
mod process;
mod retry;
mod watchdog;

pub use catalog::context_window_for;
pub use cli_drivers::{AntigravityDriver, ClaudeDriver, CodexDriver, OpenCodeDriver};
pub use mock::MockDriver;
#[cfg(unix)]
pub use process::startup::with_spawn_hook;
pub use retry::{
    FailureClass, MAX_CAPACITY_RETRIES, MAX_RETRY_AFTER_SECS, classify_harness_failure,
    classify_provider_failure, classify_rate_limit, format_delegated_error, format_run_error,
    format_worker_error, is_hard_rate_limit, is_quota_exhausted, run_turn_with_retry,
};

use std::{collections::HashMap, sync::Arc};
use t3_core::{
    config::ServerConfig,
    traits::ProviderDriver,
    types::{ModelSpec, ProviderHealth, ProviderId},
};

/// The drivers enabled for this server, in configuration order.
#[derive(Clone)]
pub struct ProviderRegistry {
    drivers: HashMap<ProviderId, Arc<dyn ProviderDriver>>,
    order: Vec<ProviderId>,
}

impl ProviderRegistry {
    pub fn from_config(cfg: &ServerConfig) -> Self {
        let mut registry = Self {
            drivers: HashMap::new(),
            order: Vec::new(),
        };
        for &id in &cfg.enabled_providers {
            if registry.drivers.contains_key(&id) {
                continue;
            }
            let driver: Arc<dyn ProviderDriver> = match id {
                ProviderId::GeminiApp => continue, // The desktop bridge is supplied by the server.
                ProviderId::Claude => Arc::new(ClaudeDriver::new(cfg.providers.claude_bin.clone())),
                ProviderId::Codex => Arc::new(CodexDriver::new(cfg.providers.codex_bin.clone())),
                ProviderId::Antigravity => Arc::new(
                    AntigravityDriver::with_data_dir(
                        cfg.providers.antigravity_bin.clone(),
                        cfg.antigravity_dir(),
                    )
                    .with_mcp(cfg.antigravity_mcp_enabled),
                ),
                ProviderId::OpenCode => {
                    Arc::new(OpenCodeDriver::new(cfg.providers.opencode_bin.clone()))
                }
                ProviderId::Mock => Arc::new(MockDriver::new()),
            };
            registry.register(driver);
        }
        registry
    }

    pub fn register(&mut self, driver: Arc<dyn ProviderDriver>) {
        let id = driver.provider_id();
        if self.drivers.insert(id, driver).is_none() {
            self.order.push(id);
        }
    }

    pub fn get(&self, id: ProviderId) -> Option<Arc<dyn ProviderDriver>> {
        self.drivers.get(&id).cloned()
    }

    pub async fn health_all(&self) -> Vec<ProviderHealth> {
        let mut health = Vec::with_capacity(self.order.len());
        for id in &self.order {
            health.push(self.drivers[id].health_check().await);
        }
        health
    }

    pub fn default_models(provider: ProviderId) -> Vec<ModelSpec> {
        catalog::default_models(provider)
    }
}

#[cfg(test)]
fn test_request(provider: ProviderId) -> t3_core::traits::TurnRequest {
    t3_core::traits::TurnRequest {
        thread_id: uuid::Uuid::new_v4(),
        run_id: uuid::Uuid::new_v4(),
        workspace_dir: std::env::temp_dir(),
        prompt: "hello".into(),
        claude_slash_context: None,
        model_selection: t3_core::types::ModelSelection {
            provider,
            model: "test-model".into(),
            effort: None,
            agent: None,
        },
        runtime_mode: t3_core::types::RuntimeMode::ApprovalRequired,
        interaction_mode: t3_core::types::InteractionMode::Default,
        resume_session_id: None,
        attachments: Vec::new(),
        mcp_injection: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{ProviderId, ProviderRegistry, ServerConfig};
    use t3_core::types::ModelTier;

    #[tokio::test]
    async fn registry_uses_enabled_providers_and_configured_binary() {
        let mut config = ServerConfig {
            enabled_providers: vec![ProviderId::Mock, ProviderId::Claude, ProviderId::Mock],
            ..ServerConfig::default()
        };
        config.providers.claude_bin = "/nonexistent/t3-providers-test/claude".into();
        let registry = ProviderRegistry::from_config(&config);
        assert!(registry.get(ProviderId::Codex).is_none());
        assert_eq!(
            registry.get(ProviderId::Mock).unwrap().provider_id(),
            ProviderId::Mock
        );
        let health = registry.health_all().await;
        assert_eq!(health.len(), 2);
        assert!(health[0].available);
        assert!(!health[1].available);
        assert_eq!(
            health[1].binary_path.as_ref(),
            Some(&config.providers.claude_bin)
        );
    }

    #[test]
    fn curated_opencode_catalog_distinguishes_cloud_and_local() {
        let models = ProviderRegistry::default_models(ProviderId::OpenCode);
        let cloud = models
            .iter()
            .find(|m| m.id == "opencode/deepseek-v4.1-flash-free")
            .unwrap();
        assert_eq!(cloud.tier, ModelTier::Tier0FreeLocal);
        assert!(cloud.is_free);
        assert!(!cloud.is_local);
        assert!(
            models
                .iter()
                .any(|m| m.id == "local-uncensored/uncensored-4b" && m.is_local)
        );
    }
}
