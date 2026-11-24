# Verified Local CLI Protocols & Security Spec

Verified directly against locally installed binaries on 2026-11-24.

## 1. Claude Code (`/opt/homebrew/bin/claude`, v2.1.330)
- **Non-interactive streaming invocation**:
  ```
  claude -p \
    --output-format stream-json \
    --input-format stream-json \
    --verbose \
    --include-partial-messages \
    --model <model> \
    --effort <low|medium|high|xhigh|max> \
    --permission-mode <default|acceptEdits|auto|plan|bypassPermissions> \
    [--dangerously-skip-permissions] \
    [--resume <session_id>] \
    [--mcp-config <json_path_or_string>]
  ```
- **Stdin (`stream-json`)**: NDJSON user messages:
  `{"type":"user","message":{"role":"user","content":"<prompt>"}}`
  Because stdin stays open in `stream-json` mode, **mid-turn `steer`** can write an additional NDJSON user message line directly to stdin!
- **Stdout (`stream-json`)**: NDJSON events:
  - `{"type":"system","subtype":"init","session_id":"..."}` -> capture `provider_session_id`
  - `{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"..."}}}` -> `AssistantDelta`
  - `{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","text":"..."}}}` -> `ReasoningDelta`
  - `{"type":"assistant","message":{"content":[...]}}` -> `ToolCall` / `AssistantMessage`
  - `{"type":"user","message":{"content":[{"type":"tool_result",...}]}}` -> `ToolResult`
  - `{"type":"result","subtype":"success"|"error","result":"...","usage":{"input_tokens":...,"output_tokens":...}}` -> `Usage` + `Completed` / `Failed`

## 2. Codex (`/opt/homebrew/bin/codex`, v0.171.0)
- **Non-interactive streaming invocation**:
  ```
  codex exec --json \
    -m <model> \
    -c model_reasoning_effort="<low|medium|high|xhigh>" \
    [--skip-git-repo-check] \
    [--dangerously-bypass-approvals-and-sandbox | --full-auto] \
    [-i <image_path> ...] \
    [-c mcp_servers.t3rs.command="<t3d_path>" -c 'mcp_servers.t3rs.args=["mcp","--url","..."]'] \
    -
  ```
  (Prompt passed via stdin when `-` is given, avoiding argv limits.)
- **Resume**: `codex exec resume <session_id> --json ... -`
- **Stdout (`--json`)**: JSONL events:
  - `{"type":"thread.started","thread_id":"..."}` -> capture `provider_session_id`
  - `{"type":"item.started"|"item.updated"|"item.completed","item":{"type":"agent_message"|"reasoning"|"command_execution"|"file_change"|"mcp_tool_call",...}}`
  - `{"type":"turn.completed","usage":{"input_tokens":...,"output_tokens":...}}`
  - `{"type":"turn.failed","error":{"message":"..."}}`

## 3. Antigravity / Gemini (`/opt/homebrew/bin/agy`, v1.5.13)
- **Standalone — zero dependency on official T3 Code!**
- Supports BOTH `agy -p --output-format stream-json --input-format stream-json` AND `agy acp --stdio --disable-telemetry`:
  ```
  agy -p \
    --output-format stream-json \
    --input-format stream-json \
    --model <model> \
    --effort <low|medium|high|xhigh|max> \
    --mode <auto|accept-edits|plan> \
    [--dangerously-skip-permissions] \
    [--conversation <conversation_id>]
  ```
  Note: Always pass `AGY_DISABLE_TELEMETRY=1` (and `--disable-telemetry` when using `acp`) and isolate data dir under `~/.t3-rs/providers/antigravity` if configured.
- **Stdout (`stream-json`)**: NDJSON lines carrying assistant deltas, reasoning/thinking chunks, tool events, `conversation_id` / `session_id`, and final result.

## 4. OpenCode (`/opt/homebrew/bin/opencode`, v1.19.6)
- **Non-interactive streaming invocation**:
  ```
  opencode run \
    --format json \
    --auto \
    --thinking \
    -m <provider/model> \
    [--variant <low|medium|high|max>] \
    [--agent <build|plan>] \
    [--dir <workspace_dir>] \
    [-s <session_id>] \
    [-f <attachment_path> ...] \
    -- "<prompt>"
  ```
- **Known local + free models on this machine**:
  - Free cloud: `opencode/deepseek-v4.1-flash-free`, `opencode/flint-40b-free`, `opencode/beam-free`
  - Local: `local/gemma4-31b`, `win10-ds4p/ds4p-9b-heretic`, `local-uncensored/uncensored-4b`
- **Stdout (`--format json`)**: NDJSON events with `type` (`text`, `reasoning`, `tool_use`, `tool_result`, `step_finish`, `error`) and `sessionID`.

## 5. Cloudflare Access Zero Trust JWT Spec
- Token locations (check in order):
  1. HTTP header `Cf-Access-Jwt-Assertion`
  2. Cookie `CF_Authorization` (critical for browser WebSocket `/ws` upgrades!)
- JWKS endpoint: `https://<team_domain>/cdn-cgi/access/certs`
  - Response JSON: `{"keys":[{"kid":"...","kty":"RSA","alg":"RS256","n":"...","e":"AQAB",...}]}`
  - Cache JWKS in memory (TTL 15 min) and force-refresh once on unknown `kid` (key rotation).
- Validation checks:
  - Algorithm strictly `RS256` (reject `none`, HS256, etc.)
  - `aud` contains configured `audience`
  - `iss` matches `https://<team_domain>` (when not using `jwks_url_override`)
  - `exp` and `nbf` validated with small leeway (60s)
  - If `allowed_emails` is non-empty, JWT `email` claim must match (case-insensitive)

## 6. Upstream Telemetry Blacklist (NEVER INCLUDE)
Upstream `pingdotgg/t3code/apps/server/src/telemetry/` contained:
- `Identify.ts`: read `~/.codex/auth.json` (`tokens.account_id`) and `~/.claude.json` (`userID`)
- `AnalyticsService.ts`, `DelegatedTaskAnalytics.ts`, `FeatureUsage.ts`, `ProviderDimensions.ts`, `ProviderUsageAnalytics.ts`
- Clerk (`clerk-tokens.json`) and T3 Connect cloud relay
`t3code-rs` MUST contain zero references to reading these files or contacting any telemetry/Clerk/relay domains.
