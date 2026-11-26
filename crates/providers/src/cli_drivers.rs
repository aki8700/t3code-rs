use crate::{
    catalog,
    process::{self, ActiveRunMap, Input},
};
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;
use t3_core::{
    protocol::StreamEvent,
    traits::{CoreError, CoreResult, ProviderDriver, TurnRequest},
    types::{
        Attachment, InteractionMode, McpInjectionSpec, PendingResponse, ProviderHealth, ProviderId,
        RuntimeMode,
    },
};
use tokio::{process::Command, sync::mpsc};
use uuid::Uuid;

#[derive(Clone)]
struct Cli {
    bin: PathBuf,
    active: ActiveRunMap,
    data_dir: Option<PathBuf>,
}

impl Cli {
    fn new(bin: PathBuf) -> Self {
        Self {
            bin,
            active: ActiveRunMap::default(),
            data_dir: None,
        }
    }

    fn command(&self) -> Command {
        let mut cmd = process::command(&self.bin);
        if let Some(dir) = &self.data_dir {
            cmd.env("ANTIGRAVITY_APP_DATA_DIR", dir);
        }
        cmd
    }

    async fn health(&self, provider: ProviderId) -> ProviderHealth {
        let version = process::version(self.command()).await;
        ProviderHealth {
            provider,
            available: version.is_ok(),
            binary_path: Some(self.bin.clone()),
            version: version
                .as_ref()
                .ok()
                .filter(|value| !value.is_empty())
                .cloned(),
            models: catalog::default_models(provider),
            error: version.err(),
        }
    }

    async fn run(
        &self,
        provider: ProviderId,
        mut req: TurnRequest,
        tx: mpsc::UnboundedSender<StreamEvent>,
        build: impl FnOnce(&mut Command, &TurnRequest) -> Input,
    ) -> CoreResult<()> {
        req.workspace_dir = std::path::absolute(&req.workspace_dir)
            .map_err(|err| CoreError::InvalidArgument(format!("CLI workspace: {err}")))?;
        let mut cmd = self.command();
        cmd.current_dir(&req.workspace_dir);
        let input = build(&mut cmd, &req);
        let secret = req
            .mcp_injection
            .as_ref()
            .map(|spec| spec.internal_token.as_str());
        process::run_turn(&self.active, req.run_id, provider, cmd, input, secret, tx).await
    }
}

macro_rules! cli_driver {
    ($name:ident, $provider:ident, $build:ident, $supports_mcp:expr) => {
        #[doc = concat!("Local CLI driver for ", stringify!($provider), ".")]
        #[derive(Clone)]
        pub struct $name(Cli);

        impl $name {
            pub fn new(binary: impl Into<PathBuf>) -> Self {
                Self(Cli::new(binary.into()))
            }
        }

        #[async_trait]
        impl ProviderDriver for $name {
            fn provider_id(&self) -> ProviderId {
                ProviderId::$provider
            }

            fn supports_mcp(&self) -> bool {
                $supports_mcp
            }

            async fn health_check(&self) -> ProviderHealth {
                self.0.health(self.provider_id()).await
            }

            async fn run_turn(
                &self,
                req: TurnRequest,
                tx: mpsc::UnboundedSender<StreamEvent>,
            ) -> CoreResult<()> {
                self.0.run(self.provider_id(), req, tx, $build).await
            }

            async fn send_steer(&self, run_id: Uuid, message: String) -> CoreResult<bool> {
                Ok(process::send_steer(&self.0.active, run_id, message))
            }

            async fn respond_request(
                &self,
                run_id: Uuid,
                resp: PendingResponse,
            ) -> CoreResult<bool> {
                Ok(process::send_control_response(
                    &self.0.active,
                    run_id,
                    &resp,
                ))
            }

            async fn interrupt(&self, run_id: Uuid) -> CoreResult<()> {
                process::interrupt(&self.0.active, run_id).await
            }
        }
    };
}

cli_driver!(ClaudeDriver, Claude, claude_args, true);
cli_driver!(CodexDriver, Codex, codex_args, true);
cli_driver!(OpenCodeDriver, OpenCode, opencode_args, true);

/// Local CLI driver for Antigravity.
#[derive(Clone)]
pub struct AntigravityDriver {
    cli: Cli,
    mcp_enabled: bool,
}

