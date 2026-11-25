use crate::{coordinator::text_value, threads::redact_credentials};
use chrono::SecondsFormat;
use serde_json::Value;
use std::fmt::Write as _;
use t3_core::{Run, RunStatus, Thread, ThreadStatus, TurnItem, TurnItemKind};

/// Renders a thread, its runs, and its chronological turn items as a self-contained
/// Markdown transcript with credentials redacted from all user, assistant, command,
/// tool, and error text.
pub(crate) fn render_thread_markdown(thread: &Thread, runs: &[Run], items: &[TurnItem]) -> String {
    let mut out = String::new();

    let raw_title = thread.title.trim();
    let title = if raw_title.is_empty() {
        "Untitled Thread".to_owned()
    } else {
        single_line(&redact_credentials(raw_title))
    };
    let _ = writeln!(out, "# {title}");
    out.push('\n');

    let provider = format!(
        "{} ({})",
        thread.model_selection.provider.display_name(),
        thread.model_selection.provider.as_str()
    );
    let model = match thread
        .model_selection
        .effort
        .as_deref()
        .map(str::trim)
        .filter(|effort| !effort.is_empty())
    {
        Some(effort) => format!("{} (effort: {effort})", thread.model_selection.model),
        None => thread.model_selection.model.clone(),
    };
    let branch = thread
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .map(|branch| inline_code(&redact_credentials(branch)))
        .unwrap_or_else(|| "none".to_owned());
    let created = thread.created_at.to_rfc3339_opts(SecondsFormat::Secs, true);
    let total_input: u64 = runs
        .iter()
        .filter_map(|run| run.input_tokens)
        .fold(0, u64::saturating_add);
    let total_output: u64 = runs
        .iter()
        .filter_map(|run| run.output_tokens)
        .fold(0, u64::saturating_add);
    let total_tokens = total_input.saturating_add(total_output);

    let _ = writeln!(out, "- **Provider**: {provider}");
    let _ = writeln!(out, "- **Model**: {}", redact_credentials(&model));
    let _ = writeln!(out, "- **Branch**: {branch}");
    let _ = writeln!(out, "- **Status**: {}", thread_status_label(thread.status));
    let _ = writeln!(out, "- **Created**: {created}");
    let _ = writeln!(out, "- **Runs**: {}", runs.len());
    let _ = writeln!(
        out,
        "- **Total Tokens**: {total_tokens} ({total_input} input / {total_output} output)"
    );

    let run_errors: Vec<_> = runs
        .iter()
        .filter_map(|run| {
            let error = run
                .error_message
                .as_deref()
                .map(str::trim)
                .filter(|msg| !msg.is_empty())?;
            Some((run.ordinal, run.status, redact_credentials(error)))
        })
        .collect();
    if !run_errors.is_empty() {
        out.push_str("\n## Run Errors\n\n");
        for (ordinal, status, error) in run_errors {
            let _ = writeln!(
                out,
                "- **Run {ordinal}** ({}): {}",
                run_status_label(status),
                single_line(&error)
            );
        }
    }

    out.push_str("\n---\n");

    if items.is_empty() {
        out.push_str("\n_No transcript items._\n");
        return out;
    }

    let mut ordered: Vec<&TurnItem> = items.iter().collect();
    ordered.sort_by_key(|item| (item.ordinal, item.created_at));

    for item in ordered {
        out.push('\n');
        render_item(&mut out, item);
    }

    out
}

fn render_item(out: &mut String, item: &TurnItem) {
    let ts = item.created_at.to_rfc3339_opts(SecondsFormat::Secs, true);
    match item.kind {
        TurnItemKind::UserMessage => render_user_message(out, item, &ts),
        TurnItemKind::AssistantMessage => render_assistant_message(out, item, &ts),
        TurnItemKind::Reasoning => render_reasoning(out, item, &ts),
        TurnItemKind::CommandExecution => render_command_execution(out, item, &ts),
        TurnItemKind::FileChange => render_file_change(out, item, &ts),
        TurnItemKind::ToolCall => render_tool_call(out, item, &ts),
        TurnItemKind::ToolResult => render_tool_result(out, item, &ts),
        TurnItemKind::SystemNotice => render_system_notice(out, item, &ts),
        TurnItemKind::Subagent => render_subagent(out, item, &ts),
        TurnItemKind::Handoff => render_handoff(out, item, &ts),
    }
}

