use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use t3_core::normalize_failure_output;
use t3_core::types::{ProviderId, TurnItemKind};

const WINDOW_BYTES: usize = 8 * 1024;
const MIN_PERIOD: usize = 48;
const MIN_LINE_CHARS: usize = 32;
const LINE_REPEATS: usize = 8;
const MAX_PERIOD: usize = 512;
const BLOCK_REPEATS: usize = 6;
const TOOL_LOOP_REPEATS: usize = 8;
const FAILED_TOOL_WARN_REPEATS: usize = 2;
const FAILED_TOOL_STOP_REPEATS: usize = 3;

/// Tool steps a provider has started but not yet finished.
///
/// agy withholds every later stdout event while a tool step is ACTIVE (for
/// example a background `sleep` or `muse-wait`), so stdout silence then means
/// "a tool is still running", not "the provider hung".
#[derive(Default)]
pub(crate) struct InFlightTools {
    steps: BTreeMap<u64, String>,
}

impl InFlightTools {
    pub(crate) fn observe(&mut self, provider: ProviderId, event: &Value) {
        let step = &event["step_update"];
        if provider != ProviderId::Antigravity
            || event["event"] != "step_update"
            || step["step_type"] != "tool"
        {
            return;
        }
        let Some(index) = step["step_index"].as_u64() else {
            return;
        };
        if step["state"] == "ACTIVE" {
            let name = step["tool_name"].as_str().unwrap_or("tool");
            self.steps.insert(index, name.to_owned());
        } else {
            self.steps.remove(&index);
        }
    }

    /// Name of the earliest tool step still running, if any.
    pub(crate) fn oldest(&self) -> Option<&str> {
        self.steps.values().next().map(String::as_str)
    }
}

/// Detects consecutive identical tool or command executions in a turn stream.
#[derive(Default)]
pub(crate) struct ToolLoopWatchdog {
    last: Option<(TurnItemKind, String)>,
    repeats: usize,
}

impl ToolLoopWatchdog {
    pub(crate) fn push(
        &mut self,
        kind: TurnItemKind,
        summary: Option<&str>,
        payload: &Value,
    ) -> bool {
        match kind {
            TurnItemKind::ToolCall | TurnItemKind::CommandExecution => {}
            TurnItemKind::FileChange => {
                self.reset();
                return false;
            }
            _ => return false,
        }

        let Some(signature) = normalized_tool_signature(summary, payload) else {
            self.reset();
            return false;
        };

        if self
            .last
            .as_ref()
            .is_some_and(|(prev_kind, prev_sig)| *prev_kind == kind && prev_sig == &signature)
        {
            self.repeats += 1;
            return self.repeats >= TOOL_LOOP_REPEATS;
        }

        self.last = Some((kind, signature));
        self.repeats = 1;
        false
    }

