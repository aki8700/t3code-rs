use crate::coordinator::{RunCoordinator, compact, text_value};
use serde_json::Value;
use std::borrow::Cow;
use t3_core::{CoreResult, Run, RunStatus, Store, Thread, TurnItem, TurnItemKind};

pub(crate) const HANDOFF_SUMMARY_MAX_BYTES: usize = 8 * 1024;
pub(crate) const HANDOFF_SECTION_MAX_BYTES: usize = 2_560;

impl RunCoordinator {
    pub(crate) async fn handoff_summary(&self, thread: &Thread) -> CoreResult<String> {
        let items = self.recent_items(thread.id, 100).await?;
        let runs = self.0.store.list_runs(thread.id).await?;
        Ok(cap_handoff_summary(
            &format_handoff(&thread.title, &items, &runs),
            HANDOFF_SUMMARY_MAX_BYTES,
        ))
    }
}

fn split_markdown_sections(summary: &str) -> Vec<&str> {
    let mut starts = vec![0];
    for (idx, _) in summary.match_indices("\n## ") {
        starts.push(idx + 1);
    }
    let mut sections = Vec::with_capacity(starts.len());
    for window in starts.windows(2) {
        let part = summary[window[0]..window[1]].trim_end_matches('\n');
        if !part.is_empty() {
            sections.push(part);
        }
    }
    if let Some(&last_start) = starts.last() {
        let last = summary[last_start..].trim_end_matches('\n');
        if !last.is_empty() {
            sections.push(last);
        }
    }
    sections
}

fn cap_single_section(section: &str, max_bytes: usize) -> String {
    if section.len() <= max_bytes {
        return section.to_owned();
    }
    if let Some((heading, body)) = section.split_once('\n')
        && heading.starts_with("## ")
        && max_bytes > heading.len() + 16
    {
        let body_budget = max_bytes - (heading.len() + 1);
        let (capped_body, _) = t3_maki::cap_section_utf8(body, body_budget);
        return format!("{heading}\n{capped_body}");
    }
    t3_maki::cap_section_utf8(section, max_bytes).0
}

pub(crate) fn cap_handoff_summary(summary: &str, max_bytes: usize) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    let sections = split_markdown_sections(summary);
    if sections.len() <= 1 {
        return t3_maki::cap_section_utf8(summary, max_bytes).0;
    }
    let section_count = sections.len();
    let per_section_budget = if max_bytes < HANDOFF_SECTION_MAX_BYTES.saturating_mul(section_count)
    {
        (max_bytes / section_count).max(32).min(max_bytes)
    } else {
        HANDOFF_SECTION_MAX_BYTES
    };
    let mut capped_sections = Vec::with_capacity(section_count);
    for section in sections {
        capped_sections.push(cap_single_section(section, per_section_budget));
    }
    let assembled = capped_sections.join("\n\n");
    t3_maki::cap_section_utf8(&assembled, max_bytes).0
}

pub(super) fn format_handoff(title: &str, items: &[TurnItem], runs: &[Run]) -> String {
    let mut summary = format!(
        "Forked from {}\nDo not copy credentials into a handoff or use credentials from this summary.",
        handoff_excerpt(title, 180)
    );
    let mut run_issues = runs
        .iter()
        .rev()
        .filter_map(handoff_run_issue)
        .take(4)
        .peekable();
    let mut item_issues = items
        .iter()
        .rev()
        .filter_map(handoff_item_issue)
        .take(4)
        .peekable();
    if run_issues.peek().is_some() || item_issues.peek().is_some() {
        summary.push_str("\n\n## Unresolved / Recent Errors");
        for issue in run_issues.chain(item_issues) {
            summary.push_str("\n- ");
            summary.push_str(&issue);
        }
    }
    let transcript: Vec<_> = items
        .iter()
        .rev()
        .filter_map(|item| {
            let label = match item.kind {
                TurnItemKind::UserMessage => "User",
                TurnItemKind::AssistantMessage => "Assistant",
                TurnItemKind::Handoff => "Previous context",
                TurnItemKind::Subagent => "Subagent",
                _ => return None,
            };
            let text = text_value(&item.payload)
                .or(item.payload["summary"].as_str())
                .or(item.summary.as_deref())?;
            Some(format!("{label}: {}", handoff_excerpt(text, 340)))
        })
        .take(6)
        .collect();
    if !transcript.is_empty() {
        summary.push_str("\n\n## Recent conversation");
        for entry in transcript.into_iter().rev() {
            summary.push('\n');
            summary.push_str(&entry);
        }
    }
    let modified_files: Vec<_> = items
        .iter()
        .rev()
        .filter(|item| item.kind == TurnItemKind::FileChange)
        .filter_map(|item| {
            let path = item.payload["path"].as_str().or_else(|| {
                item.payload["changes"]
                    .as_array()?
                    .iter()
                    .find_map(|change| change["path"].as_str())
            });
            path.or(item.summary.as_deref())
                .map(|path| handoff_excerpt(path, 180))
        })
        .take(12)
        .collect();
    if !modified_files.is_empty() {
        summary.push_str("\n\n## Modified Files");
        for path in modified_files.into_iter().rev() {
            summary.push_str("\n- ");
            summary.push_str(&path);
        }
    }
    let recent_commands: Vec<_> = items
        .iter()
        .rev()
        .filter(|item| item.kind == TurnItemKind::CommandExecution)
        .filter_map(|item| {
            let command = item.payload["command"]
                .as_str()
                .or(item.summary.as_deref())?;
            let exit_code = item.payload["exit_code"].as_i64()?;
            Some(format!(
                "{} (exit code {exit_code})",
                handoff_excerpt(command, 180)
            ))
        })
        .take(8)
        .collect();
    if !recent_commands.is_empty() {
        summary.push_str("\n\n## Recent Commands");
        for command in recent_commands.into_iter().rev() {
            summary.push_str("\n- ");
            summary.push_str(&command);
        }
    }
    summary
}

