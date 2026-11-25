use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use t3_core::{
    CoreError, CoreResult, ProviderId, RuntimeSettings, RuntimeSettingsPatch, ServerConfig,
    create_private_dir,
};
use tracing::warn;
use uuid::Uuid;

/// Thread-safe store for live-mutable runtime settings with optional atomic JSON persistence
/// (`data_dir/runtime_settings.json`, mode 0600 on Unix) so UI updates survive restarts without
/// overwriting or stripping comments from the user's `config.toml`.
#[derive(Clone)]
pub(crate) struct SettingsStore {
    inner: Arc<RwLock<RuntimeSettings>>,
    persist_path: Option<PathBuf>,
}

impl SettingsStore {
    /// Initializes from `cfg` and overlays any valid persisted `runtime_settings.json` in
    /// `cfg.data_dir`. Corrupt, unreadable, or invalid persisted files are logged and ignored so
    /// startup is never blocked.
    pub(crate) fn load_or_init(cfg: &ServerConfig) -> Self {
        let persist_path = cfg.runtime_settings_path();
        let mut settings = cfg.runtime_settings();
        if let Some(patch) = read_persisted_patch(&persist_path) {
            let mut candidate = settings.clone();
            if let Err(err) = candidate.apply_patch(patch) {
                warn!(
                    path = %persist_path.display(),
                    error = %err,
                    "ignoring invalid persisted runtime_settings.json"
                );
            } else if let Err(err) = validate_settings_providers(&candidate, &cfg.enabled_providers)
            {
                warn!(
                    path = %persist_path.display(),
                    error = %err,
                    "ignoring persisted runtime_settings.json with disabled provider"
                );
            } else {
                settings = candidate;
            }
        }
        Self {
            inner: Arc::new(RwLock::new(settings)),
            persist_path: Some(persist_path),
        }
    }

    /// Creates a non-persisted store (primarily for tests).
    #[allow(dead_code)]
    pub(crate) fn in_memory(initial: RuntimeSettings) -> Self {
        Self {
            inner: Arc::new(RwLock::new(initial)),
            persist_path: None,
        }
    }

    pub(crate) fn get(&self) -> RuntimeSettings {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Validates `patch` (including checking that any referenced providers are in
    /// `enabled_providers`), atomically persists the updated settings if a persist path is
    /// configured, and publishes the new snapshot in memory.
    pub(crate) fn patch(
        &self,
        patch: RuntimeSettingsPatch,
        enabled_providers: &[ProviderId],
    ) -> CoreResult<RuntimeSettings> {
        validate_patch_providers(&patch, enabled_providers)?;
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut next = guard.clone();
        next.apply_patch(patch)
            .map_err(|err| CoreError::InvalidArgument(err.to_string()))?;
        if let Some(path) = &self.persist_path {
            persist_atomic(path, &next)?;
        }
        *guard = next.clone();
        Ok(next)
    }
}

fn validate_patch_providers(
    patch: &RuntimeSettingsPatch,
    enabled_providers: &[ProviderId],
) -> CoreResult<()> {
    if let Some(sel) = &patch.default_model
        && !enabled_providers.contains(&sel.provider)
    {
        return Err(CoreError::InvalidArgument(format!(
            "provider '{}' is not enabled",
            sel.provider.as_str()
        )));
    }
    if let Some(fallbacks) = &patch.fallback_models {
        for sel in fallbacks {
            if !enabled_providers.contains(&sel.provider) {
                return Err(CoreError::InvalidArgument(format!(
                    "fallback provider '{}' is not enabled",
                    sel.provider.as_str()
                )));
            }
        }
    }
    Ok(())
}

fn validate_settings_providers(
    settings: &RuntimeSettings,
    enabled_providers: &[ProviderId],
) -> CoreResult<()> {
    if !enabled_providers.contains(&settings.default_model.provider) {
        return Err(CoreError::InvalidArgument(format!(
            "provider '{}' is not enabled",
            settings.default_model.provider.as_str()
        )));
    }
    for sel in &settings.fallback_models {
        if !enabled_providers.contains(&sel.provider) {
            return Err(CoreError::InvalidArgument(format!(
                "fallback provider '{}' is not enabled",
                sel.provider.as_str()
            )));
        }
    }
    Ok(())
}

fn read_persisted_patch(path: &Path) -> Option<RuntimeSettingsPatch> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            warn!(path = %path.display(), error = %err, "failed to stat runtime_settings.json");
            return None;
        }
    };
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        warn!(
            path = %path.display(),
            "refusing to read non-regular or symlink runtime_settings.json"
        );
        return None;
    }
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) => {
            warn!(path = %path.display(), error = %err, "failed to read runtime_settings.json");
            return None;
        }
    };
    match serde_json::from_str::<RuntimeSettingsPatch>(&raw) {
        Ok(patch) => Some(patch),
        Err(err) => {
            warn!(path = %path.display(), error = %err, "failed to parse runtime_settings.json");
            None
        }
    }
}

