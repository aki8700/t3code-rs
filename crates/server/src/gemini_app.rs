//! Direct, accessibility-only access to Gemini for Mac chats.
use crate::{ServerState, shell};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use t3_core::{
    CoreError, CoreResult, ModelSelection, ProviderDriver, ProviderHealth, ProviderId,
    ServerConfig, Store, StreamEvent, TurnRequest,
};
use t3_store::SqliteStore;
use tokio::{
    process::Command,
    sync::Mutex,
    time::{Instant, sleep, timeout},
};
use uuid::Uuid;

const APP: &str = "com.google.GeminiMacOS";
static SESSION: Mutex<()> = Mutex::const_new(());
/// The last prompt this process typed, so a draft it left behind can be cleared.
static LAST_TYPED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

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
    let started_at = Utc::now();
    let started = Instant::now();
    let (answer, selection) = ask_app(&state.config, &state.store, id, options).await?;
    let model = &selection.model;
    let effort = selection.effort.as_deref().unwrap_or("app");
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
        Some(&selection),
    )
    .await?;
    Ok(result)
}

pub(crate) struct Driver {
    config: ServerConfig,
    store: SqliteStore,
}

impl Driver {
    pub(crate) fn new(config: &ServerConfig, store: SqliteStore) -> Self {
        Self {
            config: config.clone(),
            store,
        }
    }
}

#[async_trait::async_trait]
impl ProviderDriver for Driver {
    fn provider_id(&self) -> ProviderId {
        ProviderId::GeminiApp
    }
    async fn health_check(&self) -> ProviderHealth {
        let bin = self.config.resolved_cua_bin();
        ProviderHealth {
            provider: self.provider_id(),
            available: bin.as_ref().is_some_and(|p| p.is_file()),
            binary_path: bin,
            version: None,
            models: t3_providers::ProviderRegistry::default_models(self.provider_id()),
            error: None,
        }
    }
    async fn run_turn(
        &self,
        req: TurnRequest,
        tx: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    ) -> CoreResult<()> {
        if !req.attachments.is_empty() {
            return Err(CoreError::InvalidArgument(
                "Gemini app accepts text only".into(),
            ));
        }
        validate_prompt(&req.prompt)?;
        let options = Options {
            prompt: &req.prompt,
            model: Some(&req.model_selection.model),
            effort: req.model_selection.effort.as_deref(),
            new: false,
        };
        let (answer, _) = ask_app(&self.config, &self.store, req.thread_id, options).await?;
        let _ = tx.send(StreamEvent::Completed {
            final_text: Some(answer),
        });
        Ok(())
    }
    // The coordinator drops run_turn (including its session lock and CUA child)
    // on interruption. Never press Stop on a different conversation.
    async fn interrupt(&self, _: Uuid) -> CoreResult<()> {
        Ok(())
    }
}

async fn ask_app(
    cfg: &ServerConfig,
    store: &SqliteStore,
    id: Uuid,
    options: Options<'_>,
) -> CoreResult<(String, ModelSelection)> {
    if !cfg.computer_use_enabled {
        return Err(failure("T3 の computer_use_enabled を有効にしてください"));
    }
    let bin = cfg
        .resolved_cua_bin()
        .ok_or_else(|| failure("SwiftCUA をインストールしてください"))?;
    let _guard = SESSION.lock().await;
    let bin = bin.as_path();
    let prompt = options.prompt;
    let mut desktop = Desktop { bin, window: None };
    let saved = store.gemini_app_session(id).await?;
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
    store
        .bind_gemini_app_session(id, conversation, model.into(), effort.into())
        .await?;
    if resume.is_none() {
        desktop.sidebar(false).await?;
    }
    let answer = timeout(
        Duration::from_secs(15 * 60),
        wait_for_answer(&desktop, prompt, prior_matches),
    )
    .await
    .map_err(|_| failure("回答が15分以内に完了しませんでした"))??;
    // The accessibility text drops long code blocks (a diff came back as its
    // prose only), so prefer the app's own Markdown copy of the finished reply.
    let answer = copy_answer(&desktop).await.unwrap_or(answer);
    Ok((
        answer,
        ModelSelection {
            provider: ProviderId::GeminiApp,
            model: model.into(),
            effort: Some(effort.into()),
            agent: None,
        },
    ))
}