fn render_user_message(out: &mut String, item: &TurnItem, ts: &str) {
    let _ = writeln!(out, "## User ({ts})\n");
    if let Some(body) = text_value(&item.payload)
        .or(item.summary.as_deref())
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        let _ = writeln!(out, "{}", redact_credentials(body));
    }
    if let Some(count) = item.payload["attachment_ids"]
        .as_array()
        .map(Vec::len)
        .filter(|&n| n > 0)
    {
        let _ = writeln!(out, "\n_Attachments: {count}_");
    }
}

fn render_assistant_message(out: &mut String, item: &TurnItem, ts: &str) {
    let _ = writeln!(out, "## Assistant ({ts})\n");
    if let Some(body) = text_value(&item.payload)
        .or(item.summary.as_deref())
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        let _ = writeln!(out, "{}", redact_credentials(body));
    }
}

fn render_reasoning(out: &mut String, item: &TurnItem, ts: &str) {
    let body = text_value(&item.payload)
        .or(item.payload["thinking"].as_str())
        .or(item.payload["reasoning"].as_str())
        .or(item.summary.as_deref())
        .map(str::trim)
        .unwrap_or("");
    let _ = writeln!(out, "<details>");
    let _ = writeln!(out, "<summary>Reasoning ({ts})</summary>\n");
    if !body.is_empty() {
        let _ = writeln!(out, "{}\n", redact_credentials(body));
    }
    let _ = writeln!(out, "</details>");
}

fn render_command_execution(out: &mut String, item: &TurnItem, ts: &str) {
    let _ = writeln!(out, "### Command Execution ({ts})\n");
    let command = item.payload["command"]
        .as_str()
        .or(item.summary.as_deref())
        .map(str::trim)
        .filter(|cmd| !cmd.is_empty());
    if let Some(command) = command {
        let redacted = redact_credentials(command);
        if redacted.contains('\n') {
            let _ = writeln!(out, "**Command**:\n");
            push_fenced_block(out, "sh", &redacted);
        } else {
            let _ = writeln!(out, "- **Command**: {}", inline_code(&redacted));
        }
    }

    let mut status_notes = Vec::new();
    if item.payload["timed_out"].as_bool() == Some(true) {
        status_notes.push("timed out");
    }
    if item.payload["cancelled"].as_bool() == Some(true) {
        status_notes.push("cancelled");
    }
    let exit_label = match item.payload["exit_code"].as_i64() {
        Some(code) if status_notes.is_empty() => code.to_string(),
        Some(code) => format!("{code} ({})", status_notes.join(", ")),
        None if status_notes.is_empty() => "unknown".to_owned(),
        None => format!("unknown ({})", status_notes.join(", ")),
    };
    let _ = writeln!(out, "- **Exit Code**: {exit_label}");

    if let Some(output) = command_output(&item.payload) {
        let trimmed = output.trim_end();
        if !trimmed.is_empty() {
            out.push('\n');
            push_fenced_block(out, "", &redact_credentials(trimmed));
        }
    }
}