impl AntigravityDriver {
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            cli: Cli::new(binary.into()),
            mcp_enabled: false,
        }
    }

    pub fn with_data_dir(binary: impl Into<PathBuf>, data_dir: impl Into<PathBuf>) -> Self {
        let mut driver = Self::new(binary);
        driver.cli.data_dir = Some(data_dir.into());
        driver
    }

    pub fn with_mcp(mut self, enabled: bool) -> Self {
        self.mcp_enabled = enabled;
        self
    }

    #[cfg(test)]
    fn build_command(&self, req: &TurnRequest) -> (Command, Input) {
        let mut cmd = self.cli.command();
        let input = antigravity_args(&mut cmd, req, self.mcp_enabled);
        (cmd, input)
    }
}

#[async_trait]
impl ProviderDriver for AntigravityDriver {
    fn provider_id(&self) -> ProviderId {
        ProviderId::Antigravity
    }

    fn supports_mcp(&self) -> bool {
        self.mcp_enabled
    }

    async fn health_check(&self) -> ProviderHealth {
        self.cli.health(self.provider_id()).await
    }

    async fn run_turn(
        &self,
        req: TurnRequest,
        tx: mpsc::UnboundedSender<StreamEvent>,
    ) -> CoreResult<()> {
        // Inject per-run MCP env vars whenever the coordinator supplies `req.mcp_injection`,
        // allowing `antigravity_mcp_enabled` to toggle live without restarting the server.
        self.cli
            .run(self.provider_id(), req, tx, |cmd, req| {
                antigravity_args(cmd, req, req.mcp_injection.is_some())
            })
            .await
    }

    async fn send_steer(&self, run_id: Uuid, message: String) -> CoreResult<bool> {
        Ok(process::send_steer(&self.cli.active, run_id, message))
    }

    async fn respond_request(&self, run_id: Uuid, resp: PendingResponse) -> CoreResult<bool> {
        Ok(process::send_control_response(
            &self.cli.active,
            run_id,
            &resp,
        ))
    }

    async fn interrupt(&self, run_id: Uuid) -> CoreResult<()> {
        process::interrupt(&self.cli.active, run_id).await
    }
}

fn stream_json_args(cmd: &mut Command) {
    cmd.args([
        "-p",
        "--output-format",
        "stream-json",
        "--input-format",
        "stream-json",
    ]);
}

fn model_effort_args(cmd: &mut Command, req: &TurnRequest) {
    cmd.args(["--model", &req.model_selection.model]);
    if let Some(effort) = &req.model_selection.effort {
        cmd.args(["--effort", effort]);
    }
}

fn claude_args(cmd: &mut Command, req: &TurnRequest) -> Input {
    stream_json_args(cmd);
    cmd.args(["--verbose", "--include-partial-messages"]);
    model_effort_args(cmd, req);
    if let Some(session) = &req.resume_session_id {
        cmd.args(["--resume", session]);
    }
    // Plan stays without the stdio prompt: approving ExitPlanMode would leave plan mode
    // inside a run whose thread is still capped at Plan.
    let relay = match (req.interaction_mode, req.runtime_mode) {
        (InteractionMode::Plan, _) => {
            cmd.args(["--permission-mode", "plan"]);
            None
        }
        (_, RuntimeMode::FullAccess) => {
            cmd.arg("--dangerously-skip-permissions");
            None
        }
        (_, RuntimeMode::AutoAcceptEdits) => Some("acceptEdits"),
        (_, RuntimeMode::ApprovalRequired) => Some("default"),
    };
    if let Some(mode) = relay {
        // Without a prompt tool, `-p` denies every gated call silently instead of asking.
        cmd.args([
            "--permission-mode",
            mode,
            "--permission-prompt-tool",
            "stdio",
        ]);
        if req.mcp_injection.is_some() {
            cmd.arg("--allowedTools")
                .args(preapproved_t3rs_tools(req.runtime_mode));
        }
    }
    if let Some(spec) = &req.mcp_injection {
        cmd.env("T3_MCP_TOKEN", &spec.internal_token);
        cmd.arg("--mcp-config").arg(
            json!({
                "mcpServers": {
                    "t3rs": {
                        "command": spec.binary_path.to_string_lossy(),
                        "args": mcp_args(spec),
                        "env": { "T3_MCP_TOKEN": "${T3_MCP_TOKEN}" },
                    }
                }
            })
            .to_string(),
        );
    }
    let content = match req.claude_slash_context.as_deref() {
        Some(context) if !context.is_empty() || !req.attachments.is_empty() => {
            // Claude resolves the slash invocation from the last text block. Earlier blocks
            // remain user context and stay outside the command's arguments.
            let context = prompt_with_paths(context, &req.attachments, false);
            json!([
                {"type": "text", "text": context.trim_start()},
                {"type": "text", "text": req.prompt}
            ])
        }
        _ => json!(prompt_with_paths(&req.prompt, &req.attachments, false)),
    };
    Input::StreamJson(content)
}

