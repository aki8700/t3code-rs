//! Direct, accessibility-only access to Gemini for Mac chats.
use crate::{ServerState, shell};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use t3_core::{CoreError, CoreResult, Store};
use tokio::{
    process::Command,
    sync::Mutex,
    time::{Instant, sleep, timeout},
};
use uuid::Uuid;

const APP: &str = "com.google.GeminiMacOS";
static SESSION: Mutex<()> = Mutex::const_new(());

#[derive(Deserialize)]
pub(crate) struct Request {
    prompt: String,
}

pub(crate) async fn ask(
    state: &ServerState,
    id: Uuid,
    req: Request,
) -> CoreResult<shell::ShellExecOutput> {
    let options = Options::parse(&req.prompt)?;
    let prompt = options.prompt;
    let thread = state.store.get_thread(id).await?;
    if thread.archived {
        return Err(CoreError::InvalidArgument("thread is archived".into()));
    }
    if !state.config.computer_use_enabled {
        return Err(failure("T3 の computer_use_enabled を有効にしてください"));
    }
    let bin = state
        .config
        .resolved_cua_bin()
        .ok_or_else(|| failure("SwiftCUA をインストールしてください"))?;
    let _guard = SESSION
        .try_lock()
        .map_err(|_| failure("Gemini アプリは別の質問を処理中です"))?;
    let started_at = Utc::now();
    let started = Instant::now();
    let mut desktop = Desktop {
        bin: &bin,
        window: None,
    };
    let saved = state.store.gemini_app_session(id).await?;
    let model = options
        .model
        .or_else(|| saved.as_ref().map(|entry| entry.1.as_str()))
        .unwrap_or("hydrogen");
    let effort = options.effort.unwrap_or_else(|| {
        if model == "auto" {
            "app"
        } else {
            saved
                .as_ref()
                .map(|entry| entry.2.as_str())
                .unwrap_or("low")
        }
    });
    let resume = saved
        .as_ref()
        .filter(|_| !options.new)
        .map(|entry| entry.0.as_str());
    let (conversation, prior_matches) = send(&mut desktop, prompt, resume, model, effort).await?;
    state
        .store
        .bind_gemini_app_session(id, conversation, model.into(), effort.into())
        .await?;
    if resume.is_none() {
        desktop.sidebar(false).await?;
    }
    let answer = wait_for_answer(&desktop, prompt, prior_matches).await?;
    let mut result = shell::ShellExecOutput {
        command: format!("/gemini --model {model} --effort {effort} {prompt}"),
        cwd: String::new(),
        exit_code: Some(0),
        stdout: answer,
        stderr: String::new(),
        duration_ms: started.elapsed().as_millis() as u64,
        timed_out: false,
        cancelled: false,
        truncated: false,
        item_id: None,
    };
    shell::persist_thread_command_result(
        state,
        &thread,
        started_at,
        Utc::now(),
        &mut result,
        "gemini_app",
    )
    .await?;
    Ok(result)
}

fn failure(message: &str) -> CoreError {
    CoreError::Provider(format!("Gemini Mac: {message}"))
}

struct Desktop<'a> {
    bin: &'a Path,
    window: Option<u64>,
}