    fn reset(&mut self) {
        self.last = None;
        self.repeats = 0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolFailureAction {
    None,
    Warn,
    Stop,
}

#[derive(Default)]
pub(crate) struct ToolFailureWatchdog {
    calls: HashMap<String, (String, String)>,
    failures: HashMap<String, usize>,
}

impl ToolFailureWatchdog {
    pub(crate) fn push(
        &mut self,
        kind: TurnItemKind,
        summary: Option<&str>,
        payload: &Value,
    ) -> ToolFailureAction {
        if kind == TurnItemKind::ToolCall {
            if let (Some(id), Some(tool)) = (item_id(payload), tool_name(summary, payload)) {
                let call_signature =
                    normalized_tool_signature(Some(&tool), payload).unwrap_or_else(|| tool.clone());
                self.calls.insert(id.to_owned(), (tool, call_signature));
            }
            return ToolFailureAction::None;
        }
        if kind != TurnItemKind::ToolResult {
            return ToolFailureAction::None;
        }

        let invocation = item_id(payload).and_then(|id| self.calls.remove(id));
        let Some(tool) = tool_name(None, payload)
            .or_else(|| invocation.as_ref().map(|(tool, _)| tool.clone()))
            .or_else(|| {
                summary
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
            })
        else {
            return ToolFailureAction::None;
        };
        if is_read_only_tool(&tool) {
            return ToolFailureAction::None;
        }

        let tool_key = tool.trim().to_ascii_lowercase();
        let prefix = format!("{tool_key}\0");
        let Some(failure) = failure_output(payload) else {
            self.failures
                .retain(|signature, _| !signature.starts_with(&prefix));
            return ToolFailureAction::None;
        };

        let call_signature = invocation
            .map(|(_, signature)| signature)
            .or_else(|| normalized_tool_signature(Some(&tool), payload))
            .unwrap_or_else(|| tool.clone());
        let error = normalize_failure_output(&failure);
        let signature = format!("{prefix}{call_signature}\0{error}");
        let count = self.failures.entry(signature.clone()).or_default();
        *count += 1;
        if *count >= FAILED_TOOL_STOP_REPEATS {
            ToolFailureAction::Stop
        } else if *count == FAILED_TOOL_WARN_REPEATS {
            ToolFailureAction::Warn
        } else {
            ToolFailureAction::None
        }
    }
}

fn item_id(payload: &Value) -> Option<&str> {
    ["tool_use_id", "call_id", "toolCallId", "id"]
        .into_iter()
        .find_map(|field| payload.get(field).and_then(Value::as_str))
}

fn tool_name(summary: Option<&str>, payload: &Value) -> Option<String> {
    summary
        .or_else(|| {
            ["name", "tool", "tool_name", "toolName"]
                .into_iter()
                .find_map(|field| payload.get(field).and_then(Value::as_str))
        })
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

fn is_read_only_tool(tool: &str) -> bool {
    let tool = tool.trim().to_ascii_lowercase();
    [
        "read",
        "read_file",
        "view",
        "grep",
        "glob",
        "ls",
        "list_files",
        "list_directory",
        "search",
        "web_search",
    ]
    .into_iter()
    .any(|name| tool == name || tool.ends_with(&format!("__{name}")))
}

fn failure_output(payload: &Value) -> Option<String> {
    for result in [payload, &payload["state"]] {
        let error = &result["error"];
        let failed = result["is_error"].as_bool() == Some(true)
            || result["isError"].as_bool() == Some(true)
            || result["status"].as_str().is_some_and(|status| {
                matches!(
                    status.to_ascii_lowercase().as_str(),
                    "error" | "failed" | "failure"
                )
            })
            || error.as_str().is_some_and(|error| !error.trim().is_empty())
            || error.as_object().is_some_and(|error| !error.is_empty())
            || error.as_array().is_some_and(|error| !error.is_empty());
        if failed {
            return Some(
                ["error", "message", "content", "output", "result", "stderr"]
                    .into_iter()
                    .find_map(|field| {
                        let value = result.get(field)?;
                        (!value.is_null() && !value.is_boolean()).then(|| {
                            value
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| value.to_string())
                        })
                    })
                    .unwrap_or_else(|| "tool reported failure without details".to_owned()),
            );
        }
    }
    None
}

fn normalized_tool_signature(summary: Option<&str>, payload: &Value) -> Option<String> {
    let summary_norm = summary
        .map(normalize_whitespace)
        .filter(|text| !text.is_empty())
        .or_else(|| {
            ["name", "tool", "command", "title", "path"]
                .into_iter()
                .find_map(|field| payload.get(field).and_then(Value::as_str))
                .map(normalize_whitespace)
                .filter(|text| !text.is_empty())
        });

    let args = payload
        .get("input")
        .or_else(|| payload.get("arguments"))
        .or_else(|| payload.get("args"))
        .or_else(|| payload.pointer("/state/input"))
        .or_else(|| payload.get("command"))
        .or_else(|| payload.get("cmd"))
        .filter(|value| !value.is_null());

    if summary_norm.is_none() && args.is_none() {
        return None;
    }

    let mut out = summary_norm.unwrap_or_default();
    if let Some(args) = args {
        out.push('\0');
        canonicalize_value(args, &mut out);
    }
    Some(out)
}

fn normalize_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

fn canonicalize_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => {
            let normalized = normalize_whitespace(s);
            if let Ok(encoded) = serde_json::to_string(&normalized) {
                out.push_str(&encoded);
            }
        }
        Value::Array(items) => {
            out.push('[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                canonicalize_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by_key(|(a, _)| *a);
            for (idx, (key, val)) in entries.into_iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                if let Ok(encoded) = serde_json::to_string(key) {
                    out.push_str(&encoded);
                }
                out.push(':');
                canonicalize_value(val, out);
            }
            out.push('}');
        }
    }
}