/// Internal tools that run without a prompt when approvals are relayed. The server caps
/// delegated children at the caller's modes, so these cannot raise a child above its parent.
/// `task_merge` writes into the parent workspace, so it is pre-approved only with edits.
fn preapproved_t3rs_tools(mode: RuntimeMode) -> Vec<String> {
    const TOOLS: [&str; 20] = [
        "orchestrator_capabilities",
        "swarm_status",
        "swarm_add_worker",
        "swarm_post_message",
        "swarm_read_board",
        "swarm_kill_worker",
        "delegate_task",
        "task_status",
        "task_cancel",
        "task_diff",
        "thread_list",
        "thread_read",
        "thread_send",
        "thread_fork",
        "orch_stats",
        "code_index",
        "code_slice",
        "code_grep",
        "code_diff",
        "compaction_econ",
    ];
    let merge = (mode == RuntimeMode::AutoAcceptEdits).then_some("task_merge");
    TOOLS
        .into_iter()
        .chain(merge)
        .map(|tool| format!("mcp__t3rs__{tool}"))
        .collect()
}

fn codex_args(cmd: &mut Command, req: &TurnRequest) -> Input {
    cmd.arg("exec");
    // The sandbox option belongs to exec, including when resuming a session.
    match (req.interaction_mode, req.runtime_mode) {
        (InteractionMode::Plan, _) => cmd.args(["-s", "read-only"]),
        (_, RuntimeMode::AutoAcceptEdits) => cmd.args(["-s", "workspace-write"]),
        _ => cmd,
    };
    if let Some(session) = &req.resume_session_id {
        cmd.args(["resume", session]);
    }
    cmd.args([
        "--json",
        "-m",
        &req.model_selection.model,
        "--skip-git-repo-check",
    ]);
    if let Some(effort) = &req.model_selection.effort {
        cmd.args(["-c", &format!("model_reasoning_effort={}", json!(effort))]);
    }
    for attachment in &req.attachments {
        if attachment.mime_type.starts_with("image/") {
            cmd.arg("-i").arg(&attachment.storage_path);
        }
    }
    if req.runtime_mode == RuntimeMode::FullAccess && req.interaction_mode != InteractionMode::Plan
    {
        cmd.arg("--dangerously-bypass-approvals-and-sandbox");
    }
    if let Some(spec) = &req.mcp_injection {
        cmd.env("T3_MCP_TOKEN", &spec.internal_token);
        cmd.args([
            "-c",
            &format!(
                "mcp_servers.t3rs.command={}",
                json!(spec.binary_path.to_string_lossy())
            ),
            "-c",
            &format!("mcp_servers.t3rs.args={}", json!(mcp_args(spec))),
            "-c",
            r#"mcp_servers.t3rs.env_vars=["T3_MCP_TOKEN"]"#,
        ]);
    }
    cmd.arg("-");
    Input::Text(prompt_with_paths(&req.prompt, &req.attachments, true))
}

fn antigravity_args(cmd: &mut Command, req: &TurnRequest, mcp_enabled: bool) -> Input {
    cmd.args(["--output-format", "stream-json"]);
    model_effort_args(cmd, req);
    match (req.interaction_mode, req.runtime_mode) {
        (InteractionMode::Plan, _) => cmd.args(["--mode", "plan"]),
        (_, RuntimeMode::AutoAcceptEdits) => cmd.args(["--mode", "accept-edits"]),
        (_, RuntimeMode::FullAccess) => {
            cmd.args(["--mode", "auto", "--dangerously-skip-permissions"])
        }
        (_, RuntimeMode::ApprovalRequired) => cmd,
    };
    if let Some(session) = &req.resume_session_id {
        cmd.args(["--conversation", session]);
    }
    if mcp_enabled && let Some(spec) = &req.mcp_injection {
        cmd.env("T3_SERVER_URL", &spec.server_url);
        cmd.env("T3_THREAD_ID", spec.thread_id.to_string());
        if !spec.internal_token.is_empty() {
            cmd.env("T3_MCP_TOKEN", &spec.internal_token);
        }
    }
    cmd.arg("--print")
        .arg(prompt_with_paths(&req.prompt, &req.attachments, false));
    Input::None
}