impl Desktop<'_> {
    async fn call(&self, method: &str, mut params: Value) -> CoreResult<Value> {
        params["app"] = json!(APP);
        if let Some(window) = self.window {
            params["windowId"] = json!(window);
        }
        let output = timeout(
            Duration::from_secs(8),
            Command::new(self.bin)
                .kill_on_drop(true)
                .args(["call", method, &params.to_string()])
                .output(),
        )
        .await
        .map_err(|_| failure("SwiftCUA が応答しません"))?
        .map_err(|_| failure("SwiftCUA を実行できません"))?;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            let code = error
                .strip_prefix('[')
                .and_then(|line| line.split_once(']'))
                .and_then(|(code, _)| code.parse::<i32>().ok());
            let hint = code
                .map(|code| format!(" (エラー {code})"))
                .unwrap_or_default();
            return Err(failure(&format!(
                "SwiftCUA の {method} が失敗しました{hint}"
            )));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|_| failure("SwiftCUA の応答を読み取れません"))
    }

    async fn snapshot(&self) -> CoreResult<Value> {
        // SwiftUI briefly retracts accessibility windows while changing views.
        for _ in 0..3 {
            if let Ok(state) = self.read_snapshot().await {
                return Ok(state);
            }
            sleep(Duration::from_millis(200)).await;
        }
        self.read_snapshot().await
    }

    async fn read_snapshot(&self) -> CoreResult<Value> {
        let state = self
            .call(
                "getAppState",
                json!({"includeScreenshot":false,"disableDiff":true,"limit":1600}),
            )
            .await?;
        let window = state
            .pointer("/state/info/window/id")
            .and_then(Value::as_u64)
            .ok_or_else(|| failure("Gemini アプリのメインウインドウを開いてください"))?;
        if self.window.is_some_and(|previous| previous != window) {
            return Err(failure("操作対象のウインドウが変わりました"));
        }
        Ok(state)
    }

    async fn chat_window(&mut self) -> CoreResult<Value> {
        let initial = self.snapshot().await?;
        let primary = initial
            .pointer("/state/info/window/id")
            .and_then(Value::as_u64);
        let candidates = initial.pointer("/state/windows").and_then(Value::as_array);
        let others = candidates
            .into_iter()
            .flatten()
            .filter(|window| {
                window["layer"] == 0
                    && window["bounds"]["width"]
                        .as_f64()
                        .is_some_and(|width| width >= 300.0)
                    && window["bounds"]["height"]
                        .as_f64()
                        .is_some_and(|height| height >= 250.0)
            })
            .filter_map(|window| window["id"].as_u64())
            .filter(|id| Some(*id) != primary);
        for id in primary.into_iter().chain(others) {
            self.window = Some(id);
            let Ok(state) = self.snapshot().await else {
                continue;
            };
            let controls = nodes(&state);
            if chat_controls(&controls) {
                return Ok(state);
            }
            if let Some(sidebar) = button(&controls, "sidebar_toggle")
                && button(&controls, "sidebar_header_search").is_some()
            {
                // An expanded history can exhaust SwiftCUA's snapshot deadline.
                self.press(index(sidebar)?).await?;
                return self.wait_for(chat_controls).await;
            }
        }
        Err(failure("Gemini のチャット画面を開いてください"))
    }

    async fn wait_for(&self, ready: impl Fn(&[&Value]) -> bool) -> CoreResult<Value> {
        for _ in 0..4 {
            let state = self.snapshot().await?;
            if ready(&nodes(&state)) {
                return Ok(state);
            }
            sleep(Duration::from_millis(250)).await;
        }
        Err(failure("Gemini の入力欄または操作ボタンを確認できません"))
    }

    async fn sidebar(&self, open: bool) -> CoreResult<Value> {
        let state = self.snapshot().await?;
        let controls = nodes(&state);
        if button(&controls, "sidebar_header_search").is_some() == open {
            return Ok(state);
        }
        let toggle = button(&controls, "sidebar_toggle")
            .ok_or_else(|| failure("サイドバーを操作できません"))?;
        self.press(index(toggle)?).await?;
        self.wait_for(|controls| button(controls, "sidebar_header_search").is_some() == open)
            .await
    }

    async fn choose_model(&self, model: &str, effort: &str) -> CoreResult<()> {
        let current = self.sidebar(false).await?;
        let controls = nodes(&current);
        let selector = button(&controls, "tab_stop_model_selector")
            .ok_or_else(|| failure("モデル選択が見つかりません"))?;
        // A SwiftUI popover can open even when AXPress returns an error. Verify its state.
        if model_option(&controls, model).is_none() {
            let _ = self.press(index(selector)?).await;
        }
        let menu = self
            .wait_for(|controls| controls.iter().any(|node| node["role"] == "AXPopover"))
            .await?;
        let menu_controls = nodes(&menu);
        let choice = model_option(&menu_controls, model)
            .ok_or_else(|| failure("指定モデルはこの Gemini アカウントで選べません"))?;
        if !has_state(choice, "selected") {
            self.press(index(choice)?).await?;
            let selected = self
                .wait_for(|controls| {
                    button(controls, "tab_stop_model_selector")
                        .is_some_and(|node| model_label(node, model))
                })
                .await?;
            let selected_controls = nodes(&selected);
            let selector = button(&selected_controls, "tab_stop_model_selector")
                .ok_or_else(|| failure("モデル選択を確認できません"))?;
            if effort == "app" {
                return Ok(());
            }
            let _ = self.press(index(selector)?).await;
        } else if effort == "app" {
            return self.press(index(choice)?).await;
        }
        let effort_menu = self
            .wait_for(|controls| effort_entry(controls).is_some())
            .await?;
        let effort_controls = nodes(&effort_menu);
        let entry = effort_entry(&effort_controls)
            .ok_or_else(|| failure("このモデルでは思考レベルを変更できません"))?;
        let _ = self.press(index(entry)?).await;
        let level_menu = self
            .wait_for(|controls| {
                effort_option(controls, "low").is_some()
                    || effort_option(controls, "high").is_some()
            })
            .await?;
        let level_controls = nodes(&level_menu);
        let level = effort_option(&level_controls, effort)
            .ok_or_else(|| failure("指定した思考レベルは選べません"))?;
        self.press(index(level)?).await?;
        self.wait_for(|controls| !controls.iter().any(|node| node["role"] == "AXPopover"))
            .await?;
        Ok(())
    }

    async fn action(&self, method: &str, params: Value) -> CoreResult<()> {
        if self.call(method, params).await?["accepted"] == true {
            Ok(())
        } else {
            Err(failure("アクセシビリティ操作が拒否されました"))
        }
    }

    async fn press(&self, index: u64) -> CoreResult<()> {
        self.action("click", json!({"elementIndex":index,"strategy":"ax"}))
            .await
    }
}