/// Presses the newest 回答をコピー button and reads the Markdown from the clipboard,
/// restoring the user's previous plain-text clipboard afterwards.
async fn copy_answer(desktop: &Desktop<'_>) -> Option<String> {
    let snapshot = desktop.snapshot_query(None).await.ok()?;
    let controls = nodes(&snapshot);
    let copy = controls
        .iter()
        .rev()
        .copied()
        .find(|n| n["role"] == "AXButton" && n["identifier"] == "copy-button")?;
    let saved = clipboard().await;
    let _ = set_clipboard("").await;
    desktop.press_node(copy).await.ok()?;
    let mut copied = None;
    for _ in 0..10 {
        sleep(Duration::from_millis(200)).await;
        if let Some(text) = clipboard().await.filter(|t| !t.trim().is_empty()) {
            copied = Some(text);
            break;
        }
    }
    let _ = set_clipboard(saved.as_deref().unwrap_or("")).await;
    copied
}

async fn clipboard() -> Option<String> {
    let out = Command::new("pbpaste").output().await.ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn set_clipboard(text: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes()).await?;
    }
    child.wait().await.map(drop)
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
        let args = params.to_string();
        let mut delays = [2, 5, 10].into_iter();
        let output = loop {
            let output = timeout(
                Duration::from_secs(8),
                Command::new(self.bin)
                    .kill_on_drop(true)
                    .args(["call", method, &args])
                    .output(),
            )
            .await
            .map_err(|_| failure("SwiftCUA が応答しません"))?
            .map_err(|_| failure("SwiftCUA を実行できません"))?;
            if output.status.success() {
                break output;
            }
            let error = String::from_utf8_lossy(&output.stderr);
            let code = error
                .trim_start()
                .strip_prefix('[')
                .and_then(|s| s.split_once(']'))
                .and_then(|(n, _)| n.parse::<i32>().ok());
            // 1007: the app is busy (AX -25204). 1006: SwiftUI briefly retracts the
            // window from the accessibility tree while it changes views.
            // A click that reports 1007 often took effect anyway; retrying a toggle
            // flips it back, so only snapshots are retried here.
            if matches!(code, Some(1006 | 1007))
                && method == "getAppState"
                && let Some(delay) = delays.next()
            {
                sleep(Duration::from_secs(delay)).await;
                continue;
            }
            let hint = code
                .map(|code| format!(" (エラー {code})"))
                .unwrap_or_default();
            return Err(failure(&format!(
                "SwiftCUA の {method} が失敗しました{hint}"
            )));
        };
        serde_json::from_slice(&output.stdout)
            .map_err(|_| failure("SwiftCUA の応答を読み取れません"))
    }

    async fn snapshot(&self) -> CoreResult<Value> {
        self.snapshot_query(Some("AXButton")).await
    }

    async fn snapshot_query(&self, _query: Option<&str>) -> CoreResult<Value> {
        // The real SwiftCUA `query` filter does not match role names such as
        // "AXTextArea" (it returned 0 nodes while the composer was on screen), so
        // the composer was never found. Always read the full tree.
        let state = self
            .call(
                "getAppState",
                json!({"includeScreenshot":false, "disableDiff":true, "limit":6000}),
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
                self.press_node(sidebar).await?;
                return self.wait_for(chat_controls).await;
            }
        }
        Err(failure("Gemini のチャット画面を開いてください"))
    }

    async fn wait_for(&self, ready: impl Fn(&[&Value]) -> bool) -> CoreResult<Value> {
        self.wait_for_query("AXButton", ready).await
    }

    async fn wait_for_query(
        &self,
        query: &str,
        ready: impl Fn(&[&Value]) -> bool,
    ) -> CoreResult<Value> {
        let mut last = String::new();
        for _ in 0..4 {
            let state = self.snapshot_query(Some(query)).await?;
            let controls = nodes(&state);
            if ready(&controls) {
                return Ok(state);
            }
            last = describe(&state, &controls);
            sleep(Duration::from_millis(250)).await;
        }
        Err(failure(&format!(
            "Gemini の入力欄または操作ボタンを確認できません（待機: {query}、最後の画面: {last}）"
        )))
    }

    async fn sidebar(&self, open: bool) -> CoreResult<Value> {
        let state = self.snapshot().await?;
        let controls = nodes(&state);
        if button(&controls, "sidebar_header_search").is_some() == open {
            return Ok(state);
        }
        let toggle = button(&controls, "sidebar_toggle")
            .ok_or_else(|| failure("サイドバーを操作できません"))?;
        self.press_node(toggle).await?;
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
            let _ = self.press_node(selector).await;
        }
        let menu = self
            .wait_for(|controls| controls.iter().any(|node| node["role"] == "AXPopover"))
            .await?;
        let menu_controls = nodes(&menu);
        let choice = model_option(&menu_controls, model)
            .ok_or_else(|| failure("指定モデルはこの Gemini アカウントで選べません"))?;
        if !has_state(choice, "selected") {
            self.press_node(choice).await?;
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
            let _ = self.press_node(selector).await;
        } else if effort == "app" {
            return self.press_node(choice).await;
        }
        let effort_menu = self
            .wait_for(|controls| effort_entry(controls).is_some())
            .await?;
        let effort_controls = nodes(&effort_menu);
        let entry = effort_entry(&effort_controls)
            .ok_or_else(|| failure("このモデルでは思考レベルを変更できません"))?;
        let _ = self.press_node(entry).await;
        let level_menu = self
            .wait_for(|controls| {
                effort_option(controls, "low").is_some()
                    || effort_option(controls, "high").is_some()
            })
            .await?;
        let level_controls = nodes(&level_menu);
        let level = effort_option(&level_controls, effort)
            .ok_or_else(|| failure("指定した思考レベルは選べません"))?;
        self.press_node(level).await?;
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

    /// Presses `node`, re-resolving it from a fresh snapshot when SwiftCUA reports
    /// a stale element index (`[1005]`, AX error -25202) because the UI changed.
    async fn press_node(&self, node: &Value) -> CoreResult<()> {
        let mut target = index(node)?;
        for _ in 0..2 {
            match self.press(target).await {
                // AX -25204: the press often lands while the app is busy animating.
                // Callers confirm the outcome by waiting for the next state.
                Err(error) if error.to_string().contains("(エラー 1007)") => return Ok(()),
                Err(error) if error.to_string().contains("(エラー 1005)") => {
                    sleep(Duration::from_millis(500)).await;
                    let fresh = self.snapshot_query(None).await?;
                    let nodes = nodes(&fresh);
                    let Some(found) = same_element(&nodes, node) else {
                        return Err(error);
                    };
                    target = index(found)?;
                }
                result => return result,
            }
        }
        self.press(target).await
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

fn composer(state: &Value) -> Option<&Value> {
    fn visit(nodes: &[Value]) -> Option<&Value> {
        nodes.iter().rev().find_map(|node| {
            // Answer rows can themselves contain code editors.
            if node["role"] == "AXRow" {
                return None;
            }
            if node["role"] == "AXTextArea" {
                return Some(node);
            }
            visit(node["children"].as_array()?)
        })
    }
    visit(state.pointer("/state/accessibility/nodes")?.as_array()?)
}

fn button<'a>(nodes: &[&'a Value], id: &str) -> Option<&'a Value> {
    nodes.iter().copied().find(|node| {
        node["role"] == "AXButton" && node["identifier"] == id && has_state(node, "enabled")
    })
}