fn opencode_args(cmd: &mut Command, req: &TurnRequest) -> Input {
    cmd.args([
        "run",
        "--format",
        "json",
        "--thinking",
        "-m",
        &req.model_selection.model,
    ]);
    if let Some(effort) = &req.model_selection.effort {
        cmd.args(["--variant", effort]);
    }
    if let Some(agent) = match req.interaction_mode {
        InteractionMode::Plan => Some("plan"),
        InteractionMode::Default => req.model_selection.agent.as_deref(),
    } {
        cmd.args(["--agent", agent]);
    }
    cmd.arg("--dir").arg(&req.workspace_dir);
    if req.runtime_mode == RuntimeMode::FullAccess && req.interaction_mode != InteractionMode::Plan
    {
        cmd.arg("--auto");
    }
    if let Some(session) = &req.resume_session_id {
        cmd.args(["-s", session]);
    }
    for attachment in &req.attachments {
        cmd.arg("-f").arg(&attachment.storage_path);
    }
    if let Some(spec) = &req.mcp_injection {
        let mut cmd_vec = vec![spec.binary_path.to_string_lossy().into_owned()];
        cmd_vec.extend(mcp_args(spec));
        cmd.env("T3_MCP_TOKEN", &spec.internal_token);
        cmd.env(
            "OPENCODE_CONFIG_CONTENT",
            json!({
                "mcp": {
                    "t3rs": {
                        "type": "local",
                        "command": cmd_vec,
                        "environment": {
                            "T3_MCP_TOKEN": "{env:T3_MCP_TOKEN}",
                        },
                    }
                }
            })
            .to_string(),
        );
    }
    cmd.args(["--", &req.prompt]);
    Input::None
}

fn mcp_args(spec: &McpInjectionSpec) -> Vec<String> {
    vec![
        "mcp".into(),
        "--url".into(),
        spec.server_url.clone(),
        "--thread-id".into(),
        spec.thread_id.to_string(),
    ]
}