fn walk<'a>(nodes: &'a [Value], out: &mut Vec<&'a Value>) {
    for node in nodes {
        out.push(node);
        if let Some(children) = node["children"].as_array() {
            walk(children, out);
        }
    }
}

fn nodes(state: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    if let Some(roots) = state
        .pointer("/state/accessibility/nodes")
        .and_then(Value::as_array)
    {
        walk(roots, &mut out);
    }
    out
}

fn composer<'a>(nodes: &[&'a Value]) -> Option<&'a Value> {
    nodes
        .iter()
        .copied()
        .find(|node| node["role"] == "AXTextArea")
}

fn button<'a>(nodes: &[&'a Value], id: &str) -> Option<&'a Value> {
    nodes.iter().copied().find(|node| {
        node["role"] == "AXButton" && node["identifier"] == id && has_state(node, "enabled")
    })
}

fn chat_controls(nodes: &[&Value]) -> bool {
    composer(nodes).is_some() && button(nodes, "tab_stop_model_selector").is_some()
}

fn temporary_chat(field: &Value) -> bool {
    field["description"]
        .as_str()
        .is_some_and(is_temporary_label)
}

fn is_temporary_label(text: &str) -> bool {
    text.contains("一時チャット") || text.to_ascii_lowercase().contains("temporary chat")
}

fn empty_field(field: &Value) -> bool {
    field["value"]
        .as_str()
        .is_none_or(|value| value.trim().is_empty())
}

fn has_state(node: &Value, state: &str) -> bool {
    node["states"]
        .as_array()
        .is_some_and(|states| states.iter().any(|item| item == state))
}

fn index(node: &Value) -> CoreResult<u64> {
    node["index"]
        .as_u64()
        .ok_or_else(|| failure("操作対象を特定できません"))
}

fn matches_prompt(value: &Value, prompt: &str) -> bool {
    value
        .as_str()
        .is_some_and(|value| value.split_whitespace().eq(prompt.split_whitespace()))
}

