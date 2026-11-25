use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Prices {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct CompactionEconInput {
    pub prefix: u64,
    pub replaced: u64,
    pub summary: u64,
    pub tail: u64,
    pub instruction: u64,
    pub summarizer_reuses_prefix: bool,
    pub prices: Prices,
    pub remaining_turns: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct CompactionEconResult {
    pub one_time_cost: f64,
    pub saving_per_turn: f64,
    pub break_even_turns: Option<f64>,
    pub worth_compacting: Option<bool>,
}

/// Apply the Maki prompt-cache cost model using a common unit for all prices.
pub fn compaction_econ(input: CompactionEconInput) -> CompactionEconResult {
    let prices = input.prices;
    let summary_read_price = if input.summarizer_reuses_prefix {
        prices.cache_read
    } else {
        prices.input
    };
    let rewrite_price = if prices.cache_write > 0.0 {
        prices.cache_write
    } else {
        prices.input
    };
    let summary_cost = (input.prefix as f64 + input.replaced as f64) * summary_read_price
        + input.instruction as f64 * prices.input
        + input.summary as f64 * prices.output;
    let rewrite_cost =
        (input.summary as f64 + input.tail as f64) * (rewrite_price - prices.cache_read).max(0.0);
    let one_time_cost = summary_cost + rewrite_cost;
    let saving_per_turn = (input.replaced as f64 - input.summary as f64) * prices.cache_read;
    let break_even_turns = (saving_per_turn > 0.0).then(|| one_time_cost / saving_per_turn);
    let worth_compacting = input
        .remaining_turns
        .map(|remaining| break_even_turns.is_some_and(|turns| turns <= remaining));

    CompactionEconResult {
        one_time_cost,
        saving_per_turn,
        break_even_turns,
        worth_compacting,
    }
}
