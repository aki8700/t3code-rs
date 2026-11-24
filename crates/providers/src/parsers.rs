use serde_json::Value;
use std::collections::{HashMap, HashSet};
use t3_core::{
    protocol::StreamEvent,
    types::{ProviderId, TurnItemKind},
};

pub(crate) struct Parser {
    provider: ProviderId,
    session: Option<String>,
    partial_text: bool,
    partial_reasoning: bool,
    codex_text: HashMap<String, String>,
    codex_calls: HashSet<String>,
    opencode_step_finished: bool,
    opencode_usage: (u64, u64),
}

impl Parser {
    pub(crate) fn new(provider: ProviderId) -> Self {
        Self {
            provider,
            session: None,
            partial_text: false,
            partial_reasoning: false,
            codex_text: HashMap::new(),
            codex_calls: HashSet::new(),
            opencode_step_finished: false,
            opencode_usage: (0, 0),
        }
    }

    pub(crate) fn parse(&mut self, event: &Value) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        let session = match self.provider {
            ProviderId::Claude => string(event, "session_id"),
            ProviderId::Codex => {
                string(event, "thread_id").filter(|_| kind(event) == "thread.started")
            }
            ProviderId::Antigravity => {
                first_string(event, &["conversation_id", "session_id", "conversationId"])
            }
            ProviderId::OpenCode => first_string(event, &["sessionID", "session_id"]),
            ProviderId::Mock => None,
        };
        if let Some(session) = session.filter(|session| !session.is_empty())
            && self.session.as_deref() != Some(session)
        {
            self.session = Some(session.to_owned());
            out.push(StreamEvent::SessionStarted {
                provider_session_id: session.to_owned(),
            });
        }