fn prompt_with_paths(prompt: &str, attachments: &[Attachment], omit_images: bool) -> String {
    let mut prompt = prompt.to_owned();
    for (index, attachment) in attachments
        .iter()
        .filter(|attachment| !omit_images || !attachment.mime_type.starts_with("image/"))
        .enumerate()
    {
        if index == 0 {
            prompt.push_str("\n\nAttachments (file paths):");
        }
        prompt.push_str("\n- ");
        prompt.push_str(&attachment.storage_path.to_string_lossy());
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_request;

    fn request(provider: ProviderId) -> TurnRequest {
        let mut req = test_request(provider);
        req.resume_session_id = Some("existing session".into());
        req.model_selection.effort = Some("high".into());
        req.workspace_dir = "/workspace with spaces".into();
        for (path, mime) in [
            ("/attachments/picture.png", "image/png"),
            ("/attachments/notes.txt", "text/plain"),
        ] {
            req.attachments.push(Attachment {
                id: Uuid::new_v4(),
                thread_id: Some(req.thread_id),
                filename: path.rsplit('/').next().unwrap().into(),
                mime_type: mime.into(),
                size_bytes: 0,
                blake3_hash: String::new(),
                storage_path: path.into(),
                created_at: chrono::Utc::now(),
            });
        }
        req.mcp_injection = Some(McpInjectionSpec {
            binary_path: "/test tools/t3d".into(),
            server_url: "http://127.0.0.1:3780".into(),
            internal_token: "fixture-token".into(),
            thread_id: Uuid::nil(),
        });
        req
    }

    fn args(cmd: &Command) -> Vec<String> {
        cmd.as_std()
            .get_args()
            .map(|arg| arg.to_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn claude_stream_args_resume_permissions_and_inline_mcp() {
        let mut req = request(ProviderId::Claude);
        req.runtime_mode = RuntimeMode::AutoAcceptEdits;
        let mut cmd = ClaudeDriver::new("claude").0.command();
        let Input::StreamJson(prompt) = claude_args(&mut cmd, &req) else {
            panic!("expected NDJSON input")
        };
        let args = args(&cmd);
        assert!(args.iter().all(|arg| !arg.contains("fixture-token")));
        assert_token_env(&cmd);
        assert_eq!(
            &args[..args.len() - 1],
            [
                "-p",
                "--output-format",
                "stream-json",
                "--input-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--model",
                "test-model",
                "--effort",
                "high",
                "--resume",
                "existing session",
                "--permission-mode",
                "acceptEdits",
                "--permission-prompt-tool",
                "stdio",
                "--allowedTools",
            ]
            .into_iter()
            .map(str::to_owned)
            .chain(preapproved_t3rs_tools(RuntimeMode::AutoAcceptEdits))
            .chain(["--mcp-config".to_owned()])
            .collect::<Vec<_>>()
        );
        let mcp: serde_json::Value = serde_json::from_str(args.last().unwrap()).unwrap();
        assert_eq!(
            mcp,
            json!({"mcpServers":{"t3rs":{"command":"/test tools/t3d",
                "args":["mcp","--url","http://127.0.0.1:3780","--thread-id","00000000-0000-0000-0000-000000000000"],
                "env":{"T3_MCP_TOKEN":"${T3_MCP_TOKEN}"}
            }}})
        );
        assert!(
            prompt
                .as_str()
                .unwrap()
                .contains("\n- /attachments/picture.png\n- /attachments/notes.txt")
        );
    }

    #[tokio::test]
    async fn claude_plan_takes_precedence_over_full_access() {
        let mut req = test_request(ProviderId::Claude);
        req.runtime_mode = RuntimeMode::FullAccess;
        req.interaction_mode = InteractionMode::Plan;
        let mut cmd = ClaudeDriver::new("claude").0.command();
        claude_args(&mut cmd, &req);
        assert_eq!(&args(&cmd)[9..], ["--permission-mode", "plan"]);
    }

    #[tokio::test]
    async fn claude_relays_approvals_only_for_gated_modes() {
        for (interaction, runtime, permissions, merge) in [
            (
                InteractionMode::Default,
                RuntimeMode::ApprovalRequired,
                &[
                    "--permission-mode",
                    "default",
                    "--permission-prompt-tool",
                    "stdio",
                ][..],
                Some(false),
            ),
            (
                InteractionMode::Default,
                RuntimeMode::AutoAcceptEdits,
                &[
                    "--permission-mode",
                    "acceptEdits",
                    "--permission-prompt-tool",
                    "stdio",
                ][..],
                Some(true),
            ),
            (
                InteractionMode::Default,
                RuntimeMode::FullAccess,
                &["--dangerously-skip-permissions"][..],
                None,
            ),
            (
                InteractionMode::Plan,
                RuntimeMode::ApprovalRequired,
                &["--permission-mode", "plan"][..],
                None,
            ),
            (
                InteractionMode::Plan,
                RuntimeMode::FullAccess,
                &["--permission-mode", "plan"][..],
                None,
            ),
        ] {
            for injected in [false, true] {
                let mut req = request(ProviderId::Claude);
                req.interaction_mode = interaction;
                req.runtime_mode = runtime;
                if !injected {
                    req.mcp_injection = None;
                }
                let mut cmd = ClaudeDriver::new("claude").0.command();
                claude_args(&mut cmd, &req);
                let args = args(&cmd);
                let start = args
                    .iter()
                    .position(|arg| arg == "existing session")
                    .unwrap()
                    + 1;
                let end = args
                    .iter()
                    .position(|arg| arg == "--allowedTools" || arg == "--mcp-config")
                    .unwrap_or(args.len());
                assert_eq!(
                    &args[start..end],
                    permissions,
                    "{interaction:?} {runtime:?}"
                );
                let allowed: Vec<_> = args
                    .iter()
                    .skip_while(|arg| *arg != "--allowedTools")
                    .skip(1)
                    .take_while(|arg| !arg.starts_with("--"))
                    .cloned()
                    .collect();
                match merge.filter(|_| injected) {
                    None => assert!(!args.contains(&"--allowedTools".to_owned())),
                    Some(merge) => {
                        assert!(allowed.iter().all(|tool| tool.starts_with("mcp__t3rs__")));
                        assert!(allowed.contains(&"mcp__t3rs__swarm_post_message".to_owned()));
                        assert!(allowed.contains(&"mcp__t3rs__delegate_task".to_owned()));
                        assert_eq!(
                            allowed.contains(&"mcp__t3rs__task_merge".to_owned()),
                            merge,
                            "{runtime:?}"
                        );
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn codex_resume_effort_images_and_mcp_overrides() {
        let mut req = request(ProviderId::Codex);
        req.runtime_mode = RuntimeMode::FullAccess;
        let mut cmd = CodexDriver::new("codex").0.command();
        let Input::Text(prompt) = codex_args(&mut cmd, &req) else {
            panic!("expected plain text input")
        };
        assert!(args(&cmd).iter().all(|arg| !arg.contains("fixture-token")));
        assert_token_env(&cmd);
        assert_eq!(
            args(&cmd),
            [
                "exec",
                "resume",
                "existing session",
                "--json",
                "-m",
                "test-model",
                "--skip-git-repo-check",
                "-c",
                r#"model_reasoning_effort="high""#,
                "-i",
                "/attachments/picture.png",
                "--dangerously-bypass-approvals-and-sandbox",
                "-c",
                r#"mcp_servers.t3rs.command="/test tools/t3d""#,
                "-c",
                r#"mcp_servers.t3rs.args=["mcp","--url","http://127.0.0.1:3780","--thread-id","00000000-0000-0000-0000-000000000000"]"#,
                "-c",
                r#"mcp_servers.t3rs.env_vars=["T3_MCP_TOKEN"]"#,
                "-",
            ]
        );
        assert_eq!(
            prompt,
            "hello\n\nAttachments (file paths):\n- /attachments/notes.txt"
        );
    }

    fn assert_token_env(cmd: &Command) {
        assert!(cmd.as_std().get_envs().any(|(key, value)| {
            key == "T3_MCP_TOKEN" && value.is_some_and(|value| value == "fixture-token")
        }));
        assert!(
            cmd.as_std()
                .get_envs()
                .any(|(key, value)| { key == "T3_BEARER_TOKEN" && value.is_none() })
        );
    }

    #[tokio::test]
    async fn provider_permissions_follow_runtime_and_plan_modes() {
        for (runtime, codex_flags, agy_flags, opencode_auto) in [
            (RuntimeMode::ApprovalRequired, &[][..], &[][..], false),
            (
                RuntimeMode::AutoAcceptEdits,
                &["-s", "workspace-write"][..],
                &["--mode", "accept-edits"][..],
                false,
            ),
            (
                RuntimeMode::FullAccess,
                &["--dangerously-bypass-approvals-and-sandbox"][..],
                &["--mode", "auto", "--dangerously-skip-permissions"][..],
                true,
            ),
        ] {
            for interaction in [InteractionMode::Default, InteractionMode::Plan] {
                let plan = interaction == InteractionMode::Plan;
                let mut req = test_request(ProviderId::Codex);
                req.resume_session_id = Some("fixture-session".into());
                req.runtime_mode = runtime;
                req.interaction_mode = interaction;

                let mut cmd = CodexDriver::new("codex").0.command();
                codex_args(&mut cmd, &req);
                let codex = args(&cmd);
                let permissions: Vec<_> = codex
                    .iter()
                    .map(String::as_str)
                    .filter(|arg| {
                        matches!(
                            *arg,
                            "-s" | "read-only"
                                | "workspace-write"
                                | "--full-auto"
                                | "--dangerously-bypass-approvals-and-sandbox"
                        )
                    })
                    .collect();
                assert_eq!(
                    permissions,
                    if plan {
                        &["-s", "read-only"]
                    } else {
                        codex_flags
                    }
                );
                if plan {
                    assert_eq!(&codex[..4], ["exec", "-s", "read-only", "resume"]);
                }

                let (cmd, _) = AntigravityDriver::new("agy").build_command(&req);
                let agy = args(&cmd);
                let permissions: Vec<_> = agy
                    .iter()
                    .map(String::as_str)
                    .filter(|arg| {
                        matches!(
                            *arg,
                            "--mode"
                                | "plan"
                                | "auto"
                                | "accept-edits"
                                | "--dangerously-skip-permissions"
                        )
                    })
                    .collect();
                assert_eq!(
                    permissions,
                    if plan { &["--mode", "plan"] } else { agy_flags }
                );

                let mut cmd = OpenCodeDriver::new("opencode").0.command();
                opencode_args(&mut cmd, &req);
                assert_eq!(
                    args(&cmd).iter().any(|arg| arg == "--auto"),
                    opencode_auto && !plan
                );
            }
        }
    }

    #[tokio::test]
    async fn antigravity_uses_standalone_streaming_and_isolated_data_dir() {
        let mut req = request(ProviderId::Antigravity);
        req.runtime_mode = RuntimeMode::AutoAcceptEdits;
        let driver = AntigravityDriver::with_data_dir("/opt/homebrew/bin/agy", "/isolated state");
        let (cmd, input) = driver.build_command(&req);
        assert!(matches!(input, Input::None));
        assert_eq!(cmd.as_std().get_program(), "/opt/homebrew/bin/agy");
        assert_eq!(
            args(&cmd),
            [
                "--output-format",
                "stream-json",
                "--model",
                "test-model",
                "--effort",
                "high",
                "--mode",
                "accept-edits",
                "--conversation",
                "existing session",
                "--print",
                "hello\n\nAttachments (file paths):\n- /attachments/picture.png\n- /attachments/notes.txt",
            ]
        );
        assert!(
            cmd.as_std()
                .get_envs()
                .any(|(key, value)| key == "ANTIGRAVITY_APP_DATA_DIR"
                    && value.unwrap() == "/isolated state")
        );
    }

    #[tokio::test]
    async fn opencode_resume_attachments_mode_and_positional_prompt() {
        let mut req = request(ProviderId::OpenCode);
        req.interaction_mode = InteractionMode::Plan;
        req.runtime_mode = RuntimeMode::AutoAcceptEdits;
        req.model_selection.agent = Some("build".into());
        req.prompt = "--version and literal text".into();
        let mut cmd = OpenCodeDriver::new("opencode").0.command();
        assert!(matches!(opencode_args(&mut cmd, &req), Input::None));
        assert_eq!(
            args(&cmd),
            [
                "run",
                "--format",
                "json",
                "--thinking",
                "-m",
                "test-model",
                "--variant",
                "high",
                "--agent",
                "plan",
                "--dir",
                "/workspace with spaces",
                "-s",
                "existing session",
                "-f",
                "/attachments/picture.png",
                "-f",
                "/attachments/notes.txt",
                "--",
                "--version and literal text",
            ]
        );
    }

    #[test]
    fn cli_drivers_report_mcp_capability() {
        assert!(ClaudeDriver::new("claude").supports_mcp());
        assert!(CodexDriver::new("codex").supports_mcp());
        assert!(OpenCodeDriver::new("opencode").supports_mcp());
        assert!(!AntigravityDriver::new("agy").supports_mcp());
        assert!(AntigravityDriver::new("agy").with_mcp(true).supports_mcp());
        assert!(
            !AntigravityDriver::new("agy")
                .with_mcp(true)
                .with_mcp(false)
                .supports_mcp()
        );
    }

    #[tokio::test]
    async fn antigravity_mcp_env_injection_enabled_and_disabled_without_disk_writes() {
        let data_dir = std::env::temp_dir().join(format!("t3-agy-mcp-test-{}", Uuid::new_v4()));
        let req = request(ProviderId::Antigravity);
        let expected_thread_id = req.mcp_injection.as_ref().unwrap().thread_id.to_string();

        // Disabled by default: never injects T3_SERVER_URL / T3_THREAD_ID / T3_MCP_TOKEN.
        let disabled = AntigravityDriver::with_data_dir("agy", &data_dir);
        assert!(!disabled.supports_mcp());
        let (disabled_cmd, input) = disabled.build_command(&req);
        assert!(matches!(input, Input::None));
        assert!(
            args(&disabled_cmd)
                .iter()
                .all(|arg| !arg.contains("fixture-token"))
        );
        assert!(
            disabled_cmd
                .as_std()
                .get_envs()
                .all(|(key, _)| key != "T3_SERVER_URL" && key != "T3_THREAD_ID")
        );
        assert!(
            disabled_cmd
                .as_std()
                .get_envs()
                .any(|(key, value)| key == "T3_MCP_TOKEN" && value.is_none())
        );

        // Enabled: injects per-run env vars ONLY, never in args and never written to disk.
        let enabled = AntigravityDriver::with_data_dir("agy", &data_dir).with_mcp(true);
        assert!(enabled.supports_mcp());
        let (enabled_cmd, input) = enabled.build_command(&req);
        assert!(matches!(input, Input::None));
        assert!(
            args(&enabled_cmd)
                .iter()
                .all(|arg| !arg.contains("fixture-token"))
        );
        assert_token_env(&enabled_cmd);
        assert!(enabled_cmd.as_std().get_envs().any(|(key, value)| {
            key == "T3_SERVER_URL" && value.is_some_and(|val| val == "http://127.0.0.1:3780")
        }));
        assert!(enabled_cmd.as_std().get_envs().any(|(key, value)| {
            key == "T3_THREAD_ID" && value.is_some_and(|val| val == expected_thread_id.as_str())
        }));

        // Enabled with empty token: sets URL/thread ID but omits T3_MCP_TOKEN.
        let mut no_tok_req = request(ProviderId::Antigravity);
        no_tok_req
            .mcp_injection
            .as_mut()
            .unwrap()
            .internal_token
            .clear();
        let (no_tok_cmd, _) = enabled.build_command(&no_tok_req);
        assert!(
            no_tok_cmd
                .as_std()
                .get_envs()
                .any(|(key, value)| key == "T3_MCP_TOKEN" && value.is_none())
        );
        assert!(no_tok_cmd.as_std().get_envs().any(|(key, value)| {
            key == "T3_SERVER_URL" && value.is_some_and(|val| val == "http://127.0.0.1:3780")
        }));

        // Zero token or config written to disk.
        assert!(!data_dir.exists());
    }

    #[tokio::test]
    async fn opencode_injects_mcp_via_env_without_embedding_secret() {
        let req = request(ProviderId::OpenCode);
        let mut cmd = OpenCodeDriver::new("opencode").0.command();
        assert!(matches!(opencode_args(&mut cmd, &req), Input::None));

        assert!(args(&cmd).iter().all(|arg| !arg.contains("fixture-token")));
        assert_token_env(&cmd);

        let config_raw = cmd
            .as_std()
            .get_envs()
            .find_map(|(key, value)| {
                (key == "OPENCODE_CONFIG_CONTENT")
                    .then(|| value.and_then(|val| val.to_str()))
                    .flatten()
            })
            .expect("OPENCODE_CONFIG_CONTENT env should be set")
            .to_owned();
        assert!(!config_raw.contains("fixture-token"));
        let config: serde_json::Value = serde_json::from_str(&config_raw).unwrap();
        assert_eq!(
            config,
            json!({
                "mcp": {
                    "t3rs": {
                        "type": "local",
                        "command": [
                            "/test tools/t3d",
                            "mcp",
                            "--url",
                            "http://127.0.0.1:3780",
                            "--thread-id",
                            "00000000-0000-0000-0000-000000000000"
                        ],
                        "environment": {
                            "T3_MCP_TOKEN": "{env:T3_MCP_TOKEN}"
                        }
                    }
                }
            })
        );

        let plain_req = test_request(ProviderId::OpenCode);
        let mut plain_cmd = OpenCodeDriver::new("opencode").0.command();
        opencode_args(&mut plain_cmd, &plain_req);
        assert!(
            plain_cmd
                .as_std()
                .get_envs()
                .any(|(key, value)| key == "T3_MCP_TOKEN" && value.is_none())
        );
        assert!(
            plain_cmd
                .as_std()
                .get_envs()
                .all(|(key, _)| key != "OPENCODE_CONFIG_CONTENT")
        );
    }

    #[tokio::test]
    async fn every_provider_command_disables_telemetry() {
        for cli in [
            ClaudeDriver::new("claude").0,
            CodexDriver::new("codex").0,
            AntigravityDriver::new("agy").cli,
            OpenCodeDriver::new("opencode").0,
        ] {
            let command = cli.command();
            for key in [
                "DO_NOT_TRACK",
                "AGY_DISABLE_TELEMETRY",
                "DISABLE_TELEMETRY",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
            ] {
                assert!(
                    command
                        .as_std()
                        .get_envs()
                        .any(|(name, value)| name == key && value.unwrap() == "1")
                );
            }
        }
    }
}
