# t3code-rs

Zero-telemetry, privacy-hardened personal Rust rewrite of [T3 Code](https://github.com/pingdotgg/t3code), optimized for high-frequency multi-provider AI coding workflows on macOS / Linux.

## Why `t3code-rs`?

1. **Zero Telemetry & Zero Credential File Access**
   - Never reads `~/.codex/auth.json`, `~/.claude.json`, `~/.t3/userdata/clerk`, or `~/.config/opencode/config.json`.
   - Zero PostHog / OTLP / Clerk / T3 Connect relay calls.
   - The **only** permitted outbound HTTP call in the entire binary is the explicitly configured Cloudflare Access JWKS endpoint (`https://<team>/cdn-cgi/access/certs`) when `--auth cloudflare-access` is active.
   - Child CLIs are always launched with `DO_NOT_TRACK=1`, `AGY_DISABLE_TELEMETRY=1`, `DISABLE_TELEMETRY=1`, and `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`.
2. **Ultra-Low Memory & Storage Footprint**
   - Single native Rust binary (`t3d`) with embedded Japanese-first Web/Mobile PWA (no Electron, no Node runtime).
   - SQLite WAL + transparent **zstd compression** (>4 KiB payloads) + **blake3 content-addressed deduplication** for large tool outputs.
   - Built-in Japanese full-text search via **SQLite FTS5 (`tokenize='trigram'`)** with short-query fallback (1–2 chars).
3. **Focused on 4 Real Providers (+ Mock)**
   - `claude` (`/opt/homebrew/bin/claude`, `stream-json` bidirectional streaming + mid-turn `steer` + `--resume`)
   - `codex` (`/opt/homebrew/bin/codex`, `codex exec --json` + resume + Plan read-only sandbox)
   - `antigravity` (standalone `/opt/homebrew/bin/agy`, zero dependency on official T3 Code)
   - `opencode` (`/opt/homebrew/bin/opencode`, free cloud + local LLM models)
4. **Native Multi-Provider Orchestration**
   - Built-in MCP server (`POST /mcp` + `t3d mcp` stdio bridge) exposing `orchestrator_capabilities`, `delegate_task` (`shared` / `worktree` isolation), `task_status`, `task_cancel`, `thread_list`, `thread_read`, `thread_send`, `thread_fork`, and `orch_stats`.
   - Internal MCP tokens are passed via `T3_MCP_TOKEN` env var (never argv), bound to caller project scope, and enforce child permission ceiling (`child <= parent`).
   - Clean Unix process-group supervision (`setpgid` + `SIGTERM`/`SIGKILL` escalation + graceful server `SIGTERM` shutdown).

## Quick Start

### 1. Build
```bash
cargo build --release
```
Binary path: `target/release/t3d` (optional: `cp target/release/t3d ~/.local/bin/t3d`).

### 2. Run Locally (Default: `127.0.0.1:3774`, zero outbound network)
```bash
./target/release/t3d serve
```
Open `http://127.0.0.1:3774/` in your browser.

### 3. Remote Access with Cloudflare Access + Tunnel
```bash
# Print ready-to-use cloudflared ingress YAML & Zero Trust Access checklist
./target/release/t3d cf-tunnel --hostname t3.example.com --port 3774

# Start server verifying RS256 Cloudflare Access JWTs (header + CF_Authorization cookie for WebSockets)
./target/release/t3d serve \
  --host 127.0.0.1 \
  --port 3774 \
  --auth cloudflare-access \
  --cf-team-domain yourteam.cloudflareaccess.com \
  --cf-audience <your-application-aud-tag> \
  --cf-allowed-email you@example.com
```
> Note: Authenticated users have project/agent access according to the thread's `RuntimeMode` (default `approval-required`; `full-access` must be explicitly chosen). Always configure `--cf-allowed-email` in addition to your Cloudflare Access policy.

### 4. Remote Access with Tailscale Serve
```bash
tailscale serve --bg http://127.0.0.1:3774
./target/release/t3d serve \
  --host 127.0.0.1 \
  --port 3774 \
  --auth tailscale \
  --tailscale-login you@example.com
```
Tailscale mode enforces loopback peer IP, `*.ts.net` (or loopback) `Host` header (blocking DNS rebinding), and an explicit non-empty `--tailscale-login` allowlist.

### 5. Useful CLI Commands
```bash
# Privacy & outbound policy self-audit
./target/release/t3d audit-privacy

# Multi-provider orchestration & DB compression stats (like orch-stats)
./target/release/t3d stats

# Optional read-only project import from official T3 Code (reads ONLY projection_projects)
./target/release/t3d import-projects

# Generate macOS launchd plist for lightweight background startup
./target/release/t3d launchd-plist --port 3774 > ~/Library/LaunchAgents/com.local.t3code-rs.plist
```

## Workspace Crates
- `crates/core` (`t3-core`): Domain types, config validation, privacy invariants, traits
- `crates/store` (`t3-store`): SQLite WAL + zstd/blake3 dedup + Japanese FTS5 trigram + stats
- `crates/git` (`t3-git`): Hardened git worktree lifecycle & whitespace-ignoring diffs
- `crates/providers` (`t3-providers`): Claude / Codex / Antigravity / OpenCode / Mock drivers + process-group supervision + rate-limit auto-resume
- `crates/mcp` (`t3-mcp`): JSON-RPC 2.0 MCP server (`2025-03-26`)
- `crates/server` (`t3-server`): Axum HTTP/WS server, Cloudflare Access / Tailscale / Bearer auth, embedded `web/` assets
- `crates/cli` (`t3d`): Single binary CLI entrypoint
- `web/`: Zero-CDN Japanese-first Web & Mobile PWA