fn render_file_change(out: &mut String, item: &TurnItem, ts: &str) {
    let _ = writeln!(out, "### File Change ({ts})\n");
    let mut wrote_path = false;
    if let Some(changes) = item.payload["changes"].as_array() {
        for change in changes {
            if let Some(path) = change["path"]
                .as_str()
                .map(str::trim)
                .filter(|p| !p.is_empty())
            {
                let redacted = redact_credentials(path);
                let action = first_str(change, &["kind", "action", "status"]);
                match action {
                    Some(kind) => {
                        let _ = writeln!(out, "- {} ({})", inline_code(&redacted), kind);
                    }
                    None => {
                        let _ = writeln!(out, "- {}", inline_code(&redacted));
                    }
                }
                wrote_path = true;
            }
        }
    }
    if !wrote_path
        && let Some(path) = item.payload["path"]
            .as_str()
            .or(item.summary.as_deref())
            .map(str::trim)
            .filter(|p| !p.is_empty())
    {
        let redacted = redact_credentials(path);
        let action = first_str(&item.payload, &["kind", "action", "status"]);
        match action {
            Some(kind) => {
                let _ = writeln!(out, "- {} ({})", inline_code(&redacted), kind);
            }
            None => {
                let _ = writeln!(out, "- {}", inline_code(&redacted));
            }
        }
    }
    if let Some(diff) = first_str(&item.payload, &["diff", "patch"])
        .map(str::trim_end)
        .filter(|d| !d.is_empty())
    {
        out.push('\n');
        push_fenced_block(out, "diff", &redact_credentials(diff));
    }
}

fn render_tool_call(out: &mut String, item: &TurnItem, ts: &str) {
    let name = first_str(&item.payload, &["name", "tool", "tool_name"])
        .or(item.summary.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("tool");
    let _ = writeln!(
        out,
        "### Tool Call: {} ({ts})",
        single_line(&redact_credentials(name))
    );
    if let Some(input) = item
        .payload
        .get("input")
        .or_else(|| item.payload.get("arguments"))
        .filter(|v| !v.is_null())
        && let Ok(pretty) = serde_json::to_string_pretty(input)
    {
        out.push('\n');
        push_fenced_block(out, "json", &redact_credentials(&pretty));
    }
}

fn render_tool_result(out: &mut String, item: &TurnItem, ts: &str) {
    let is_error = item.payload["is_error"].as_bool() == Some(true)
        || item.payload["error"].as_bool() == Some(true)
        || matches!(
            item.payload["state"]["status"].as_str(),
            Some("error" | "failed")
        )
        || item.payload["error"]
            .as_str()
            .is_some_and(|msg| !msg.trim().is_empty())
        || item.payload["state"]["error"]
            .as_str()
            .is_some_and(|msg| !msg.trim().is_empty());
    let suffix = if is_error { " (Error)" } else { "" };
    let _ = writeln!(out, "### Tool Result{suffix} ({ts})");
    if let Some(body) = tool_result_text(item) {
        let trimmed = body.trim_end();
        if !trimmed.is_empty() {
            out.push('\n');
            push_fenced_block(out, "", &redact_credentials(trimmed));
        }
    }
}

fn render_system_notice(out: &mut String, item: &TurnItem, ts: &str) {
    let _ = writeln!(out, "### System Notice ({ts})\n");
    let text = text_value(&item.payload)
        .or(item.summary.as_deref())
        .map(str::trim)
        .unwrap_or("");
    if !text.is_empty() {
        let redacted = redact_credentials(text);
        for line in redacted.lines() {
            let _ = writeln!(out, "> {line}");
        }
    }
}

fn render_subagent(out: &mut String, item: &TurnItem, ts: &str) {
    let title = item.payload["title"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match title {
        Some(title) => {
            let _ = writeln!(
                out,
                "### Subagent: {} ({ts})\n",
                single_line(&redact_credentials(title))
            );
        }
        None => {
            let _ = writeln!(out, "### Subagent ({ts})\n");
        }
    }
    if let Some(status) = item.payload["status"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let _ = writeln!(out, "- **Status**: {status}");
    }
    let provider = item.payload["provider"].as_str();
    let model = item.payload["model"].as_str();
    match (provider, model) {
        (Some(p), Some(m)) => {
            let _ = writeln!(out, "- **Model**: {p}/{}", redact_credentials(m));
        }
        (None, Some(m)) => {
            let _ = writeln!(out, "- **Model**: {}", redact_credentials(m));
        }
        (Some(p), None) => {
            let _ = writeln!(out, "- **Provider**: {p}");
        }
        (None, None) => {}
    }
    if let Some(child_id) = item.payload["child_thread_id"].as_str() {
        let _ = writeln!(out, "- **Child Thread**: {}", inline_code(child_id));
    }
    if let Some(summary) = item.payload["summary"]
        .as_str()
        .or_else(|| text_value(&item.payload))
        .or(item.summary.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push('\n');
        let _ = writeln!(out, "{}", redact_credentials(summary));
    }
}

fn render_handoff(out: &mut String, item: &TurnItem, ts: &str) {
    let _ = writeln!(out, "### Context Handoff ({ts})\n");
    if let (Some(from_model), Some(to_model)) = (
        item.payload
            .pointer("/from_model/model")
            .and_then(Value::as_str),
        item.payload
            .pointer("/to_model/model")
            .and_then(Value::as_str),
    ) {
        let _ = writeln!(
            out,
            "- **Transition**: {} -> {}\n",
            redact_credentials(from_model),
            redact_credentials(to_model)
        );
    }
    if let Some(summary) = item.payload["summary"]
        .as_str()
        .or_else(|| text_value(&item.payload))
        .or(item.summary.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let _ = writeln!(out, "{}", redact_credentials(summary));
    }
}

fn command_output(payload: &Value) -> Option<String> {
    if let Some(output) = first_str(payload, &["output", "aggregated_output"])
        && !output.is_empty()
    {
        return Some(output.to_owned());
    }
    let stdout = payload["stdout"].as_str().unwrap_or("");
    let stderr = payload["stderr"].as_str().unwrap_or("");
    if stdout.is_empty() && stderr.is_empty() {
        return None;
    }
    let mut combined = String::new();
    combined.push_str(stdout);
    if !stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(stderr);
    }
    Some(combined)
}

fn tool_result_text(item: &TurnItem) -> Option<String> {
    let payload = &item.payload;
    if let Some(text) = text_value(payload)
        && !text.trim().is_empty()
    {
        return Some(text.to_owned());
    }
    if let Some(blocks) = payload["content"].as_array() {
        let parts: Vec<&str> = blocks
            .iter()
            .filter_map(text_value)
            .filter(|s| !s.trim().is_empty())
            .collect();
        if !parts.is_empty() {
            return Some(parts.join("\n"));
        }
    }
    let state = &payload["state"];
    for candidate in [
        state["output"].as_str(),
        state["error"].as_str(),
        payload["error"].as_str(),
        payload["error_message"].as_str(),
        item.summary.as_deref(),
    ] {
        if let Some(text) = candidate.filter(|s| !s.trim().is_empty()) {
            return Some(text.to_owned());
        }
    }
    if let Some(result) = payload.get("result").filter(|v| !v.is_null()) {
        return serde_json::to_string_pretty(result).ok();
    }
    None
}

fn first_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| value[*key].as_str())
}

fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn inline_code(text: &str) -> String {
    let max_run = max_backtick_run(text);
    let ticks = "`".repeat(max_run + 1);
    if text.starts_with('`') || text.ends_with('`') {
        format!("{ticks} {text} {ticks}")
    } else {
        format!("{ticks}{text}{ticks}")
    }
}

fn push_fenced_block(out: &mut String, lang: &str, body: &str) {
    let fence_len = max_backtick_run(body).max(2) + 1;
    let fence = "`".repeat(fence_len);
    let _ = writeln!(out, "{fence}{lang}");
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    let _ = writeln!(out, "{fence}");
}

fn max_backtick_run(text: &str) -> usize {
    let mut best = 0;
    let mut current = 0;
    for byte in text.bytes() {
        if byte == b'`' {
            current += 1;
            best = best.max(current);
        } else {
            current = 0;
        }
    }
    best
}

fn thread_status_label(status: ThreadStatus) -> &'static str {
    match status {
        ThreadStatus::Idle => "idle",
        ThreadStatus::Running => "running",
        ThreadStatus::WaitingForChildren => "waiting-for-children",
        ThreadStatus::RateLimited => "rate-limited",
        ThreadStatus::Completed => "completed",
        ThreadStatus::Failed => "failed",
        ThreadStatus::Interrupted => "interrupted",
    }
}