fn answer(nodes: &[&Value], prompt: &str) -> Option<String> {
    let question = nodes.iter().rposition(|node| {
        node["role"] == "AXHeading" && matches_prompt(&node["description"], prompt)
    })?;
    for row in nodes[question + 1..]
        .iter()
        .filter(|node| node["role"] == "AXRow")
    {
        let Some(children) = row["children"].as_array() else {
            continue;
        };
        let mut parts = Vec::new();
        walk(children, &mut parts);
        if !parts
            .iter()
            .any(|node| node["identifier"] == "copy-button" && has_state(node, "enabled"))
        {
            continue;
        }
        let text = parts
            .iter()
            .filter(|node| {
                matches!(
                    node["role"].as_str(),
                    Some("AXTextArea" | "AXStaticText" | "AXHeading" | "AXLink")
                ) || (node["role"].as_str() == Some("AXButton")
                    && node["identifier"]
                        .as_str()
                        .is_some_and(|id| id.starts_with("code-widget-")))
            })
            .filter_map(|node| {
                node["value"]
                    .as_str()
                    .or_else(|| node["description"].as_str())
            })
            .map(str::trim_end)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

struct Options<'a> {
    prompt: &'a str,
    model: Option<&'a str>,
    effort: Option<&'a str>,
    new: bool,
}

impl<'a> Options<'a> {
    fn parse(raw: &'a str) -> CoreResult<Self> {
        let mut options = Self {
            prompt: raw.trim(),
            model: None,
            effort: None,
            new: false,
        };
        while options.prompt.starts_with("--") {
            let (flag, rest) = options
                .prompt
                .split_once(char::is_whitespace)
                .unwrap_or((options.prompt, ""));
            options.prompt = rest.trim_start();
            match flag {
                "--" => break,
                "--new" if !options.new => options.new = true,
                "--model" | "--effort" => {
                    let (value, rest) = options
                        .prompt
                        .split_once(char::is_whitespace)
                        .unwrap_or((options.prompt, ""));
                    options.prompt = rest.trim_start();
                    match flag {
                        "--model"
                            if options.model.is_none()
                                && matches!(
                                    value,
                                    "auto" | "hydrogen" | "neon" | "carbon" | "argon"
                                ) =>
                        {
                            options.model = Some(value)
                        }
                        "--effort"
                            if options.effort.is_none()
                                && matches!(value, "low" | "standard" | "high" | "app") =>
                        {
                            options.effort = Some(value)
                        }
                        _ => {
                            return Err(CoreError::InvalidArgument(format!(
                                "{flag} の指定が無効です"
                            )));
                        }
                    }
                }
                _ => {
                    return Err(CoreError::InvalidArgument(format!(
                        "未対応のオプション: {flag}"
                    )));
                }
            }
        }
        if options.prompt.is_empty()
            || options.prompt.len() > 16 * 1024
            || options.prompt.contains('\0')
        {
            return Err(CoreError::InvalidArgument(
                "Gemini の質問は 1〜16384 バイトで入力してください".into(),
            ));
        }
        Ok(options)
    }
}

fn by_label<'a>(nodes: &[&'a Value], test: impl Fn(&str) -> bool) -> Option<&'a Value> {
    nodes.iter().copied().find(|node| {
        node["role"] == "AXButton"
            && has_state(node, "enabled")
            && node["description"].as_str().is_some_and(&test)
    })
}

fn model_label(node: &Value, model: &str) -> bool {
    node["description"].as_str().is_some_and(|label| {
        label.to_ascii_lowercase().contains(model) || model == "auto" && label.contains("自動")
    })
}

fn model_option<'a>(nodes: &[&'a Value], model: &str) -> Option<&'a Value> {
    nodes.iter().copied().find(|node| {
        node["role"] == "AXButton"
            && node["identifier"].is_null()
            && has_state(node, "enabled")
            && model_label(node, model)
    })
}