#[derive(Default)]
pub(crate) struct RepetitionWatchdog {
    window: String,
    pending: String,
    pending_overflow: bool,
    last_line: String,
    equal_lines: usize,
    fence: Option<(char, usize)>,
}

impl RepetitionWatchdog {
    pub(crate) fn push(&mut self, text: &str) -> bool {
        for fragment in text.split_inclusive('\n') {
            let body = fragment.strip_suffix('\n').unwrap_or(fragment);
            let take = body.floor_char_boundary(WINDOW_BYTES - self.pending.len());
            self.pending.push_str(&body[..take]);
            self.pending_overflow |= take < body.len();

            let trimmed = self.pending.trim();
            let marker = fence_marker(trimmed)
                .map(|(character, count, suffix)| (character, count, suffix.is_empty()));
            let exempt = self.fence.is_some()
                || marker.is_some()
                || markdown_row(trimmed)
                || self.pending.starts_with("    ")
                || self.pending.starts_with('\t')
                || (!self.pending_overflow
                    && !trimmed.is_empty()
                    && !trimmed.chars().any(char::is_alphanumeric));
            if exempt {
                self.window.clear();
            } else if self.push_blocks(fragment) {
                return true;
            }

            let trimmed = self.pending.trim();
            let eligible = !exempt
                && !self.pending_overflow
                && trimmed.chars().nth(MIN_LINE_CHARS - 1).is_some();
            if eligible && trimmed == self.last_line && self.equal_lines + 1 == LINE_REPEATS {
                return true;
            }
            if fragment.ends_with('\n') {
                if let Some((character, count, closed)) = marker {
                    match self.fence {
                        None => self.fence = Some((character, count)),
                        Some((open, width)) if character == open && count >= width && closed => {
                            self.fence = None;
                        }
                        _ => {}
                    }
                }
                if eligible {
                    if trimmed == self.last_line {
                        self.equal_lines += 1;
                    } else {
                        self.last_line.clear();
                        self.last_line.push_str(trimmed);
                        self.equal_lines = 1;
                    }
                } else {
                    self.equal_lines = 0;
                }
                self.pending.clear();
                self.pending_overflow = false;
            }
        }
        false
    }

    fn push_blocks(&mut self, mut text: &str) -> bool {
        while !text.is_empty() {
            let end = text.floor_char_boundary(WINDOW_BYTES);
            let (chunk, rest) = text.split_at(end);
            let previous = self.window.len();
            self.window.push_str(chunk);
            for (offset, ch) in chunk.char_indices() {
                let end = previous + offset + ch.len_utf8();
                let start = self
                    .window
                    .ceil_char_boundary(end.saturating_sub(WINDOW_BYTES));
                let tail = &self.window[start..end];
                if repeated_block(tail, ch) {
                    return true;
                }
            }
            let keep = self.window.len().saturating_sub(WINDOW_BYTES);
            self.window.drain(..self.window.ceil_char_boundary(keep));
            text = rest;
        }
        false
    }
}

fn fence_marker(line: &str) -> Option<(char, usize, &str)> {
    let character = line.chars().next().filter(|ch| matches!(ch, '`' | '~'))?;
    let width = line.chars().take_while(|&ch| ch == character).count();
    (width >= 3).then(|| (character, width, line[width..].trim()))
}

fn markdown_row(line: &str) -> bool {
    line.starts_with('|') || line.ends_with('|') || line.contains(" | ")
}

fn repeated_block(tail: &str, last: char) -> bool {
    let end = tail.len();
    let max_period = MAX_PERIOD.min(end / BLOCK_REPEATS);
    if max_period < MIN_PERIOD
        || !tail[tail.ceil_char_boundary(end - max_period)..]
            .chars()
            .rev()
            .any(char::is_alphanumeric)
    {
        return false;
    }
    // Candidate periods must end at a previous occurrence of the last character.
    let start = tail.ceil_char_boundary(end - max_period - last.len_utf8());
    let stop = tail.floor_char_boundary(end - MIN_PERIOD);
    for (offset, _) in tail
        .get(start..stop)
        .unwrap_or_default()
        .rmatch_indices(last)
    {
        let period = end - (start + offset + last.len_utf8());
        let repeated = &tail.as_bytes()[end - period * BLOCK_REPEATS..];
        if repeated[..period * (BLOCK_REPEATS - 1)] != repeated[period..] {
            continue;
        }
        let block = &tail[end - period..];
        // A repeated single line belongs to the line detector. Longer multiples
        // of an exempt short period are also exempt at this position.
        return block.chars().any(char::is_alphanumeric)
            && block.bytes().filter(|&b| b == b'\n').count() != 1
            && !short_pattern(block.as_bytes());
    }
    false
}

