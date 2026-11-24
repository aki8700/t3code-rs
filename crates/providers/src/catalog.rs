use t3_core::types::{ModelSpec, ModelTier, ProviderId};

pub(crate) fn default_models(provider: ProviderId) -> Vec<ModelSpec> {
    use ModelTier::*;
    use ProviderId::*;

    let entries: &[(&str, ModelTier)] = match provider {
        Claude => &[
            ("claude-opus-5-5", Tier3Arch),
            ("claude-sonnet-5-5", Tier2Review),
            ("claude-haiku-5-5", Tier1Fast),
        ],
        Codex => &[
            ("gpt-6-luna", Tier1Fast),
            ("gpt-6.2-sol", Tier2Review),
            ("gpt-6.1-terra", Tier2Review),
            ("gpt-6.2-astra", Tier3Arch),
        ],
        Antigravity => &[
            ("gemini-4-hydrogen", Tier1Fast),
            ("gemini-4-argon", Tier2Review),
            ("gemini-4-neon", Tier3Arch),
        ],
        OpenCode => &[
            ("opencode/deepseek-v4.1-flash-free", Tier0FreeLocal),
            ("opencode/flint-40b-free", Tier0FreeLocal),
            ("opencode/beam-free", Tier0FreeLocal),
            ("local/gemma4-31b", Tier0FreeLocal),
            ("win10-ds4p/ds4p-9b-heretic", Tier0FreeLocal),
            ("local-uncensored/uncensored-4b", Tier0FreeLocal),
        ],
        Mock => &[("mock-fast", Tier0FreeLocal)],
    };
    entries
        .iter()
        .map(|&(id, tier)| {
            let mut spec = model(provider, id, tier);
            spec.is_free = tier == Tier0FreeLocal;
            spec.is_local = spec.is_free && !id.starts_with("opencode/");
            spec
        })
        .collect()
}

fn model(provider: ProviderId, id: &str, tier: ModelTier) -> ModelSpec {
    let effort_levels: &[&str] = match provider {
        ProviderId::Claude | ProviderId::Antigravity => &["low", "medium", "high", "xhigh", "max"],
        ProviderId::Codex => &["low", "medium", "high", "xhigh"],
        ProviderId::OpenCode => &["low", "medium", "high", "max"],
        ProviderId::Mock => &[],
    };
    ModelSpec {
        provider,
        id: id.to_owned(),
        label: id.to_owned(),
        tier,
        effort_levels: effort_levels
            .iter()
            .map(|level| (*level).to_owned())
            .collect(),
        default_effort: None,
        is_local: false,
        is_free: false,
    }
}
