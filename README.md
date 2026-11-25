# t3code-rs

A zero-telemetry, privacy-hardened Rust rewrite of [T3 Code](https://github.com/pingdotgg/t3code) with a native macOS desktop app (`WKWebView`), embedded zero-build ES Modules Web/Mobile PWA, native `/swarm` multi-agent board, and real-time context window & quota tracking.

## Highlights

- **Zero Telemetry & Zero Credential File Access**
  - Never reads `~/.codex/auth.json`, `~/.claude.json`, `~/.t3/userdata/clerk`, or `~/.config/opencode/config.json`.
  - Zero PostHog / OTLP / Clerk / external relay calls; zero external CDN requests (strict CSP, local system fonts only).
  - Child CLIs are always launched with `DO_NOT_TRACK=1`, `AGY_DISABLE_TELEMETRY=1`, `DISABLE_TELEMETRY=1`, and `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`.
  - Self-auditable at any time with `t3d audit-privacy`.
- **T3 Code-Inspired Right Sidebar & Context Window Inspector (`⌘I`)**
  - **Context Window Ring & Multi-Segment Bar**: Real-time token usage (`live` → SQLite `stored` → transcript `session` → `run` → `estimated`) with cache-read / cache-write / fresh-input / output breakdown and 75% / 90% warnings.
  - **SQLite Context Persistence**: Context snapshots survive daemon restarts (`thread_context` table).
  - **Context Breakdown**: Per-item-kind token/byte composition (messages, reasoning, tool I/O, commands, file changes, subagents).
  - **Unified Right Rail**: Switch smoothly between **Context**, **Git Diff** (with **All / Latest Turn** scope filter and zero-dependency syntax highlighting), and **Swarm** tabs — with a draggable resize handle persisted in `localStorage`.
- **Multi-Provider Engine & Native `/swarm` Orchestration**
  - Supports **Claude Code**, **Codex**, **Antigravity (Gemini)**, **OpenCode** (free cloud & local LLMs), plus a deterministic **Mock** provider for testing.
  - Built-in JSON-RPC 2.0 MCP server (`POST /mcp` + `t3d mcp` stdio bridge) for cross-provider `delegate_task` (`shared` / `git worktree` isolation), `thread_fork` (with automatic redacted handoff summaries), and `/swarm` shared message board coordination.
  - Per-run internal MCP tokens bound to caller project scope and enforcing child permission ceilings (`child <= parent`).
- **Composer Command Palette (`/`) & Direct Shell Execution (`!`)**
  - Type `/` in the composer for an interactive, typo-tolerant slash command palette with keyboard navigation (`↑`/`↓`, `Tab`, `Enter`, `Esc`) and argument completion (`/compact`, `/new`, `/model`, `/effort`, `/plan`, `/mode`, `/diff`, `/context`, `/swarm`, `/fork`, `/delegate`, `/title`, `/pin`, `/archive`, `/search`, `/usage`, `/limits`, `/theme`, `/sh`, `/help`, plus provider-native commands).
  - Type `!<command>` (or `/sh <command>`) to run shell commands directly in the active project or git worktree with zero-telemetry env isolation, bounded output, process-group timeout cleanup, and persistent timeline recording.
- **Usage & Quota Limits Dashboard (`⌘U`)**
  - Interactive daily/hourly cost & token charts, cache hit rates, model rankings, DB compression stats, and pooled provider quota limits (Claude, Codex with reset-credit redemption, and Antigravity).
- **Ultra-Low Memory & Storage Footprint**
  - Single native Rust binary (`t3d`) + lightweight native macOS app (`T3 Code RS.app`, ~19 MB, no Electron / no Node runtime).
  - SQLite WAL + transparent **zstd** compression (`>4 KiB` payloads) + **blake3** content-addressed deduplication + Japanese-capable **FTS5 trigram** full-text search + `t3d gc`.
  - Vanilla ES Modules frontend (`web/app.js` + `web/js/*.js`) with zero build step and zero runtime dependencies.

## Quick Start

### 1. Build & Run Locally (`127.0.0.1:3774`, zero outbound network)

```bash
cargo build --release
./target/release/t3d serve
```

Open `http://127.0.0.1:3774/` in your browser.

### 2. Build Native macOS App (`T3 Code RS.app`)

```bash
./scripts/build-macos-app.sh
```

This builds `t3d` and `t3code-rs-app`, generates the app icon, ad-hoc signs the bundle, and installs it atomically to `dist/T3 Code RS.app`.

```bash
# Optional: copy into /Applications and ~/.local/bin
cp -R "dist/T3 Code RS.app" /Applications/
cp "dist/T3 Code RS.app/Contents/MacOS/t3d" ~/.local/bin/t3d

# Optional: run t3d in the background via launchd
t3d launchd-plist --port 3774 > ~/Library/LaunchAgents/com.local.t3code-rs.plist
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/com.local.t3code-rs.plist
```

## Remote Access (Optional)

### Cloudflare Access + Tunnel

```bash
# Print ready-to-use cloudflared ingress YAML & Zero Trust Access checklist
t3d cf-tunnel --hostname t3.example.com --port 3774

# Verify RS256 Cloudflare Access JWTs (header + CF_Authorization cookie for WebSockets)
t3d serve \
  --host 127.0.0.1 \
  --port 3774 \
  --auth cloudflare-access \
  --cf-team-domain yourteam.cloudflareaccess.com \
  --cf-audience <your-application-aud-tag> \
  --cf-allowed-email you@example.com
```

### Tailscale Serve

```bash
tailscale serve --bg http://127.0.0.1:3774
t3d serve \
  --host 127.0.0.1 \
  --port 3774 \
  --auth tailscale \
  --tailscale-login you@example.com
```

Tailscale mode enforces loopback peer IP, `*.ts.net` (or loopback) `Host` header validation (blocking DNS rebinding), and an explicit non-empty `--tailscale-login` allowlist.

## Keyboard Shortcuts

| Shortcut (macOS / Linux & Windows) | Action |
| :--- | :--- |
| `⌘K` / `Ctrl+K` | Full-text search across threads & tool outputs |
| `⌘N` / `Ctrl+N` | New thread |
| `⌘B` / `Ctrl+B` | Toggle left sidebar |
| `⌘I` / `Ctrl+I` | Toggle right Context & Thread Details panel |
| `⌘D` / `Ctrl+D` | Toggle right Git Diff panel |
| `⌘U` / `Ctrl+U` | Open Usage & Quota Limits dashboard |
| `⌥S` / `Alt+S` | Toggle Swarm board (when active on thread) |
| `/` | Focus composer |

## CLI Reference

```bash
t3d serve [--config ~/.t3-rs/config.toml]   # Start HTTP/WS/MCP server
t3d audit-privacy                           # Verify zero-telemetry & outbound policy invariants
t3d stats                                   # Show provider/model orchestration & DB compression stats
t3d gc [--apply]                            # Inspect or compact orphan blobs, attachments, and SQLite DB
t3d import-projects                         # Read-only project import from official T3 Code DB
t3d import-history                          # Full read-only thread history import from official T3 Code DB
t3d mcp                                     # Stdio MCP bridge to running t3d server
t3d launchd-plist --port 3774               # Generate macOS LaunchAgent plist
```

## Architecture

### Rust Workspace (`crates/`)

| Crate | Responsibility |
| :--- | :--- |
| `crates/core` (`t3-core`) | Domain types, config validation, privacy invariants, provider/store traits, WS/stream protocols |
| `crates/store` (`t3-store`) | SQLite WAL, zstd + blake3 deduplication, Japanese FTS5 trigram search, persisted thread context, GC, history import |
| `crates/git` (`t3-git`) | Hardened Git worktree lifecycle & whitespace-ignoring diffs |
| `crates/maki` (`t3-maki`) | Local syntax-aware code indexing, slicing, grep, and diff utilities |
| `crates/providers` (`t3-providers`) | Claude / Codex / Antigravity / OpenCode / Mock CLI drivers, NDJSON parsers, process-group supervision, rate-limit retry |
| `crates/mcp` (`t3-mcp`) | JSON-RPC 2.0 MCP server (`2025-03-26`) for orchestration & Swarm tools |
| `crates/server` (`t3-server`) | Axum HTTP/WS server, Cloudflare/Tailscale/Bearer auth, Usage/Limits probes, embedded `web/` assets |
| `crates/desktop` (`t3-desktop`) | Native macOS `WKWebView` shell (`T3 Code RS.app`) |
| `crates/cli` (`t3d`) | Single-binary CLI entrypoint |

### Frontend (`web/` — Zero-Build ES Modules)

- `web/index.html`, `web/styles.css`, `web/manifest.json`
- `web/app.js` — Main application state, API/WebSocket client, sidebar, timeline, composer, Usage/Limits, Swarm
- `web/js/util.js` — DOM helpers, SVG icons, formatters, desktop IPC, domain constants
- `web/js/highlight.js` — Zero-dependency XSS-safe syntax highlighter (Rust, JS/TS, Python, Go, Shell, JSON, TOML, YAML, SQL, CSS, HTML, C-like)
- `web/js/markdown.js` — Safe Markdown renderer with highlighted code blocks and thread/path chips
- `web/js/patch.js` — Unified diff renderer with syntax highlighting and latest-turn file scoping
- `web/js/rail.js` — Draggable right-rail resizer with `localStorage` persistence and keyboard support
- `web/js/context-ui.js` — Context window donut ring and inspector helpers

## Development & Verification

```bash
# Rust lint & full workspace test suite
cargo clippy --all-targets -- -D warnings
cargo test

# Frontend syntax check
for f in web/app.js web/js/*.js; do node --check "$f"; done
```

See also [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [docs/PROTOCOLS.md](docs/PROTOCOLS.md), and [docs/NETWORK_CALLS.md](docs/NETWORK_CALLS.md).

## License

MIT — see [LICENSE](LICENSE).