fn short_pattern(block: &[u8]) -> bool {
    // A longer multiple of a repeated short token is still a short pattern.
    (1..MIN_PERIOD).any(|period| {
        block.len().is_multiple_of(period) && block[..block.len() - period] == block[period..]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_call(id: &str, tool: &str, input: Value) -> Value {
        serde_json::json!({"id": id, "name": tool, "input": input})
    }

    fn tool_result(id: &str, failed: bool, output: &str) -> Value {
        serde_json::json!({
            "tool_use_id": id,
            "is_error": failed,
            "content": output
        })
    }

    #[test]
    fn repeated_failure_signature_warns_then_stops_across_read_only_results() {
        let mut watchdog = ToolFailureWatchdog::default();
        let input = serde_json::json!({"command": "make build"});
        for (id, elapsed) in [("b1", "10ms"), ("b2", "30ms"), ("b3", "50ms")] {
            assert_eq!(
                watchdog.push(
                    TurnItemKind::ToolCall,
                    Some("Bash"),
                    &tool_call(id, "Bash", input.clone())
                ),
                ToolFailureAction::None
            );
            if id == "b2" {
                watchdog.push(
                    TurnItemKind::ToolCall,
                    Some("Read"),
                    &tool_call("r1", "Read", serde_json::json!({"file_path": "x"})),
                );
                watchdog.push(
                    TurnItemKind::ToolResult,
                    None,
                    &tool_result("r1", true, "missing file"),
                );
            }
            let result = tool_result(id, true, &format!("failed in {elapsed}"));
            let action = watchdog.push(TurnItemKind::ToolResult, None, &result);
            assert_eq!(
                action,
                if id == "b2" {
                    ToolFailureAction::Warn
                } else if id == "b3" {
                    ToolFailureAction::Stop
                } else {
                    ToolFailureAction::None
                }
            );
        }
    }

    #[test]
    fn success_of_a_tool_clears_its_failure_count_and_read_failures_are_neutral() {
        let mut watchdog = ToolFailureWatchdog::default();
        let input = serde_json::json!({"command": "make build"});
        for id in ["b1", "b2"] {
            watchdog.push(
                TurnItemKind::ToolCall,
                Some("Bash"),
                &tool_call(id, "Bash", input.clone()),
            );
            let action = watchdog.push(
                TurnItemKind::ToolResult,
                None,
                &tool_result(id, true, "failed"),
            );
            assert_eq!(
                action,
                if id == "b2" {
                    ToolFailureAction::Warn
                } else {
                    ToolFailureAction::None
                }
            );
        }

        watchdog.push(
            TurnItemKind::ToolCall,
            Some("Bash"),
            &tool_call("ok", "Bash", input.clone()),
        );
        assert_eq!(
            watchdog.push(
                TurnItemKind::ToolResult,
                None,
                &tool_result("ok", false, "success")
            ),
            ToolFailureAction::None
        );
        for id in ["b3", "b4"] {
            watchdog.push(
                TurnItemKind::ToolCall,
                Some("Bash"),
                &tool_call(id, "Bash", input.clone()),
            );
            let action = watchdog.push(
                TurnItemKind::ToolResult,
                None,
                &tool_result(id, true, "failed"),
            );
            assert_eq!(
                action,
                if id == "b4" {
                    ToolFailureAction::Warn
                } else {
                    ToolFailureAction::None
                }
            );
        }

        for id in ["r2", "r3", "r4"] {
            watchdog.push(
                TurnItemKind::ToolCall,
                Some("Read"),
                &tool_call(id, "Read", serde_json::json!({"file_path": "x"})),
            );
            assert_eq!(
                watchdog.push(
                    TurnItemKind::ToolResult,
                    None,
                    &tool_result(id, true, "failed")
                ),
                ToolFailureAction::None
            );
        }
    }

    #[test]
    fn detects_the_eighth_trimmed_line_without_a_final_newline() {
        let mut watchdog = RepetitionWatchdog::default();
        let line = "Repeating this exact line until stopped!";
        for _ in 0..7 {
            assert!(!watchdog.push(&format!("  {line} \r\n")));
        }
        assert!(!watchdog.push("Repeating this exact "));
        assert!(watchdog.push("line until stopped!"));
    }

    #[test]
    fn detects_a_block_split_across_chunks_and_the_end_of_a_large_chunk() {
        let block = "もう一度同じファイルを確認します。";
        let mut watchdog = RepetitionWatchdog::default();
        assert!(!watchdog.push(&"🎨".repeat(WINDOW_BYTES / 4 + 1)));
        for _ in 0..5 {
            assert!(!watchdog.push(block));
        }
        assert!(watchdog.push(&format!("{block}{}", "🎨".repeat(WINDOW_BYTES))));

        let mut watchdog = RepetitionWatchdog::default();
        let final_chunk = format!("{}{}", "🎨".repeat(WINDOW_BYTES), block.repeat(6));
        assert!(watchdog.push(&final_chunk));
    }

    #[test]
    fn normal_code_tables_dividers_and_short_tokens_do_not_trigger() {
        for text in [
            (0..150)
                .map(|i| format!("    let value_{i} = {i};\n"))
                .collect(),
            format!(
                "| Name | Value |\n| :--- | ---: |\n{}",
                "| ok | 1 |\n".repeat(80)
            ),
            "| A long markdown table label that appears more than once | value |\n".repeat(16),
            format!(
                "```rust\n{}```\n",
                "    println!(\"a repeated source statement\");\n".repeat(16)
            ),
            "---".repeat(100),
            "===\n".repeat(100),
            "|:------------------------|------------------------:|\n".repeat(16),
            "ok ".repeat(100),
            format!("{}a", "🎨".repeat(40)),
        ] {
            let mut watchdog = RepetitionWatchdog::default();
            assert!(!watchdog.push(&text));
        }
    }

    #[test]
    fn tool_loop_detects_eight_consecutive_identical_tool_calls_across_tool_results() {
        let mut watchdog = ToolLoopWatchdog::default();
        for idx in 0..7 {
            let payload = serde_json::json!({
                "type": "tool_use",
                "id": format!("call-{idx}"),
                "name": "Bash",
                "input": { "command": "  cargo   test " }
            });
            assert!(!watchdog.push(TurnItemKind::ToolCall, Some("Bash"), &payload));
            // Interleaved tool_result events must not reset the consecutive counter.
            assert!(!watchdog.push(
                TurnItemKind::ToolResult,
                None,
                &serde_json::json!({"tool_use_id": format!("call-{idx}")})
            ));
        }
        let eighth = serde_json::json!({
            "type": "tool_use",
            "id": "call-7",
            "name": "Bash",
            "input": { "command": "cargo test" }
        });
        assert!(watchdog.push(TurnItemKind::ToolCall, Some("  Bash "), &eighth));
    }

    #[test]
    fn tool_loop_distinguishes_kinds_arguments_and_resets_on_change() {
        let mut watchdog = ToolLoopWatchdog::default();
        // Same tool name with different inputs must not trigger.
        for idx in 0..10 {
            let payload = serde_json::json!({
                "name": "Read",
                "input": { "file_path": format!("file_{idx}.rs") }
            });
            assert!(!watchdog.push(TurnItemKind::ToolCall, Some("Read"), &payload));
        }

        // Seven identical command executions followed by a different kind does not trigger.
        for _ in 0..7 {
            assert!(!watchdog.push(
                TurnItemKind::CommandExecution,
                Some("ls -la"),
                &serde_json::json!({"command": "ls -la"})
            ));
        }
        assert!(!watchdog.push(
            TurnItemKind::ToolCall,
            Some("ls -la"),
            &serde_json::json!({"command": "ls -la"})
        ));

        // FileChange resets the consecutive streak.
        for _ in 0..7 {
            assert!(!watchdog.push(
                TurnItemKind::CommandExecution,
                Some("cargo check"),
                &Value::Null
            ));
        }
        assert!(!watchdog.push(TurnItemKind::FileChange, Some("src/lib.rs"), &Value::Null));
        assert!(!watchdog.push(
            TurnItemKind::CommandExecution,
            Some("cargo check"),
            &Value::Null
        ));

        // Empty/unidentifiable items do not trigger.
        for _ in 0..10 {
            assert!(!watchdog.push(TurnItemKind::ToolCall, None, &Value::Null));
        }
    }
}