fn effort_entry<'a>(nodes: &[&'a Value]) -> Option<&'a Value> {
    by_label(nodes, |label| {
        matches!(label, "思考レベル" | "Thinking level" | "Thinking Level")
    })
}

fn effort_option<'a>(nodes: &[&'a Value], effort: &str) -> Option<&'a Value> {
    let local = match effort {
        "low" => "低",
        "standard" => "標準",
        "high" => "高",
        _ => return None,
    };
    by_label(nodes, |label| {
        let prefix = label.split(['、', ',']).next().unwrap_or_default().trim();
        prefix == local
            || prefix.eq_ignore_ascii_case(effort)
            || effort == "standard" && prefix.eq_ignore_ascii_case("medium")
    })
}

fn selected_conversation<'a>(nodes: &[&'a Value]) -> Option<&'a str> {
    nodes
        .iter()
        .find(|node| {
            has_state(node, "selected")
                && node["identifier"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("sidebar.conversation."))
        })
        .and_then(|node| node["identifier"].as_str())
}

fn require_empty(controls: &[&Value]) -> CoreResult<()> {
    if button(controls, "stop_button").is_some() {
        return Err(failure("別の回答を生成中です"));
    }
    let field = composer(controls).ok_or_else(|| failure("入力欄が見つかりません"))?;
    if !empty_field(field) {
        return Err(failure("入力欄に下書きが残っています"));
    }
    if temporary_chat(field) {
        return Err(failure("Gemini の一時チャットを閉じてください"));
    }
    Ok(())
}

fn prompt_count(controls: &[&Value], prompt: &str) -> usize {
    controls
        .iter()
        .filter(|node| node["role"] == "AXHeading" && matches_prompt(&node["description"], prompt))
        .count()
}

async fn send(
    desktop: &mut Desktop<'_>,
    prompt: &str,
    resume: Option<&str>,
    model: &str,
    effort: &str,
) -> CoreResult<(String, usize)> {
    let initial = desktop.chat_window().await?;
    require_empty(&nodes(&initial))?;
    if let Some(conversation) = resume {
        let sidebar = desktop.sidebar(true).await?;
        let rows = nodes(&sidebar);
        if selected_conversation(&rows) != Some(conversation) {
            let target = button(&rows, conversation).ok_or_else(|| failure("このスレッドの Gemini 会話を履歴で特定できません。Gemini でその会話を開くか /gemini --new を使ってください"))?;
            let _ = desktop.press(index(target)?).await;
            desktop
                .wait_for(|rows| selected_conversation(rows) == Some(conversation))
                .await?;
        }
    } else {
        let current = desktop.sidebar(false).await?;
        let controls = nodes(&current);
        if button(&controls, "tempChat_button").is_none() {
            let new = button(&controls, "newChat_button")
                .ok_or_else(|| failure("新規チャットのボタンが見つかりません"))?;
            desktop.press(index(new)?).await?;
            desktop
                .wait_for(|controls| button(controls, "tempChat_button").is_some())
                .await?;
        }
    }
    desktop.choose_model(model, effort).await?;
    let ready = desktop.snapshot().await?;
    let ready_nodes = nodes(&ready);
    require_empty(&ready_nodes)?;
    let prior_matches = prompt_count(&ready_nodes, prompt);
    let field = composer(&ready_nodes).ok_or_else(|| failure("Gemini の入力欄が見つかりません"))?;
    desktop
        .action(
            "setValue",
            json!({"elementIndex":index(field)?,"value":prompt}),
        )
        .await?;
    let drafted = desktop
        .wait_for(|controls| {
            composer(controls).is_some_and(|field| field["value"].as_str() == Some(prompt))
                && button(controls, "send_button").is_some()
        })
        .await?;
    let drafted_nodes = nodes(&drafted);
    let submit = button(&drafted_nodes, "send_button")
        .ok_or_else(|| failure("送信ボタンが見つかりません"))?;
    desktop.press(index(submit)?).await?;

    if let Some(conversation) = resume {
        return Ok((conversation.to_owned(), prior_matches));
    }
    desktop.sidebar(true).await?;
    let linked = desktop
        .wait_for(|rows| selected_conversation(rows).is_some())
        .await?;
    let conversation = selected_conversation(&nodes(&linked))
        .ok_or_else(|| failure("送信した Gemini 会話を特定できません"))?
        .to_owned();
    Ok((conversation, prior_matches))
}