fn chat_controls(nodes: &[&Value]) -> bool {
    button(nodes, "tab_stop_model_selector").is_some()
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
            let body = options.prompt;
            let (flag, rest) = options
                .prompt
                .split_once(char::is_whitespace)
                .unwrap_or((options.prompt, ""));
            options.prompt = rest.trim_start();
            match flag {
                "--" if rest.starts_with("--") => break,
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
                "--new" => return Err(CoreError::InvalidArgument("duplicate --new".into())),
                _ => {
                    options.prompt = body;
                    break;
                }
            }
        }
        validate_prompt(options.prompt)?;
        Ok(options)
    }
}

pub(crate) fn validate_prompt(prompt: &str) -> CoreResult<()> {
    if prompt.trim().is_empty() || prompt.len() > 16 * 1024 || prompt.contains('\0') {
        return Err(CoreError::InvalidArgument(
            "Gemini の質問は 1〜16384 バイトで入力してください".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_selection(selection: &ModelSelection) -> CoreResult<()> {
    if !matches!(
        selection.model.as_str(),
        "auto" | "hydrogen" | "neon" | "carbon" | "argon"
    ) || !matches!(
        selection.effort.as_deref(),
        None | Some("low" | "standard" | "high" | "app")
    ) {
        return Err(CoreError::InvalidArgument(
            "invalid Gemini app model or effort".into(),
        ));
    }
    Ok(())
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

async fn empty_composer(desktop: &Desktop<'_>) -> CoreResult<u64> {
    if button(
        &nodes(&desktop.snapshot_query(Some("stop_button")).await?),
        "stop_button",
    )
    .is_some()
    {
        return Err(failure("別の回答を生成中です"));
    }
    let state = desktop.snapshot_query(Some("AXTextArea")).await?;
    let field = composer(&state).ok_or_else(|| {
        failure(&format!(
            "入力欄が見つかりません（最後の画面: {}）",
            describe(&state, &nodes(&state))
        ))
    })?;
    if !empty_field(field) {
        let own = field["value"].as_str().is_some_and(|draft| {
            LAST_TYPED
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_deref()
                == Some(draft)
        });
        if !own {
            return Err(failure("入力欄に下書きが残っています"));
        }
        // A previous bridge call typed this and failed before sending it.
        desktop
            .action("setValue", json!({"elementIndex":index(field)?,"value":""}))
            .await?;
    }
    if temporary_chat(field) {
        return Err(failure("Gemini の一時チャットを閉じてください"));
    }
    index(field)
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
    desktop.chat_window().await?;
    // An open history sidebar makes SwiftCUA's snapshot hit its deadline before the
    // chat pane (122 nodes, truncated, no composer; closed: 47 nodes in 0.2 s).
    desktop.sidebar(false).await?;
    empty_composer(desktop).await?;
    if let Some(conversation) = resume {
        let sidebar = desktop.sidebar(true).await?;
        let rows = nodes(&sidebar);
        if selected_conversation(&rows) != Some(conversation) {
            let target = button(&rows, conversation).ok_or_else(|| failure("このスレッドの Gemini 会話を履歴で特定できません。Gemini でその会話を開くか /gemini --new を使ってください"))?;
            let _ = desktop.press_node(target).await;
            desktop
                .wait_for(|rows| selected_conversation(rows) == Some(conversation))
                .await?;
        }
        desktop.sidebar(false).await?;
    } else {
        let current = desktop.sidebar(false).await?;
        let controls = nodes(&current);
        if button(&controls, "tempChat_button").is_none() {
            let new = button(&controls, "newChat_button")
                .ok_or_else(|| failure("新規チャットのボタンが見つかりません"))?;
            desktop.press_node(new).await?;
            desktop
                .wait_for(|controls| button(controls, "tempChat_button").is_some())
                .await?;
        }
    }
    desktop.choose_model(model, effort).await?;
    let headings = desktop.snapshot_query(Some("AXHeading")).await?;
    let prior_matches = prompt_count(&nodes(&headings), prompt);
    let field = empty_composer(desktop).await?;
    desktop
        .action("setValue", json!({"elementIndex":field,"value":prompt}))
        .await?;
    *LAST_TYPED.lock().unwrap_or_else(|e| e.into_inner()) = Some(prompt.to_owned());
    let mut drafted = false;
    for _ in 0..4 {
        let state = desktop.snapshot_query(Some("AXTextArea")).await?;
        if composer(&state).is_some_and(|field| field["value"].as_str() == Some(prompt)) {
            drafted = true;
            break;
        }
        sleep(Duration::from_millis(250)).await;
    }
    if !drafted {
        return Err(failure("入力した本文を確認できません"));
    }
    let drafted = desktop
        .wait_for_query("send_button", |controls| {
            button(controls, "send_button").is_some()
        })
        .await?;
    let drafted_nodes = nodes(&drafted);
    let submit = button(&drafted_nodes, "send_button")
        .ok_or_else(|| failure("送信ボタンが見つかりません"))?;
    desktop.press_node(submit).await?;

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
    let mut stable = StableAnswer::default();
    let mut retries = 0;
    loop {
        let snapshot = desktop.snapshot_query(None).await?;
        let controls = nodes(&snapshot);
        if prompt_count(&controls, prompt) > prior_matches {
            let question = controls
                .iter()
                .rposition(|node| {
                    node["role"] == "AXHeading" && matches_prompt(&node["description"], prompt)
                })
                .unwrap();
            let reply = &controls[question + 1..];
            if let Some(retry) = retry_card(reply) {
                stable = StableAnswer::default();
                if retries == 5 {
                    return Err(failure("Gemini で回答の生成に5回再試行しても失敗しました"));
                }
                let retry =
                    retry.ok_or_else(|| failure("Gemini のエラーに再試行ボタンがありません"))?;
                desktop.press_node(retry).await?;
                retries += 1;
                sleep(Duration::from_secs(2)).await;
                continue;
            }
            let response = answer(&controls, prompt);
            let stop = desktop.snapshot_query(Some("stop_button")).await?;
            if let Some(done) = stable.observe(
                response,
                button(&nodes(&stop), "stop_button").is_some(),
                Instant::now(),
            ) {
                return Ok(done);
            }
        } else {
            stable = StableAnswer::default();
        }
        sleep(Duration::from_secs(1)).await;
    }
}

// Only inspect the current question's response. Earlier error cards can remain
// in the conversation after a later turn succeeds.
/// One-line summary of a snapshot for failure messages.
fn describe(state: &Value, controls: &[&Value]) -> String {
    let has = |id: &str| controls.iter().any(|n| n["identifier"] == id);
    format!(
        "要素{} 打ち切り{} サイドバー{} 入力欄{} 送信{} 停止{}",
        controls.len(),
        state
            .pointer("/state/accessibility/truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        has("sidebar_header_search"),
        controls.iter().any(|n| n["role"] == "AXTextArea"),
        has("send_button"),
        has("stop_button"),
    )
}

/// Finds the element matching `node` (role, identifier, description) in a fresh snapshot.
fn same_element<'a>(nodes: &[&'a Value], node: &Value) -> Option<&'a Value> {
    nodes.iter().copied().find(|candidate| {
        candidate["role"] == node["role"]
            && candidate["identifier"] == node["identifier"]
            && candidate["description"] == node["description"]
    })
}

fn retry_card<'a>(reply: &[&'a Value]) -> Option<Option<&'a Value>> {
    let retry = reply.iter().rev().copied().find(|n| {
        n["role"] == "AXButton" && matches!(n["description"].as_str(), Some("再試行" | "Retry"))
    });
    let error = reply.iter().any(|node| {
        node["role"] == "AXStaticText"
            && [node["value"].as_str(), node["description"].as_str()]
                .into_iter()
                .flatten()
                .any(|text| {
                    let text = text.trim().to_lowercase();
                    [
                        "エラーが発生しました",
                        "コンテンツを生成できませんでした",
                        "something went wrong",
                        "an error occurred",
                        "error",
                    ]
                    .iter()
                    .filter_map(|label| text.strip_prefix(label))
                    .any(|suffix| {
                        let code = suffix.trim_matches(|c: char| {
                            c.is_whitespace() || "（()）。.:：".contains(c)
                        });
                        code == "1213" || code.is_empty() || retry.is_some()
                    })
                })
    });
    error.then_some(retry)
}