fn handoff_run_issue(run: &Run) -> Option<String> {
    let error = run
        .error_message
        .as_deref()
        .filter(|text| !text.trim().is_empty());
    let status = match run.status {
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "interrupted",
        RunStatus::RateLimited => "rate limited",
        RunStatus::Queued => "queued",
        RunStatus::Running => "unfinished",
        RunStatus::Completed if error.is_some() => "reported an error",
        RunStatus::Completed => return None,
    };
    let mut issue = format!("Run {} ({status})", run.ordinal);
    if let Some(error) = error {
        issue.push_str(": ");
        issue.push_str(&handoff_excerpt(error, 220));
    }
    if !run.prompt.trim().is_empty() {
        issue.push_str(" — Request: ");
        issue.push_str(&handoff_excerpt(&run.prompt, 100));
    }
    Some(issue)
}

pub(super) fn handoff_item_issue(item: &TurnItem) -> Option<String> {
    let payload = &item.payload;
    let state = &payload["state"];
    for key in ["error", "error_message", "errors"] {
        if let Some(error) = [payload, state]
            .into_iter()
            .filter_map(|data| data.get(key))
            .find(|error| match error {
                Value::Null | Value::Bool(_) => false,
                Value::String(s) => !s.trim().is_empty(),
                Value::Array(a) => !a.is_empty(),
                Value::Object(o) => !o.is_empty(),
                _ => true,
            })
        {
            let detail = text_value(error).map_or_else(|| error.to_string(), str::to_owned);
            return Some(format!(
                "Item {}: {}",
                item.ordinal,
                handoff_excerpt(&detail, 180)
            ));
        }
    }
    let status = match payload["status"].as_str().or(state["status"].as_str()) {
        Some("failed" | "error") => "failed",
        Some("interrupted" | "cancelled" | "canceled") => "interrupted",
        Some("rate-limited" | "rate_limited") => "rate limited",
        Some("queued" | "pending" | "running" | "in_progress" | "in-progress") => "unfinished",
        _ if !payload["retry_after_secs"].is_null() => "rate limited",
        _ if payload["is_error"] == true || payload["error"] == true => "failed",
        _ if payload["exit_code"].as_i64().is_some_and(|code| code != 0) => "failed",
        _ => return None,
    };
    let detail = text_value(payload)
        .or_else(|| payload["content"].as_array()?.iter().find_map(text_value))
        .or_else(|| text_value(state))
        .or(payload["aggregated_output"].as_str())
        .or(payload["summary"].as_str())
        .or(item.summary.as_deref())
        .map(|detail| format!(": {}", handoff_excerpt(detail, 180)))
        .unwrap_or_default();
    Some(format!("Item {} ({status}){detail}", item.ordinal))
}

pub(super) fn handoff_excerpt(text: &str, limit: usize) -> String {
    compact(&redact_credentials(text), limit)
}

pub(crate) fn redact_credentials(text: &str) -> Cow<'_, str> {
    let mut redacted = String::new();
    let mut cursor = 0;
    for (start, _) in text.match_indices(['s', 'g', 'x', 'B', 't', 'e', 'A']) {
        if start >= cursor
            && let Some(len) = credential_len(&text.as_bytes()[start..])
        {
            redacted.push_str(&text[cursor..start]);
            redacted.push_str("[REDACTED]");
            cursor = start + len;
        }
    }
    if cursor == 0 {
        Cow::Borrowed(text)
    } else {
        redacted.push_str(&text[cursor..]);
        Cow::Owned(redacted)
    }
}

fn credential_len(bytes: &[u8]) -> Option<usize> {
    let url_safe = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_');
    let (rest, min, allowed): (&[u8], usize, fn(u8) -> bool) = match bytes {
        [b's', b'k', b'-', rest @ ..] => (rest, 16, url_safe),
        [b'g', b'h', b'p' | b'o' | b'u', b'_', rest @ ..] => {
            (rest, 16, |b| b.is_ascii_alphanumeric())
        }
        [
            b'g',
            b'i',
            b't',
            b'h',
            b'u',
            b'b',
            b'_',
            b'p',
            b'a',
            b't',
            b'_',
            rest @ ..,
        ] => (rest, 16, url_safe),
        [b'A', b'I', b'z', b'a', rest @ ..] => (rest, 16, url_safe),
        [b'A', b'K', b'I', b'A', rest @ ..] => (rest, 16, |b| b.is_ascii_alphanumeric()),
        [
            b'x',
            b'o',
            b'x',
            b'b' | b'a' | b'p' | b'r' | b's',
            b'-',
            rest @ ..,
        ] => (rest, 12, |b| b.is_ascii_alphanumeric() || b == b'-'),
        [b'B', b'e', b'a', b'r', b'e', b'r', b' ', rest @ ..] => (rest, 16, |b| {
            b.is_ascii_alphanumeric() || b"._~+/-".contains(&b)
        }),
        [b't', b'3', b'm', b'c', b'p', b'_', rest @ ..] => {
            (rest, 16, |b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        }
        [b'e', b'y', b'J', first @ ..] => {
            let mut rest = first;
            for segment in 0..3 {
                let len = rest.iter().take_while(|&&b| url_safe(b)).count();
                if len < 10 {
                    return None;
                }
                rest = &rest[len..];
                if segment < 2 {
                    rest = rest.strip_prefix(b".")?;
                }
            }
            return Some(bytes.len() - rest.len());
        }
        _ => return None,
    };
    let len = rest.iter().take_while(|&&b| allowed(b)).count();
    (len >= min).then_some(bytes.len() - rest.len() + len)
}