async fn wait_for_answer(
    desktop: &Desktop<'_>,
    prompt: &str,
    prior_matches: usize,
) -> CoreResult<String> {
    let deadline = Instant::now() + Duration::from_secs(240);
    while Instant::now() < deadline {
        if let Ok(snapshot) = desktop.snapshot().await {
            let controls = nodes(&snapshot);
            if prompt_count(&controls, prompt) > prior_matches
                && let Some(response) = answer(&controls, prompt)
            {
                return Ok(response);
            }
        }
        sleep(Duration::from_secs(1)).await;
    }
    Err(failure(
        "送信後に回答を取得できませんでした。再送前に Gemini のチャットを確認してください",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse_accepts_valid_flags_and_double_dash() {
        let opts = Options::parse("--model hydrogen --effort high hello world").unwrap();
        assert_eq!(opts.model, Some("hydrogen"));
        assert_eq!(opts.effort, Some("high"));
        assert!(!opts.new);
        assert_eq!(opts.prompt, "hello world");

        let opts_new = Options::parse("--new --model auto hi").unwrap();
        assert!(opts_new.new);
        assert_eq!(opts_new.model, Some("auto"));
        assert_eq!(opts_new.prompt, "hi");

        let opts_dash = Options::parse("-- --flag-like prompt").unwrap();
        assert_eq!(opts_dash.prompt, "--flag-like prompt");
    }

    #[test]
    fn options_parse_rejects_invalid_inputs() {
        assert!(Options::parse("--model unknown hello").is_err());
        assert!(Options::parse("--effort ultra hello").is_err());
        assert!(Options::parse("   \n\t  ").is_err());
        assert!(Options::parse("--model hydrogen").is_err());
        assert!(Options::parse(&"a".repeat(16 * 1024)).is_ok());
        assert!(Options::parse(&"a".repeat(16 * 1024 + 1)).is_err());
        assert!(Options::parse("hello\0world").is_err());
    }

    #[test]
    fn model_label_and_effort_option_match_japanese_and_english() {
        let auto_ja = json!({"description": "自動"});
        let auto_en = json!({"description": "Auto"});
        assert!(model_label(&auto_ja, "auto"));
        assert!(model_label(&auto_en, "auto"));

        let controls = [
            json!({"role": "AXButton", "description": "低、高速", "states": ["enabled"]}),
            json!({"role": "AXButton", "description": "標準", "states": ["enabled"]}),
            json!({"role": "AXButton", "description": "高、深い推論", "states": ["enabled"]}),
        ];
        let refs: Vec<&Value> = controls.iter().collect();
        assert!(effort_option(&refs, "low").is_some());
        assert!(effort_option(&refs, "standard").is_some());
        assert!(effort_option(&refs, "high").is_some());
    }

    #[test]
    fn answer_extracts_static_text_textarea_and_code_widget() {
        let prompt = "Write Rust add";
        let tree = json!([
            {"role": "AXHeading", "description": prompt},
            {
                "role": "AXRow",
                "children": [
                    {"role": "AXButton", "identifier": "code-widget-copy-1", "description": "pub fn add(a: i32, b: i32) -> i32 { a + b }"},
                    {"role": "AXTextArea", "value": "Explanation here"},
                    {"role": "AXButton", "identifier": "copy-button", "states": ["enabled"]}
                ]
            }
        ]);
        let mut flat = Vec::new();
        walk(tree.as_array().unwrap(), &mut flat);
        let extracted = answer(&flat, prompt).expect("answer should be extracted");
        assert!(extracted.contains("pub fn add(a: i32, b: i32)"));
        assert!(extracted.contains("Explanation here"));
    }
}
