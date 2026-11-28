use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Duration, SecondsFormat, Timelike, Utc};
use t3_core::ProviderId;

use super::{
    Sample, Totals, UsageBucket, UsageModelRow, UsageProviderRow, UsageResponse, UsageWindow,
    cache::CachedFile, local_runs::LocalRun, pricing::Tokens,
};

#[derive(Default)]
struct Rollup {
    cost: f64,
    tokens: Tokens,
    sessions: HashSet<usize>,
    requests: u64,
}

impl Rollup {
    fn record(&mut self, sample: &Sample, session: usize, cost: f64) {
        self.cost += cost;
        self.tokens.add(sample.tokens);
        self.sessions.insert(session);
        self.requests += 1;
    }

    fn totals(&self) -> Totals {
        Totals {
            cost_usd: self.cost,
            total_tokens: self.tokens.total(),
            tokens: self.tokens,
            sessions: self.sessions.len(),
            requests: self.requests,
        }
    }
}

fn provider_info(provider: ProviderId) -> (&'static str, &'static str, &'static str) {
    match provider {
        ProviderId::Claude => ("claude", "Claude Code", "#d97757"),
        ProviderId::Codex => ("codex", "Codex", "var(--text)"),
        ProviderId::Antigravity => ("antigravity", "Antigravity", "#8c7bd1"),
        ProviderId::OpenCode => ("open-code", "OpenCode", "#5b9bbd"),
        ProviderId::GeminiApp => ("gemini-app", "Gemini for Mac", "#4285f4"),
        ProviderId::Mock => ("mock", "Mock", "#808080"),
    }
}

struct UsageAggregation<'a> {
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    step: Duration,
    totals: Rollup,
    providers: HashMap<ProviderId, Rollup>,
    models: HashMap<(ProviderId, &'a str), Rollup>,
    buckets: Vec<UsageBucket>,
}

impl<'a> UsageAggregation<'a> {
    fn record(&mut self, provider: ProviderId, session: usize, sample: &'a Sample) {
        if sample.at < self.since || sample.at > self.until {
            return;
        }
        let index = (sample.at - self.since).num_seconds() / self.step.num_seconds();
        let index = (index as usize).min(self.buckets.len() - 1);
        let bucket = &mut self.buckets[index];
        let cost = sample.tokens.estimated_cost(provider, &sample.model);
        self.totals.record(sample, session, cost);
        self.providers
            .entry(provider)
            .or_default()
            .record(sample, session, cost);
        self.models
            .entry((provider, &sample.model))
            .or_default()
            .record(sample, session, cost);
        let provider = bucket
            .by_provider
            .entry(provider_info(provider).0)
            .or_default();
        let tokens = sample.tokens.total();
        provider.cost_usd += cost;
        provider.total_tokens = provider.total_tokens.saturating_add(tokens);
        bucket.total_cost_usd += cost;
        bucket.total_tokens = bucket.total_tokens.saturating_add(tokens);
    }
}

pub(super) fn aggregate<'a>(
    days: u32,
    offset: i32,
    until: DateTime<Utc>,
    files: impl Iterator<Item = &'a CachedFile>,
    runs: &'a [LocalRun],
) -> UsageResponse {
    let hourly = days == 1;
    let offset = Duration::minutes(i64::from(offset));
    let local_now = (until + offset).naive_utc();
    let (since, step, count, resolution) = if hourly {
        let hour = local_now
            .date()
            .and_hms_opt(local_now.hour(), 0, 0)
            .expect("hour is valid")
            .and_utc()
            - offset;
        (hour - Duration::hours(23), Duration::hours(1), 24, "hour")
    } else {
        let midnight = local_now
            .date()
            .and_hms_opt(0, 0, 0)
            .expect("midnight is valid")
            .and_utc()
            - offset;
        (
            midnight - Duration::days(i64::from(days - 1)),
            Duration::days(1),
            days,
            "day",
        )
    };
    let buckets = (0..count)
        .map(|index| {
            let start = since + step * index as i32;
            let (key, label) = if hourly {
                (rfc3339(start), (start + offset).format("%H:%M").to_string())
            } else {
                let day = (start + offset).format("%Y-%m-%d").to_string();
                (day.clone(), day)
            };
            UsageBucket {
                key,
                label,
                total_cost_usd: 0.0,
                total_tokens: 0,
                by_provider: BTreeMap::new(),
            }
        })
        .collect();
    let mut result = UsageAggregation {
        since,
        until,
        step,
        totals: Rollup::default(),
        providers: HashMap::new(),
        models: HashMap::new(),
        buckets,
    };
    let mut next_session = 0;
    for file in files {
        for sample in &file.samples {
            result.record(file.provider, next_session, sample);
        }
        next_session += 1;
    }
    let mut run_sessions = HashMap::new();
    for run in runs {
        let session = run_sessions.entry(&run.session).or_insert_with(|| {
            let id = next_session;
            next_session += 1;
            id
        });
        result.record(run.provider, *session, &run.sample);
    }
    let totals = result.totals.totals();
    let shares = |row: &Totals| {
        let cost = if totals.cost_usd > 0.0 {
            row.cost_usd / totals.cost_usd
        } else {
            0.0
        };
        let tokens = if totals.total_tokens > 0 {
            row.total_tokens as f64 / totals.total_tokens as f64
        } else {
            0.0
        };
        (cost, tokens)
    };
    let mut providers: Vec<_> = result
        .providers
        .into_iter()
        .map(|(id, usage)| {
            let (provider, label, color) = provider_info(id);
            let totals = usage.totals();
            let (cost_share, token_share) = shares(&totals);
            UsageProviderRow {
                provider,
                label,
                color,
                totals,
                cost_share,
                token_share,
            }
        })
        .collect();
    providers.sort_by(|a, b| {
        b.totals
            .cost_usd
            .total_cmp(&a.totals.cost_usd)
            .then(a.provider.cmp(b.provider))
    });
    let mut models: Vec<_> = result
        .models
        .into_iter()
        .filter_map(|((id, model), usage)| {
            let totals = usage.totals();
            if model == "<synthetic>" || (totals.total_tokens == 0 && totals.cost_usd == 0.0) {
                return None;
            }
            let (cost_share, token_share) = shares(&totals);
            Some(UsageModelRow {
                provider: provider_info(id).0,
                model: model.to_owned(),
                totals,
                cost_share,
                token_share,
            })
        })
        .collect();
    models.sort_by(|a, b| {
        b.totals
            .cost_usd
            .total_cmp(&a.totals.cost_usd)
            .then(a.provider.cmp(b.provider))
            .then(a.model.cmp(&b.model))
    });
    UsageResponse {
        window: UsageWindow {
            days,
            resolution,
            since: rfc3339(since),
            until: rfc3339(until),
        },
        totals,
        providers,
        models,
        buckets: result.buckets,
        scanned_at: rfc3339(until),
    }
}

fn rfc3339(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}
