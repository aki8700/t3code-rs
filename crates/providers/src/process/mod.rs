mod command;
mod lifecycle;
mod run;
mod stream;

#[cfg(test)]
mod tests;

#[cfg(unix)]
pub(crate) mod startup;

#[allow(unused_imports)]
pub(crate) use command::{prepare, version};
use lifecycle::{GroupCleanup, RunGuard, already_reaped, configured_idle_timeout, wait_for_exit};
#[cfg(test)]
use lifecycle::{classify_group_signal_error, parse_idle_timeout, signal_group};
pub(crate) use lifecycle::{interrupt, send_control_response, send_steer};
#[cfg(test)]
use run::run_turn_with_idle_timeout;
pub(crate) use run::run_turn_with_profile;
#[cfg(test)]
use stream::normalize_process_start_time;
use stream::{
    ReadOutcome, exit_message, process_error, process_start_time, read_events, read_tail,
    read_tail_watching_limit, redact_stderr, write_input,
};
#[allow(unused_imports)]
pub(crate) use stream::{command, detect_fatal_stderr_limit};

use crate::{
    parsers::{Parser, provider_error},
    retry::TOOL_FAILURE_LOOP_ERROR_CODE,
    watchdog::{
        BudgetAction, InFlightTools, RepetitionWatchdog, ToolFailureAction, ToolFailureWatchdog,
        ToolLoopWatchdog, TurnBudgetWatchdog,
    },
};
#[allow(unused_imports)]
use serde_json::{Value, json};
#[allow(unused_imports)]
use std::{
    borrow::Cow,
    collections::HashMap,
    io,
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
#[allow(unused_imports)]
use t3_core::{
    protocol::StreamEvent,
    traits::{CoreError, CoreResult},
    types::{PendingResponse, ProviderId, TurnItemKind},
};
#[allow(unused_imports)]
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, Command},
    sync::{mpsc, oneshot},
};
#[cfg(unix)]
#[allow(unused_imports)]
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::watch,
};
#[allow(unused_imports)]
use uuid::Uuid;

const STDERR_LIMIT: usize = 16 * 1024;
const STDOUT_LINE_LIMIT: usize = 4 * 1024 * 1024;
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_IDLE_STDOUT_TIMEOUT: Duration = Duration::from_secs(900);
const MIN_IDLE_TIMEOUT_SECS: u64 = 60;
const MAX_IDLE_TIMEOUT_SECS: u64 = 3600;
const IDLE_TIMEOUT_ENV: &str = "T3_IDLE_TIMEOUT_SECS";
const LEGACY_IDLE_STDOUT_TIMEOUT_ENV: &str = "T3_IDLE_STDOUT_TIMEOUT_SECS";
const DEFAULT_STARTUP_TIMEOUT_SECS: u64 = 60;
const MIN_STARTUP_TIMEOUT_SECS: u64 = 15;
const STARTUP_TIMEOUT_ENV: &str = "T3_STARTUP_TIMEOUT_SECS";
/// While a tool step is running the idle limit is stretched by this factor
/// (default 900 s -> 1 h), so a hung tool is still stopped eventually.
const IN_FLIGHT_TOOL_IDLE_FACTOR: u32 = 4;
const TOOL_FAILURE_LOOP_NOTICE: &str = "[automatic notice] This tool call has failed with the same result twice. Stop retrying it and change the command, inputs, or approach. One more identical failure will stop the run.";

fn startup_timeout(idle_timeout: Duration) -> Duration {
    let secs = std::env::var(STARTUP_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_STARTUP_TIMEOUT_SECS)
        .max(MIN_STARTUP_TIMEOUT_SECS);
    Duration::from_secs(secs).min(idle_timeout)
}

fn warn_agent(
    steer: Option<&mpsc::UnboundedSender<StdinMsg>>,
    tx: &mpsc::UnboundedSender<StreamEvent>,
    notice: &str,
) {
    let steered = steer.is_some_and(|steer| steer.send(StdinMsg::Steer(notice.to_owned())).is_ok());
    if !steered {
        let _ = tx.send(StreamEvent::Item {
            kind: TurnItemKind::SystemNotice,
            summary: Some(notice.to_owned()),
            payload: json!({ "message": notice }),
        });
    }
}

fn warn_agent_of_tool_failure(
    steer: Option<&mpsc::UnboundedSender<StdinMsg>>,
    tx: &mpsc::UnboundedSender<StreamEvent>,
) {
    warn_agent(steer, tx, TOOL_FAILURE_LOOP_NOTICE);
}

/// Messages written to a streaming provider's stdin while the turn is open.
enum StdinMsg {
    /// A mid-turn user message, wrapped as a stream-json `user` event.
    Steer(String),
    /// A complete JSON object written as-is (one line, newline added on write).
    RawLine(String),
}

/// A provider permission request that is waiting for the user's answer.
pub(crate) struct ControlRequest {
    /// The CLI's own `request_id`, echoed back in `control_response`.
    provider_request_id: String,
    /// Original tool input, used as `updatedInput` when the user approves unchanged.
    input: Value,
}

pub(crate) struct ActiveProcess {
    #[cfg(unix)]
    pid: u32,
    #[cfg(unix)]
    released: watch::Receiver<bool>,
    stdin_tx: Option<mpsc::UnboundedSender<StdinMsg>>,
    /// Keyed by the `PendingRequest::id` shown to the user.
    control_requests: ControlRequests,
    interrupted: Arc<AtomicBool>,
    #[cfg(not(unix))]
    stop: oneshot::Sender<()>,
}

pub(crate) type ActiveRunMap = Arc<Mutex<HashMap<Uuid, ActiveProcess>>>;
type ControlRequests = Arc<Mutex<HashMap<Uuid, ControlRequest>>>;

pub(crate) enum Input {
    StreamJson(Value),
    Text(String),
    None,
}
