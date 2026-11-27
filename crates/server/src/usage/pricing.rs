use serde::Serialize;
use t3_core::ProviderId;

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub(super) struct Tokens {
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) cache_read_tokens: u64,
    pub(super) cache_write_tokens: u64,
    pub(super) reasoning_tokens: u64,
}

impl Tokens {
    pub(super) fn total(self) -> u64 {
        // Codex reasoning is a subset of output tokens.
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }

    pub(super) fn add(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
    }

    pub(super) fn estimated_cost(self, provider: ProviderId, model: &str) -> f64 {
        let Some([input, output, read, write]) = model_rates_per_million(provider, model) else {
            return 0.0;
        };
        (self.input_tokens as f64 * input
            + self.output_tokens as f64 * output
            + self.cache_read_tokens as f64 * read
            + self.cache_write_tokens as f64 * write)
            / 1_000_000.0
    }
}

/// Per-million-token USD rates `[input, output, cache_read, cache_write]`, or `None` for free/unknown models.
pub(crate) fn model_rates_per_million(provider: ProviderId, model: &str) -> Option<[f64; 4]> {
    let model = model.to_ascii_lowercase();
    match provider {
        ProviderId::Claude if model.contains("opus") => Some([15.0, 75.0, 1.5, 18.75]),
        ProviderId::Claude if model.contains("sonnet") => Some([3.0, 15.0, 0.30, 3.75]),
        ProviderId::Claude if model.contains("haiku") => Some([0.80, 4.0, 0.08, 1.0]),
        ProviderId::Codex if model.contains("astra") => Some([10.0, 40.0, 1.0, 10.0]),
        ProviderId::Codex if model.contains("sol") => Some([2.50, 15.0, 0.25, 2.50]),
        ProviderId::Codex if model.contains("terra") => Some([1.75, 14.0, 0.175, 1.75]),
        ProviderId::Codex if model.contains("luna") => Some([0.60, 2.40, 0.06, 0.60]),
        ProviderId::Antigravity if model.contains("neon") => Some([2.50, 15.0, 0.25, 2.50]),
        ProviderId::Antigravity if model.contains("argon") => Some([1.25, 10.0, 0.125, 1.25]),
        ProviderId::Antigravity if model.contains("hydrogen") || model.contains("flash") => {
            Some([0.30, 2.50, 0.03, 0.30])
        }
        _ => None,
    }
}

/// Estimated USD cost of one token snapshot, using the same price table as the usage dashboard.
pub(crate) fn estimate_tokens_cost(
    provider: ProviderId,
    model: &str,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
) -> f64 {
    Tokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        reasoning_tokens: 0,
    }
    .estimated_cost(provider, model)
}