fn persist_atomic(path: &Path, settings: &RuntimeSettings) -> CoreResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| CoreError::Storage("runtime_settings path has no parent".into()))?;
    create_private_dir(parent).map_err(|err| CoreError::Storage(err.to_string()))?;

    if let Ok(meta) = fs::symlink_metadata(path)
        && (meta.file_type().is_symlink() || !meta.file_type().is_file())
    {
        return Err(CoreError::Storage(
            "refusing to overwrite non-regular or symlink runtime_settings.json".into(),
        ));
    }

    let mut payload =
        serde_json::to_vec_pretty(settings).map_err(|err| CoreError::Internal(err.to_string()))?;
    payload.push(b'\n');

    let tmp_name = format!(".runtime_settings.{}.tmp", Uuid::new_v4());
    let tmp_path = parent.join(tmp_name);

    let write_result = (|| -> std::io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp_path)?;
        file.write_all(&payload)?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(err) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(CoreError::Storage(err.to_string()));
    }

    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(CoreError::Storage(err.to_string()));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use t3_core::{FollowUpBehavior, ModelSelection};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("t3-settings-test-{label}-{}", Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_config(data_dir: PathBuf) -> ServerConfig {
        ServerConfig {
            data_dir,
            ..Default::default()
        }
    }

    #[test]
    fn load_or_init_uses_config_defaults_when_file_missing() {
        let dir = TempDir::new("missing");
        let mut cfg = test_config(dir.path().to_path_buf());
        cfg.verify_max_auto_fixes = 4;
        cfg.antigravity_quota_probe = true;

        let store = SettingsStore::load_or_init(&cfg);
        let current = store.get();
        assert_eq!(current, cfg.runtime_settings());
        assert_eq!(current.verify_max_auto_fixes, 4);
        assert!(current.antigravity_quota_probe);
        assert!(!cfg.runtime_settings_path().exists());
    }

    #[test]
    fn patch_persists_with_0600_mode_and_survives_restart() {
        let dir = TempDir::new("persist");
        let cfg = test_config(dir.path().to_path_buf());
        let store = SettingsStore::load_or_init(&cfg);

        let updated = store
            .patch(
                RuntimeSettingsPatch {
                    default_model: Some(ModelSelection {
                        provider: ProviderId::Codex,
                        model: "gpt-6.2-sol".into(),
                        effort: Some("medium".into()),
                        agent: None,
                    }),
                    follow_up_behavior: Some(FollowUpBehavior::Queue),
                    auto_resume_limited_threads: Some(false),
                    rate_limit_max_retries: Some(5),
                    rate_limit_initial_backoff_secs: Some(30),
                    fallback_models: Some(vec![ModelSelection {
                        provider: ProviderId::Antigravity,
                        model: "gemini-4-neon".into(),
                        effort: None,
                        agent: None,
                    }]),
                    verify_max_auto_fixes: Some(7),
                    antigravity_mcp_enabled: Some(true),
                    antigravity_quota_probe: Some(true),
                },
                &cfg.enabled_providers,
            )
            .unwrap();

        assert_eq!(store.get(), updated);
        let path = cfg.runtime_settings_path();
        assert!(path.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        // Reloading from disk recovers the persisted settings.
        let reloaded = SettingsStore::load_or_init(&cfg);
        assert_eq!(reloaded.get(), updated);

        // No temporary files remain in data_dir.
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["runtime_settings.json"]);
    }

    #[test]
    fn patch_rejects_disabled_provider_without_mutating_or_persisting() {
        let dir = TempDir::new("disabled-provider");
        let mut cfg = test_config(dir.path().to_path_buf());
        cfg.enabled_providers = vec![ProviderId::Claude];
        let store = SettingsStore::load_or_init(&cfg);
        let before = store.get();

        let err = store
            .patch(
                RuntimeSettingsPatch {
                    default_model: Some(ModelSelection {
                        provider: ProviderId::Codex,
                        model: "gpt-6.2-sol".into(),
                        effort: None,
                        agent: None,
                    }),
                    verify_max_auto_fixes: Some(6),
                    ..Default::default()
                },
                &cfg.enabled_providers,
            )
            .unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)));
        assert_eq!(store.get(), before);
        assert!(!cfg.runtime_settings_path().exists());

        // Fallback provider must also be enabled.
        let err = store
            .patch(
                RuntimeSettingsPatch {
                    fallback_models: Some(vec![ModelSelection {
                        provider: ProviderId::OpenCode,
                        model: "local/qwen".into(),
                        effort: None,
                        agent: None,
                    }]),
                    ..Default::default()
                },
                &cfg.enabled_providers,
            )
            .unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)));
        assert_eq!(store.get(), before);
    }

    #[test]
    fn patch_rejects_invalid_bounds_and_empty_model_atomically() {
        let dir = TempDir::new("invalid-bounds");
        let cfg = test_config(dir.path().to_path_buf());
        let store = SettingsStore::load_or_init(&cfg);
        let before = store.get();

        let err = store
            .patch(
                RuntimeSettingsPatch {
                    verify_max_auto_fixes: Some(11),
                    antigravity_mcp_enabled: Some(true),
                    ..Default::default()
                },
                &cfg.enabled_providers,
            )
            .unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)));
        assert_eq!(store.get(), before);
        assert!(!cfg.runtime_settings_path().exists());

        let err = store
            .patch(
                RuntimeSettingsPatch {
                    default_model: Some(ModelSelection {
                        provider: ProviderId::Claude,
                        model: "   ".into(),
                        effort: None,
                        agent: None,
                    }),
                    ..Default::default()
                },
                &cfg.enabled_providers,
            )
            .unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument(_)));
        assert_eq!(store.get(), before);
    }

    #[test]
    fn load_or_init_ignores_corrupt_or_invalid_persisted_file() {
        let dir = TempDir::new("corrupt");
        let cfg = test_config(dir.path().to_path_buf());

        fs::write(cfg.runtime_settings_path(), "{not valid json").unwrap();
        let store = SettingsStore::load_or_init(&cfg);
        assert_eq!(store.get(), cfg.runtime_settings());

        // Valid JSON but out-of-range field is also ignored.
        fs::write(
            cfg.runtime_settings_path(),
            r#"{"verify_max_auto_fixes": 99}"#,
        )
        .unwrap();
        let store = SettingsStore::load_or_init(&cfg);
        assert_eq!(store.get(), cfg.runtime_settings());
    }

    #[test]
    fn load_or_init_overlays_partial_persisted_json_on_config() {
        let dir = TempDir::new("partial");
        let mut cfg = test_config(dir.path().to_path_buf());
        cfg.rate_limit_max_retries = 6;
        fs::write(
            cfg.runtime_settings_path(),
            r#"{"antigravity_mcp_enabled": true}"#,
        )
        .unwrap();

        let store = SettingsStore::load_or_init(&cfg);
        let current = store.get();
        assert!(current.antigravity_mcp_enabled);
        assert_eq!(current.rate_limit_max_retries, 6);
    }

    #[test]
    fn in_memory_store_patches_without_disk_io() {
        let store = SettingsStore::in_memory(RuntimeSettings::default());
        let updated = store
            .patch(
                RuntimeSettingsPatch {
                    rate_limit_max_retries: Some(8),
                    ..Default::default()
                },
                &[ProviderId::Claude],
            )
            .unwrap();
        assert_eq!(updated.rate_limit_max_retries, 8);
        assert_eq!(store.get(), updated);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_runtime_settings_file() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new("symlink");
        let cfg = test_config(dir.path().to_path_buf());
        let target = dir.path().join("outside.json");
        fs::write(&target, r#"{"antigravity_quota_probe": true}"#).unwrap();
        symlink(&target, cfg.runtime_settings_path()).unwrap();

        // Load ignores symlink.
        let store = SettingsStore::load_or_init(&cfg);
        assert!(!store.get().antigravity_quota_probe);

        // Patch refuses to overwrite symlink.
        let err = store
            .patch(
                RuntimeSettingsPatch {
                    verify_max_auto_fixes: Some(3),
                    ..Default::default()
                },
                &cfg.enabled_providers,
            )
            .unwrap_err();
        assert!(matches!(err, CoreError::Storage(_)));
        assert_eq!(store.get().verify_max_auto_fixes, 2);
    }
}