fn run_status_label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Queued => "queued",
        RunStatus::Running => "running",
        RunStatus::RateLimited => "rate-limited",
        RunStatus::Completed => "completed",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use serde_json::json;
    use t3_core::{InteractionMode, ModelSelection, ProviderId, RuntimeMode};
    use uuid::Uuid;

    fn sample_thread(title: &str) -> Thread {
        let ts = Utc.with_ymd_and_hms(2026, 11, 25, 10, 0, 0).unwrap();
        Thread {
            id: Uuid::nil(),
            project_id: Uuid::nil(),
            parent_thread_id: None,
            relationship: None,
            title: title.to_owned(),
            model_selection: ModelSelection {
                provider: ProviderId::Claude,
                model: "claude-sonnet-5-5".into(),
                effort: Some("high".into()),
                agent: None,
            },
            runtime_mode: RuntimeMode::FullAccess,
            interaction_mode: InteractionMode::Default,
            status: ThreadStatus::Completed,
            worktree_path: None,
            branch: Some("feat/export".into()),
            provider_session_id: None,
            archived: false,
            pinned: false,
            created_at: ts,
            updated_at: ts,
        }
    }

    fn sample_run(
        ordinal: i64,
        input: Option<u64>,
        output: Option<u64>,
        error: Option<&str>,
    ) -> Run {
        let ts = Utc.with_ymd_and_hms(2026, 11, 25, 10, 1, 0).unwrap();
        Run {
            id: Uuid::new_v4(),
            thread_id: Uuid::nil(),
            ordinal,
            provider: ProviderId::Claude,
            model: "claude-sonnet-5-5".into(),
            effort: Some("high".into()),
            status: if error.is_some() {
                RunStatus::Failed
            } else {
                RunStatus::Completed
            },
            prompt: "hello".into(),
            provider_session_id: None,
            input_tokens: input,
            output_tokens: output,
            duration_ms: Some(1200),
            error_message: error.map(str::to_owned),
            started_at: ts,
            completed_at: Some(ts),
        }
    }

    fn sample_item(
        ordinal: i64,
        kind: TurnItemKind,
        summary: Option<&str>,
        payload: Value,
    ) -> TurnItem {
        let ts = Utc
            .with_ymd_and_hms(2026, 11, 25, 10, 0, (ordinal.max(0) as u32) % 60)
            .unwrap();
        TurnItem {
            id: Uuid::new_v4(),
            thread_id: Uuid::nil(),
            run_id: Uuid::nil(),
            ordinal,
            kind,
            summary: summary.map(str::to_owned),
            payload,
            raw_bytes: 0,
            compressed_bytes: None,
            created_at: ts,
        }
    }

    #[test]
    fn empty_thread_renders_metadata_and_empty_notice() {
        let mut thread = sample_thread("Empty Session");
        thread.branch = None;
        thread.status = ThreadStatus::Idle;
        let md = render_thread_markdown(&thread, &[], &[]);

        assert!(md.starts_with("# Empty Session\n"));
        assert!(md.contains("- **Provider**: Claude Code (claude)"));
        assert!(md.contains("- **Model**: claude-sonnet-5-5 (effort: high)"));
        assert!(md.contains("- **Branch**: none"));
        assert!(md.contains("- **Status**: idle"));
        assert!(md.contains("- **Created**: 2026-11-25T10:00:00Z"));
        assert!(md.contains("- **Runs**: 0"));
        assert!(md.contains("- **Total Tokens**: 0 (0 input / 0 output)"));
        assert!(md.contains("_No transcript items._"));
        assert!(!md.contains("## Run Errors"));
    }

    #[test]
    fn renders_all_item_kinds_in_chronological_order() {
        let thread = sample_thread("Tier 5 Export");
        let runs = vec![
            sample_run(1, Some(1200), Some(300), None),
            sample_run(2, Some(800), Some(200), Some("rate limit hit")),
        ];
        // Deliberately pass out-of-order ordinals to verify chronological sorting.
        let items = vec![
            sample_item(
                10,
                TurnItemKind::Handoff,
                None,
                json!({
                    "from_model": {"model": "claude-sonnet-5-5"},
                    "to_model": {"model": "gpt-6-luna"},
                    "summary": "Handed off after tests passed."
                }),
            ),
            sample_item(
                1,
                TurnItemKind::UserMessage,
                Some("Please check build"),
                json!({"text": "Please run cargo check and summarize.", "attachment_ids": ["att-1"]}),
            ),
            sample_item(
                2,
                TurnItemKind::Reasoning,
                None,
                json!({"text": "I should inspect Cargo.toml and run cargo check."}),
            ),
            sample_item(
                3,
                TurnItemKind::ToolCall,
                Some("Read"),
                json!({"name": "Read", "input": {"file_path": "Cargo.toml"}}),
            ),
            sample_item(
                4,
                TurnItemKind::ToolResult,
                None,
                json!({"content": [{"type": "text", "text": "[workspace]\nmembers = []"}]}),
            ),
            sample_item(
                5,
                TurnItemKind::CommandExecution,
                Some("cargo check"),
                json!({"command": "cargo check", "exit_code": 0, "output": "Finished dev profile"}),
            ),
            sample_item(
                6,
                TurnItemKind::FileChange,
                None,
                json!({
                    "changes": [{"path": "crates/server/src/export.rs", "kind": "create"}],
                    "diff": "+pub(crate) fn render_thread_markdown() {}"
                }),
            ),
            sample_item(
                7,
                TurnItemKind::SystemNotice,
                None,
                json!({"message": "Auto-verify succeeded."}),
            ),
            sample_item(
                8,
                TurnItemKind::Subagent,
                None,
                json!({
                    "child_thread_id": "child-123",
                    "title": "Audit Worker",
                    "status": "completed",
                    "provider": "codex",
                    "model": "gpt-6-luna",
                    "summary": "All checks green."
                }),
            ),
            sample_item(
                9,
                TurnItemKind::AssistantMessage,
                None,
                json!({"text": "Everything builds cleanly."}),
            ),
        ];

        let md = render_thread_markdown(&thread, &runs, &items);

        assert!(md.contains("- **Branch**: `feat/export`"));
        assert!(md.contains("- **Runs**: 2"));
        assert!(md.contains("- **Total Tokens**: 2500 (2000 input / 500 output)"));
        assert!(md.contains("## Run Errors"));
        assert!(md.contains("**Run 2** (failed): rate limit hit"));

        assert!(md.contains("## User (2026-11-25T10:00:01Z)"));
        assert!(md.contains("Please run cargo check and summarize."));
        assert!(md.contains("_Attachments: 1_"));

        assert!(md.contains("<details>\n<summary>Reasoning (2026-11-25T10:00:02Z)</summary>"));
        assert!(md.contains("I should inspect Cargo.toml and run cargo check."));
        assert!(md.contains("</details>"));

        assert!(md.contains("### Tool Call: Read (2026-11-25T10:00:03Z)"));
        assert!(md.contains("\"file_path\": \"Cargo.toml\""));

        assert!(md.contains("### Tool Result (2026-11-25T10:00:04Z)"));
        assert!(md.contains("[workspace]\nmembers = []"));

        assert!(md.contains("### Command Execution (2026-11-25T10:00:05Z)"));
        assert!(md.contains("- **Command**: `cargo check`"));
        assert!(md.contains("- **Exit Code**: 0"));
        assert!(md.contains("```\nFinished dev profile\n```"));

        assert!(md.contains("### File Change (2026-11-25T10:00:06Z)"));
        assert!(md.contains("- `crates/server/src/export.rs` (create)"));
        assert!(md.contains("```diff\n+pub(crate) fn render_thread_markdown() {}\n```"));

        assert!(md.contains("### System Notice (2026-11-25T10:00:07Z)"));
        assert!(md.contains("> Auto-verify succeeded."));

        assert!(md.contains("### Subagent: Audit Worker (2026-11-25T10:00:08Z)"));
        assert!(md.contains("- **Status**: completed"));
        assert!(md.contains("- **Model**: codex/gpt-6-luna"));
        assert!(md.contains("All checks green."));

        assert!(md.contains("## Assistant (2026-11-25T10:00:09Z)"));
        assert!(md.contains("Everything builds cleanly."));

        assert!(md.contains("### Context Handoff (2026-11-25T10:00:10Z)"));
        assert!(md.contains("- **Transition**: claude-sonnet-5-5 -> gpt-6-luna"));
        assert!(md.contains("Handed off after tests passed."));

        // Verify chronological ordering in the rendered output.
        let pos_user = md.find("## User (").unwrap();
        let pos_reasoning = md.find("<summary>Reasoning (").unwrap();
        let pos_cmd = md.find("### Command Execution (").unwrap();
        let pos_assistant = md.find("## Assistant (").unwrap();
        let pos_handoff = md.find("### Context Handoff (").unwrap();
        assert!(pos_user < pos_reasoning);
        assert!(pos_reasoning < pos_cmd);
        assert!(pos_cmd < pos_assistant);
        assert!(pos_assistant < pos_handoff);
    }

    #[test]
    fn redacts_credentials_across_all_rendered_sections() {
        let secrets = [
            "sk-1234567890abcdef_-",
            "ghp_1234567890abcdef",
            "Bearer abcdefgh012345._~+/-",
            "t3mcp_0123456789abcdef",
            "eyJ1234567890.ABCDEFGHIJ.KLMNOPQRST",
            "xoxb-12345-fixturetoken",
            "AIzaSyD1234567890abcdef",
            "AKIA1234567890ABCDEF",
        ];

        let thread = sample_thread(&format!("Debug {}", secrets[0]));
        let runs = vec![sample_run(
            1,
            Some(10),
            Some(20),
            Some(&format!("auth failed with {}", secrets[1])),
        )];
        let items = vec![
            sample_item(
                1,
                TurnItemKind::UserMessage,
                None,
                json!({"text": format!("My key is {}", secrets[0])}),
            ),
            sample_item(
                2,
                TurnItemKind::AssistantMessage,
                None,
                json!({"text": format!("Using header {}", secrets[2])}),
            ),
            sample_item(
                3,
                TurnItemKind::Reasoning,
                None,
                json!({"text": format!("Token {} seen", secrets[3])}),
            ),
            sample_item(
                4,
                TurnItemKind::CommandExecution,
                None,
                json!({
                    "command": format!("curl -H '{}' https://example.invalid", secrets[2]),
                    "exit_code": 1,
                    "stdout": format!("jwt={}", secrets[4]),
                    "stderr": format!("slack={}", secrets[5]),
                }),
            ),
            sample_item(
                5,
                TurnItemKind::ToolCall,
                None,
                json!({"name": "Http", "input": {"api_key": secrets[6]}}),
            ),
            sample_item(
                6,
                TurnItemKind::ToolResult,
                None,
                json!({"is_error": true, "error": format!("denied for {}", secrets[7])}),
            ),
            sample_item(
                7,
                TurnItemKind::SystemNotice,
                None,
                json!({"message": format!("Notice leaking {}", secrets[1])}),
            ),
            sample_item(
                8,
                TurnItemKind::Subagent,
                None,
                json!({"title": format!("Child {}", secrets[0]), "summary": format!("Saw {}", secrets[3])}),
            ),
        ];

        let md = render_thread_markdown(&thread, &runs, &items);

        for secret in secrets {
            assert!(
                !md.contains(secret),
                "exported markdown leaked credential: {secret}"
            );
        }
        assert!(md.contains("[REDACTED]"));
        assert!(md.contains("### Tool Result (Error)"));
    }

    #[test]
    fn command_output_containing_triple_backticks_uses_longer_fence() {
        let thread = sample_thread("Fence Safety");
        let items = vec![sample_item(
            1,
            TurnItemKind::CommandExecution,
            None,
            json!({
                "command": "echo '```code```'",
                "exit_code": 0,
                "output": "before\n```rust\nfn main() {}\n```\nafter"
            }),
        )];

        let md = render_thread_markdown(&thread, &[], &items);
        assert!(md.contains("````\nbefore\n```rust\nfn main() {}\n```\nafter\n````"));
    }
}