        match self.provider {
            ProviderId::Claude => self.claude(event, &mut out),
            ProviderId::Codex => self.codex(event, &mut out),
            ProviderId::Antigravity => self.antigravity(event, &mut out),
            ProviderId::OpenCode => self.opencode(event, &mut out),
            ProviderId::Mock => {}
        }
        out
    }

    pub(crate) fn finish(&self) -> Option<StreamEvent> {
        // OpenCode emits one step_finish per model step, then closes stdout at the end of the run.
        self.opencode_step_finished
            .then_some(StreamEvent::Completed { final_text: None })
    }

    fn claude(&mut self, event: &Value, out: &mut Vec<StreamEvent>) {
        match kind(event) {
            "stream_event" => self.claude_partial(&event["event"], out),
            "assistant" | "user" => self.claude_message(event, out),
            "result" => stream_json_result(event, out),
            "error" | "failed" | "rate_limit" | "rate_limited" => out.push(error_event(event)),
            "rate_limit_event"
                if matches!(
                    string(&event["rate_limit_info"], "status"),
                    Some("rejected" | "rate_limited")
                ) =>
            {
                out.push(error_event(event));
            }
            _ => {}
        }
    }

    fn claude_partial(&mut self, event: &Value, out: &mut Vec<StreamEvent>) {
        match kind(event) {
            "message_start" => {
                self.partial_text = false;
                self.partial_reasoning = false;
            }
            "content_block_delta" => {
                let delta = &event["delta"];
                match kind(delta) {
                    "text_delta" => {
                        self.partial_text |= push_delta(out, string(delta, "text"), false)
                    }
                    "thinking_delta" => {
                        self.partial_reasoning |=
                            push_delta(out, first_string(delta, &["thinking", "text"]), true)
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn claude_message(&self, event: &Value, out: &mut Vec<StreamEvent>) {
        let assistant = kind(event) == "assistant";
        if let Some(content) = event.pointer("/message/content").and_then(Value::as_array) {
            for block in content {
                match kind(block) {
                    "text" if assistant && !self.partial_text => {
                        push_delta(out, string(block, "text"), false);
                    }
                    "thinking" if assistant && !self.partial_reasoning => {
                        push_delta(out, first_string(block, &["thinking", "text"]), true);
                    }
                    "tool_use" | "server_tool_use" => push_item(out, TurnItemKind::ToolCall, block),
                    "tool_result" | "mcp_tool_result" | "web_search_tool_result" => {
                        push_item(out, TurnItemKind::ToolResult, block);
                    }
                    _ => {}
                }
            }
        } else if assistant && !self.partial_text {
            push_delta(
                out,
                event.pointer("/message/content").and_then(Value::as_str),
                false,
            );
        }
    }

    fn codex(&mut self, event: &Value, out: &mut Vec<StreamEvent>) {
        match kind(event) {
            "item.started" | "item.updated" | "item.completed" => {
                self.codex_item(&event["item"], kind(event) == "item.completed", out);
            }
            "turn.completed" => {
                push_usage(out, &event["usage"]);
                out.push(StreamEvent::Completed { final_text: None });
            }
            "error"
                if string(event, "message")
                    .is_some_and(|message| message.starts_with("Reconnecting...")) => {}
            "turn.failed" | "error" | "rate_limit" | "rate_limited" => out.push(error_event(event)),
            _ => {}
        }
    }

    fn codex_item(&mut self, item: &Value, complete: bool, out: &mut Vec<StreamEvent>) {
        let item_type = kind(item);
        let id = string(item, "id");
        match item_type {
            "agent_message" | "reasoning" => {
                if let Some(text) = string(item, "text") {
                    let previous = id.and_then(|id| self.codex_text.get(id));
                    let delta = previous
                        .and_then(|prev| text.strip_prefix(prev))
                        .unwrap_or(text);
                    push_delta(out, Some(delta), item_type == "reasoning");
                    if let Some(id) = id {
                        if complete {
                            self.codex_text.remove(id);
                        } else {
                            self.codex_text.insert(id.to_owned(), text.to_owned());
                        }
                    }
                }
            }
            "mcp_tool_call" | "dynamic_tool_call" | "collab_tool_call" => {
                if id.is_none_or(|id| self.codex_calls.insert(id.to_owned())) {
                    push_item(out, TurnItemKind::ToolCall, item);
                }
                if complete {
                    push_item(out, TurnItemKind::ToolResult, item);
                    if let Some(id) = id {
                        self.codex_calls.remove(id);
                    }
                }
            }
            "command_execution" if complete => push_item(out, TurnItemKind::CommandExecution, item),
            "file_change" if complete => push_item(out, TurnItemKind::FileChange, item),
            "web_search" if complete => push_item(out, TurnItemKind::ToolCall, item),
            _ => {}
        }
    }

    fn antigravity(&mut self, event: &Value, out: &mut Vec<StreamEvent>) {
        match kind(event) {
            "stream_event" => self.claude_partial(&event["event"], out),
            "assistant" | "user" if event["message"].is_object() => self.claude_message(event, out),
            "assistant" | "assistant_delta" | "text" | "text_delta" | "content" => {
                push_delta(out, chunk_text(event), false);
            }
            "reasoning" | "reasoning_delta" | "thinking" | "thinking_delta" | "thought" => {
                push_delta(out, chunk_text(event), true);
            }
            "message" if matches!(string(event, "role"), Some("assistant" | "model")) => {
                push_delta(
                    out,
                    chunk_text(event),
                    event["thought"].as_bool() == Some(true),
                );
            }
            "tool_use" | "tool_call" => push_item(out, TurnItemKind::ToolCall, event),
            "tool_result" | "tool_response" => push_item(out, TurnItemKind::ToolResult, event),
            "command_execution" => push_item(out, TurnItemKind::CommandExecution, event),
            "file_change" => push_item(out, TurnItemKind::FileChange, event),
            "usage" => push_usage(out, event.get("usage").unwrap_or(event)),
            "result" | "completed" | "done" => stream_json_result(event, out),
            "error" | "failed" | "rate_limit" | "rate_limited" => out.push(error_event(event)),
            _ => {}
        }
    }

    fn opencode(&mut self, event: &Value, out: &mut Vec<StreamEvent>) {
        let part = event.get("part").unwrap_or(event);
        match kind(event) {
            "step_start" => self.opencode_step_finished = false,
            "text" | "reasoning" => {
                push_delta(out, string(part, "text"), kind(event) == "reasoning");
            }
            "tool_use" => {
                push_item(out, TurnItemKind::ToolCall, part);
                let state = &part["state"];
                if matches!(string(state, "status"), Some("completed" | "error"))
                    || state.get("output").is_some()
                {
                    push_item(out, TurnItemKind::ToolResult, part);
                }
            }
            "tool_result" => push_item(out, TurnItemKind::ToolResult, part),
            "command_execution" => push_item(out, TurnItemKind::CommandExecution, part),
            "file_change" => push_item(out, TurnItemKind::FileChange, part),
            "step_finish" => {
                self.opencode_step_finished = true;
                if let Some((input, output)) =
                    token_counts(part.get("tokens").or(event.get("usage")).unwrap_or(part))
                {
                    self.opencode_usage.0 = self.opencode_usage.0.saturating_add(input);
                    self.opencode_usage.1 = self.opencode_usage.1.saturating_add(output);
                    out.push(StreamEvent::Usage {
                        input_tokens: self.opencode_usage.0,
                        output_tokens: self.opencode_usage.1,
                    });
                }
            }
            "error" | "failed" | "rate_limit" | "rate_limited" => out.push(error_event(event)),
            _ => {}
        }
    }
}

fn kind(value: &Value) -> &str {
    string(value, "type").unwrap_or("")
}

fn string<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}

fn first_string<'a>(value: &'a Value, fields: &[&str]) -> Option<&'a str> {
    fields.iter().find_map(|field| string(value, field))
}

fn chunk_text(value: &Value) -> Option<&str> {
    first_string(
        value,
        &["text", "thinking", "reasoning", "content", "delta"],
    )
    .or_else(|| first_string(&value["delta"], &["text", "thinking", "content"]))
}

fn push_delta(out: &mut Vec<StreamEvent>, text: Option<&str>, reasoning: bool) -> bool {
    let Some(text) = text.filter(|text| !text.is_empty()) else {
        return false;
    };
    let text = text.to_owned();
    out.push(if reasoning {
        StreamEvent::ReasoningDelta { text }
    } else {
        StreamEvent::AssistantDelta { text }
    });
    true
}

fn push_item(out: &mut Vec<StreamEvent>, kind: TurnItemKind, value: &Value) {
    out.push(StreamEvent::Item {
        kind,
        summary: first_string(value, &["name", "tool", "command", "title", "path"])
            .map(str::to_owned),
        payload: value.clone(),
    });
}

fn token_counts(value: &Value) -> Option<(u64, u64)> {
    let input = ["input_tokens", "input", "inputTokens", "promptTokenCount"]
        .into_iter()
        .find_map(|key| value[key].as_u64());
    let output = [
        "output_tokens",
        "output",
        "outputTokens",
        "candidatesTokenCount",
    ]
    .into_iter()
    .find_map(|key| value[key].as_u64());
    (input.is_some() || output.is_some()).then_some((input.unwrap_or(0), output.unwrap_or(0)))
}

fn push_usage(out: &mut Vec<StreamEvent>, value: &Value) {
    if let Some((input_tokens, output_tokens)) = token_counts(value) {
        out.push(StreamEvent::Usage {
            input_tokens,
            output_tokens,
        });
    }
}

fn stream_json_result(event: &Value, out: &mut Vec<StreamEvent>) {
    push_usage(
        out,
        event
            .get("usage")
            .or(event.get("stats"))
            .unwrap_or(&Value::Null),
    );
    let failed = ["subtype", "status"].into_iter().any(|field| {
        string(event, field).is_some_and(|name| {
            name.starts_with("error") || matches!(name, "failed" | "rate_limited")
        })
    });
    if failed || event["is_error"].as_bool() == Some(true) || !event["error"].is_null() {
        out.push(error_event(event));
    } else {
        out.push(StreamEvent::Completed {
            final_text: first_string(event, &["result", "text", "response"]).map(str::to_owned),
        });
    }
}

fn error_event(event: &Value) -> StreamEvent {
    let error = event.get("error").unwrap_or(event);
    let info = &event["rate_limit_info"];
    let message = error
        .as_str()
        .or_else(|| string(error, "message"))
        .or_else(|| string(&error["data"], "message"))
        .or_else(|| string(&error["data"]["error"], "message"))
        .or_else(|| first_string(event, &["message", "result"]))
        .map(str::to_owned)
        .or_else(|| {
            let errors: Vec<_> = event["errors"]
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .collect();
            (!errors.is_empty()).then(|| errors.join("\n"))
        })
        .or_else(|| first_string(error, &["code", "name", "type"]).map(str::to_owned))
        .unwrap_or_else(|| "provider reported an error".into());
    let rate_limit = [event, error, &error["data"], info]
        .into_iter()
        .any(|value| {
            ["code", "name", "type", "subtype", "status", "statusCode"]
                .into_iter()
                .any(|key| {
                    value[key].as_u64() == Some(429)
                        || string(value, key).is_some_and(is_rate_limit)
                })
        });
    let retry = [error, &error["data"], event, info]
        .into_iter()
        .find_map(retry_after);
    if rate_limit || is_rate_limit(&message) {
        StreamEvent::RateLimited {
            retry_after_secs: retry.unwrap_or(0),
            message,
        }
    } else {
        StreamEvent::Failed { message }
    }
}

fn retry_after(value: &Value) -> Option<u64> {
    [
        "retry_after_secs",
        "retry_after",
        "retryAfter",
        "retry-after",
    ]
    .into_iter()
    .find_map(|key| as_u64(&value[key]))
    .or_else(|| as_u64(&value["retry_after_ms"]).map(|ms| ms.div_ceil(1000)))
    .or_else(|| {
        as_u64(&value["resetsAt"])
            .map(|epoch| epoch.saturating_sub(chrono::Utc::now().timestamp().max(0) as u64))
    })
}

fn as_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn is_rate_limit(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    let normalized = lower.replace(['_', '-'], " ");
    matches!(
        normalized.as_str(),
        "429" | "resourceexhausted" | "ratelimiterror"
    ) || [
        "rate limit",
        "usage limit",
        "too many requests",
        "quota exceeded",
        "resource exhausted",
        "overloaded",
        "hit your limit",
        "http 429",
        "status 429",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

pub(crate) fn provider_error(message: String) -> StreamEvent {
    if is_rate_limit(&message) {
        StreamEvent::RateLimited {
            retry_after_secs: 0,
            message,
        }
    } else {
        StreamEvent::Failed { message }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(provider: ProviderId, lines: &str) -> Vec<Value> {
        let mut parser = Parser::new(provider);
        let mut events: Vec<_> = lines
            .lines()
            .filter(|line| !line.trim().is_empty())
            .flat_map(|line| parser.parse(&serde_json::from_str(line).unwrap()))
            .collect();
        events.extend(parser.finish());
        events
            .into_iter()
            .map(|event| serde_json::to_value(event).unwrap())
            .collect()
    }

    fn event_types(events: &[Value]) -> Vec<&str> {
        events.iter().map(kind).collect()
    }

    #[test]
    fn claude_ndjson_uses_partial_deltas_and_preserves_tool_payloads() {
        let events = fixture(
            ProviderId::Claude,
            r#"
{"type":"system","subtype":"init","session_id":"claude-s"}
{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"case 429"}}}
{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"consider"}}}
{"type":"assistant","session_id":"claude-s","message":{"content":[{"type":"text","text":"case 429"},{"type":"thinking","thinking":"consider"},{"type":"tool_use","id":"tool-1","name":"Read","input":{"file_path":"notes"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","content":"contents"}]}}
{"type":"result","subtype":"success","result":"case 429","usage":{"input_tokens":10,"output_tokens":5}}
"#,
        );
        assert_eq!(
            event_types(&events),
            [
                "session_started",
                "assistant_delta",
                "reasoning_delta",
                "item",
                "item",
                "usage",
                "completed"
            ]
        );
        assert_eq!(events[0]["provider_session_id"], "claude-s");
        assert_eq!(events[1]["text"], "case 429");
        assert_eq!(events[2]["text"], "consider");
        assert_eq!(events[3]["kind"], "tool_call");
        assert_eq!(events[3]["payload"]["input"]["file_path"], "notes");
        assert_eq!(events[4]["kind"], "tool_result");
        assert_eq!(
            events[5],
            json!({"type":"usage","input_tokens":10,"output_tokens":5})
        );
        assert_eq!(events[6]["final_text"], "case 429");
    }

    #[test]
    fn codex_ndjson_handles_snapshots_and_completed_items() {
        let events = fixture(
            ProviderId::Codex,
            r#"
{"type":"thread.started","thread_id":"codex-s"}
{"type":"item.updated","item":{"id":"1","type":"agent_message","text":"hel"}}
{"type":"item.completed","item":{"id":"1","type":"agent_message","text":"hello"}}
{"type":"item.completed","item":{"id":"2","type":"reasoning","text":"think"}}
{"type":"item.started","item":{"id":"3","type":"mcp_tool_call","tool":"read"}}
{"type":"item.completed","item":{"id":"3","type":"mcp_tool_call","tool":"read","result":{"content":[]}}}
{"type":"item.started","item":{"id":"4","type":"command_execution","command":"ls","status":"in_progress"}}
{"type":"item.completed","item":{"id":"4","type":"command_execution","command":"ls","exit_code":0}}
{"type":"item.completed","item":{"id":"5","type":"file_change","changes":[{"path":"notes","kind":"update"}]}}
{"type":"turn.completed","usage":{"input_tokens":11,"output_tokens":6}}
"#,
        );
        assert_eq!(
            event_types(&events),
            [
                "session_started",
                "assistant_delta",
                "assistant_delta",
                "reasoning_delta",
                "item",
                "item",
                "item",
                "item",
                "usage",
                "completed"
            ]
        );
        assert_eq!(events[0]["provider_session_id"], "codex-s");
        assert_eq!(events[1]["text"], "hel");
        assert_eq!(events[2]["text"], "lo");
        assert_eq!(events[3]["text"], "think");
        assert_eq!(events[4]["kind"], "tool_call");
        assert_eq!(events[5]["kind"], "tool_result");
        assert_eq!(events[6]["kind"], "command_execution");
        assert_eq!(events[6]["payload"]["exit_code"], 0);
        assert_eq!(events[7]["kind"], "file_change");
        assert_eq!(
            events[8],
            json!({"type":"usage","input_tokens":11,"output_tokens":6})
        );
    }

    #[test]
    fn antigravity_ndjson_handles_conversation_chunks_and_tools() {
        let events = fixture(
            ProviderId::Antigravity,
            r#"
{"type":"init","conversation_id":"agy-s"}
{"type":"assistant_delta","delta":{"text":"answer"}}
{"type":"thinking","thinking":"consider"}
{"type":"tool_call","name":"read","id":"c"}
{"type":"tool_result","id":"c","output":"ok"}
{"type":"command_execution","command":"pwd"}
{"type":"file_change","path":"notes"}
{"type":"result","status":"success","response":"answer","usage":{"inputTokens":8,"outputTokens":3}}
"#,
        );
        assert_eq!(
            event_types(&events),
            [
                "session_started",
                "assistant_delta",
                "reasoning_delta",
                "item",
                "item",
                "item",
                "item",
                "usage",
                "completed"
            ]
        );
        assert_eq!(events[0]["provider_session_id"], "agy-s");
        assert_eq!(events[1]["text"], "answer");
        assert_eq!(events[2]["text"], "consider");
        assert_eq!(events[3]["kind"], "tool_call");
        assert_eq!(events[4]["kind"], "tool_result");
        assert_eq!(events[5]["kind"], "command_execution");
        assert_eq!(events[6]["kind"], "file_change");
        assert_eq!(
            events[7],
            json!({"type":"usage","input_tokens":8,"output_tokens":3})
        );
        assert_eq!(events[8]["final_text"], "answer");
    }

    #[test]
    fn opencode_ndjson_accumulates_usage_until_end_of_stdout() {
        let events = fixture(
            ProviderId::OpenCode,
            r#"
{"type":"step_start","sessionID":"opencode-s"}
{"type":"reasoning","part":{"text":"think"}}
{"type":"tool_use","part":{"tool":"read","state":{"status":"completed","output":"ok"}}}
{"type":"step_finish","part":{"reason":"tool-calls","tokens":{"input":12,"output":4}}}
{"type":"step_start"}
{"type":"text","sessionID":"opencode-s","part":{"text":"answer"}}
{"type":"step_finish","part":{"reason":"stop","tokens":{"input":20,"output":5}}}
"#,
        );
        assert_eq!(
            event_types(&events),
            [
                "session_started",
                "reasoning_delta",
                "item",
                "item",
                "usage",
                "assistant_delta",
                "usage",
                "completed"
            ]
        );
        assert_eq!(events[0]["provider_session_id"], "opencode-s");
        assert_eq!(events[1]["text"], "think");
        assert_eq!(events[2]["kind"], "tool_call");
        assert_eq!(events[3]["kind"], "tool_result");
        assert_eq!(events[3]["payload"]["state"]["output"], "ok");
        assert_eq!(events[5]["text"], "answer");
        assert_eq!(
            events[6],
            json!({"type":"usage","input_tokens":32,"output_tokens":9})
        );
    }

    #[test]
    fn provider_specific_limit_and_error_events() {
        for (provider, limit, error) in [
            (
                ProviderId::Claude,
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","retry_after_secs":3}}"#,
                r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["fixture failure"]}"#,
            ),
            (
                ProviderId::Codex,
                r#"{"type":"turn.failed","error":{"message":"usage limit hit","retry_after_secs":3}}"#,
                r#"{"type":"turn.failed","error":{"message":"fixture failure"}}"#,
            ),
            (
                ProviderId::Antigravity,
                r#"{"type":"error","error":{"code":"RESOURCE_EXHAUSTED","retry_after_ms":2500}}"#,
                r#"{"type":"result","status":"failed","error":"fixture failure"}"#,
            ),
            (
                ProviderId::OpenCode,
                r#"{"type":"error","error":{"name":"APIError","data":{"message":"request failed","statusCode":429,"retry_after":3}}}"#,
                r#"{"type":"error","error":{"data":{"message":"fixture failure"}}}"#,
            ),
        ] {
            let limit = fixture(provider, limit);
            assert_eq!(event_types(&limit), ["rate_limited"]);
            assert_eq!(limit[0]["retry_after_secs"], 3);

            let error = fixture(provider, error);
            assert_eq!(
                error,
                [json!({"type":"failed","message":"fixture failure"})]
            );
        }
    }
}