#[derive(Default)]
struct StableAnswer(Option<(String, Instant)>);

impl StableAnswer {
    fn observe(
        &mut self,
        response: Option<String>,
        generating: bool,
        now: Instant,
    ) -> Option<String> {
        let Some(text) = response.filter(|_| !generating) else {
            self.0 = None;
            return None;
        };
        if let Some((previous, since)) = &self.0
            && previous == &text
        {
            return (now.duration_since(*since) >= Duration::from_secs(20)).then_some(text);
        }
        self.0 = Some((text, now));
        None
    }
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

        let divider = Options::parse("--model neon ---\nreview").unwrap();
        assert_eq!(divider.prompt, "---\nreview");
        assert_eq!(
            Options::parse("--literal text").unwrap().prompt,
            "--literal text"
        );

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
    fn same_element_finds_a_moved_button_by_role_identifier_and_description() {
        let old = json!({"index": 37, "role": "AXButton", "identifier": "sidebar_toggle", "description": "サイドバーを切り替え"});
        let moved = json!({"index": 52, "role": "AXButton", "identifier": "sidebar_toggle", "description": "サイドバーを切り替え"});
        let other = json!({"index": 37, "role": "AXButton", "identifier": "newChat_button", "description": "新しいチャット"});
        assert_eq!(same_element(&[&other, &moved], &old), Some(&moved));
        assert_eq!(same_element(&[&other], &old), None);
    }

    #[test]
    fn retry_card_distinguishes_an_error_label_from_an_answer_about_errors() {
        let discussion =
            json!({"role":"AXStaticText","value":"The error 1213 comes from the app."});
        let card = json!({"role":"AXStaticText","value":"エラーが発生しました（1213）"});
        let button = json!({"role":"AXButton","description":"再試行"});
        assert!(retry_card(&[&discussion]).is_none());
        assert_eq!(retry_card(&[&card]), Some(None));
        assert_eq!(retry_card(&[&card, &button]), Some(Some(&button)));
    }

    #[test]
    fn changing_text_or_a_stop_button_resets_the_stability_window() {
        let mut stable = StableAnswer::default();
        let start = Instant::now();
        for (second, text, generating) in [
            (0, "a", false),
            (19, "a", false),
            (20, "ab", false),
            (38, "ab", true),
            (40, "ab", false),
            (59, "ab", false),
        ] {
            assert_eq!(
                stable.observe(
                    Some(text.into()),
                    generating,
                    start + Duration::from_secs(second)
                ),
                None
            );
        }
        assert_eq!(
            stable.observe(Some("ab".into()), false, start + Duration::from_secs(60)),
            Some("ab".into())
        );
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
