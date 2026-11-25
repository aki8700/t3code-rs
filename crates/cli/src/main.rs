use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::Value;
use std::{
    collections::HashSet,
    error::Error,
    fmt::Write as _,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::ExitCode,
};
use t3_core::{
    AuthConfig, DbStats, ModelSelection, OrchModelStat, ProviderId, ServerConfig, Store,
};
use t3_store::SqliteStore;
use uuid::Uuid;

mod client;

type CliResult<T> = Result<T, Box<dyn Error + Send + Sync>>;
const DEFAULT_URL: &str = "http://127.0.0.1:3774";

#[derive(Parser)]
#[command(version, about = "Run and manage a local t3code-rs server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the HTTP, WebSocket, and MCP server.
    Serve(ServeArgs),
    /// Forward newline-delimited MCP JSON-RPC between stdio and the server.
    Mcp {
        #[arg(long, env = "T3_SERVER_URL", default_value = DEFAULT_URL)]
        url: String,
        #[arg(long, env = "T3_MCP_TOKEN", hide_env_values = true)]
        token: String,
        #[arg(long, env = "T3_THREAD_ID")]
        thread_id: Option<Uuid>,
    },
    /// Register or remove the static zero-secret `t3rs` MCP entry in Antigravity (`agy`).
    SetupAgyMcp {
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        remove: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Query the server's health endpoint.
    Status {
        #[arg(long, default_value = DEFAULT_URL)]
        url: String,
    },
    /// List threads from a running server.
    Threads {
        #[arg(long, default_value = "http://127.0.0.1:3774")]
        url: String,
        #[arg(long, env = "T3_BEARER_TOKEN", hide_env_values = true)]
        token: Option<String>,
        #[arg(long)]
        archived: bool,
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Send a prompt message to a thread.
    Send {
        thread_id: Uuid,
        prompt: String,
        #[arg(long, default_value = "http://127.0.0.1:3774")]
        url: String,
        #[arg(long, env = "T3_BEARER_TOKEN", hide_env_values = true)]
        token: Option<String>,
        #[arg(long)]
        behavior: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Interrupt an active run on a thread.
    Interrupt {
        thread_id: Uuid,
        #[arg(long, default_value = "http://127.0.0.1:3774")]
        url: String,
        #[arg(long, env = "T3_BEARER_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
    /// Query provider usage limits from a running server.
    Limits {
        #[arg(long, default_value = "http://127.0.0.1:3774")]
        url: String,
        #[arg(long, env = "T3_BEARER_TOKEN", hide_env_values = true)]
        token: Option<String>,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// Print local orchestration and storage statistics.
    Stats {
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Report orphan storage, or remove it and compact the database with --apply.
    Gc {
        /// Remove unreferenced storage. Stop the server first.
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Read only project records from a legacy T3 SQLite database.
    ImportProjects {
        #[arg(long)]
        from: Option<PathBuf>,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Import history from a read-only legacy T3 SQLite database.
    ImportHistory {
        #[arg(long)]
        from: Option<PathBuf>,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Print the configured privacy policy and validation result as JSON.
    AuditPrivacy {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Print Cloudflare tunnel ingress YAML and Access policy setup.
    CfTunnel {
        #[arg(long)]
        hostname: String,
        #[arg(long, default_value_t = 3774)]
        port: u16,
    },
    /// Print a macOS LaunchAgent configuration for t3d.
    LaunchdPlist {
        #[arg(long, default_value_t = 3774)]
        port: u16,
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

#[derive(Args)]
struct ServeArgs {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    host: Option<String>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    data_dir: Option<PathBuf>,
    #[arg(long, value_enum)]
    auth: Option<AuthMode>,
    #[arg(long, env = "T3_BEARER_TOKEN", hide_env_values = true)]
    bearer_token: Option<String>,
    #[arg(long)]
    cf_team_domain: Option<String>,
    #[arg(long)]
    cf_audience: Option<String>,
    #[arg(long)]
    cf_allowed_email: Vec<String>,
    #[arg(long)]
    tailscale_login: Vec<String>,
    #[arg(long)]
    enable_mock: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum AuthMode {
    LocalOnly,
    Bearer,
    Tailscale,
    CloudflareAccess,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut builder = match &cli.command {
        Command::Serve(_) => tokio::runtime::Builder::new_multi_thread(),
        _ => tokio::runtime::Builder::new_current_thread(),
    };
    let result = builder
        .enable_all()
        .build()
        .map_err(Into::into)
        .and_then(|runtime| runtime.block_on(run(cli)));
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("t3d: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> CliResult<ExitCode> {
    match cli.command {
        Command::Serve(args) => {
            tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                .with_writer(std::io::stderr)
                .init();
            t3_server::serve(args.into_config()?).await?;
        }
        Command::Mcp {
            url,
            token,
            thread_id,
        } => client::run_stdio_bridge(&url, &token, thread_id).await?,
        Command::SetupAgyMcp {
            url,
            remove,
            dry_run,
        } => return run_setup_agy_mcp(url, remove, dry_run).await,
        Command::Status { url } => {
            let health = client::health(&url).await?;
            println!("{}", serde_json::to_string_pretty(&health)?);
            if health["ok"] != Value::Bool(true) {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Threads {
            url,
            token,
            archived,
            limit,
            json,
        } => {
            let threads = client::list_threads(&url, token.as_deref(), archived, limit).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&threads)?);
            } else {
                print!("{}", format_threads_table(&threads));
            }
        }
        Command::Send {
            thread_id,
            prompt,
            url,
            token,
            behavior,
            json,
        } => {
            let result = client::send_message(
                &url,
                token.as_deref(),
                thread_id,
                &prompt,
                behavior.as_deref(),
            )
            .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                let run_id = result["run_id"].as_str().unwrap_or("-");
                println!("Run {run_id} queued on thread {thread_id}");
            }
        }
        Command::Interrupt {
            thread_id,
            url,
            token,
        } => {
            client::interrupt_thread(&url, token.as_deref(), thread_id).await?;
            println!("Interrupted thread {thread_id}");
        }
        Command::Limits {
            url,
            token,
            refresh,
            json,
        } => {
            let limits = client::usage_limits(&url, token.as_deref(), refresh).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&limits)?);
            } else {
                print!("{}", format_limits_table(&limits));
            }
        }
        Command::Stats { data_dir } => {
            let cfg = local_config(data_dir);
            let store = local_store(Some(cfg.data_dir.clone()))?;
            let (models, db) =
                tokio::try_join!(t3_server::orchestration_stats(&cfg), store.db_stats())?;
            print_stats(&models, &db);
        }
        Command::Gc { apply, data_dir } => {
            let cfg = local_config(data_dir);
            let store = SqliteStore::open_for_gc(&cfg.db_path(), cfg.storage.clone(), apply)?;
            let report = store.gc_storage(&cfg.attachments_dir(), apply).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            gc_orphan_worktrees(&store, &cfg.worktrees_dir(), apply).await;
        }
        Command::ImportProjects { from, data_dir } => {
            let cfg = local_config(data_dir);
            let from = legacy_source(from, &cfg.db_path())?;
            let count = local_store(Some(cfg.data_dir))?
                .import_projects_from_t3_sqlite(&from)
                .await?;
            println!("Imported {count} project(s).");
        }
        Command::ImportHistory { from, data_dir } => {
            let cfg = local_config(data_dir);
            let from = legacy_source(from, &cfg.db_path())?;
            let report = local_store(Some(cfg.data_dir))?
                .import_history_from_t3_sqlite(&from)
                .await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::AuditPrivacy { config } => {
            let report = t3_server::audit_privacy(&read_config(config.as_deref())?);
            println!("{}", serde_json::to_string_pretty(&report)?);
            if !report.passed {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::CfTunnel { hostname, port } => print_tunnel(&hostname, port)?,
        Command::LaunchdPlist { port, config } => print_plist(port, config)?,
    }
    Ok(ExitCode::SUCCESS)
}

impl ServeArgs {
    fn into_config(self) -> CliResult<ServerConfig> {
        let mut cfg = read_config(self.config.as_deref())?;
        if let Some(host) = self.host {
            cfg.host = host;
        }
        if let Some(port) = self.port {
            cfg.port = port;
        }
        if let Some(dir) = self.data_dir {
            cfg.data_dir = dir;
        }
        let mode = self.auth.unwrap_or(match cfg.auth {
            AuthConfig::LocalOnly => AuthMode::LocalOnly,
            AuthConfig::Bearer { .. } => AuthMode::Bearer,
            AuthConfig::Tailscale { .. } => AuthMode::Tailscale,
            AuthConfig::CloudflareAccess { .. } => AuthMode::CloudflareAccess,
        });
        cfg.auth = match (mode, cfg.auth) {
            (AuthMode::LocalOnly, _) => AuthConfig::LocalOnly,
            (AuthMode::Bearer, existing) => {
                let token = self
                    .bearer_token
                    .or(match existing {
                        AuthConfig::Bearer { token } => Some(token),
                        _ => None,
                    })
                    .unwrap_or_default();
                AuthConfig::Bearer { token }
            }
            (AuthMode::Tailscale, existing) => {
                let allowed_logins = match existing {
                    AuthConfig::Tailscale { allowed_logins } if self.tailscale_login.is_empty() => {
                        allowed_logins
                    }
                    _ => self.tailscale_login,
                };
                AuthConfig::Tailscale { allowed_logins }
            }
            (AuthMode::CloudflareAccess, existing) => {
                let (team_domain, audience, allowed_emails, jwks_url_override) = match existing {
                    AuthConfig::CloudflareAccess {
                        team_domain,
                        audience,
                        allowed_emails,
                        jwks_url_override,
                    } => (team_domain, audience, allowed_emails, jwks_url_override),
                    _ => Default::default(),
                };
                AuthConfig::CloudflareAccess {
                    team_domain: self.cf_team_domain.unwrap_or(team_domain),
                    audience: self.cf_audience.unwrap_or(audience),
                    allowed_emails: if self.cf_allowed_email.is_empty() {
                        allowed_emails
                    } else {
                        self.cf_allowed_email
                    },
                    jwks_url_override,
                }
            }
        };
        if self.enable_mock && !cfg.enabled_providers.contains(&ProviderId::Mock) {
            cfg.enabled_providers.push(ProviderId::Mock);
            if self.config.is_none() {
                cfg.default_model = ModelSelection {
                    provider: ProviderId::Mock,
                    model: "mock".into(),
                    effort: None,
                    agent: None,
                };
            }
        }
        Ok(cfg)
    }
}

fn read_config(path: Option<&Path>) -> CliResult<ServerConfig> {
    let defaults = ServerConfig::default();
    let Some(path) = path else {
        return Ok(defaults);
    };
    let mut config = toml::Value::try_from(defaults)?;
    let text = std::fs::read_to_string(path)?;
    let provided = text
        .parse::<toml::Value>()
        .map_err(|_| format!("invalid TOML configuration in {}", path.display()))?;
    merge(&mut config, provided);
    config
        .try_into()
        .map_err(|_| format!("invalid server settings in {}", path.display()).into())
}

fn merge(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                if let Some(old) = base.get_mut(&key) {
                    merge(old, value);
                } else {
                    base.insert(key, value);
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn legacy_source(from: Option<PathBuf>, destination: &Path) -> CliResult<PathBuf> {
    let source = from
        .unwrap_or_else(|| home().join(".t3/userdata/statev2.sqlite"))
        .canonicalize()?;
    if let Ok(destination) = destination.canonicalize() {
        let same_path = source == destination;
        #[cfg(unix)]
        let same_path = {
            use std::os::unix::fs::MetadataExt;
            let source = std::fs::metadata(&source)?;
            let destination = std::fs::metadata(&destination)?;
            same_path || (source.dev(), source.ino()) == (destination.dev(), destination.ino())
        };
        if same_path {
            return Err("source and destination databases must differ".into());
        }
    }
    Ok(source)
}

/// Reports worktree directories that no thread references, and removes them with `apply`.
/// Best-effort: a missing worktrees directory or a failed removal never fails the command.
/// Git keeps its own registry of worktrees, so `git worktree prune` in the source repo
/// is still needed afterwards to drop stale metadata.
async fn gc_orphan_worktrees(store: &SqliteStore, worktrees_dir: &Path, apply: bool) {
    let entries = match fs::read_dir(worktrees_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return,
        Err(error) => {
            eprintln!(
                "Skipped worktree gc in {}: {error}",
                worktrees_dir.display()
            );
            return;
        }
    };
    let threads = match store.list_threads(None, true, usize::MAX).await {
        Ok(threads) => threads,
        Err(error) => {
            eprintln!("Skipped worktree gc: {error}");
            return;
        }
    };
    let referenced: HashSet<PathBuf> = threads
        .iter()
        .filter_map(|thread| thread.worktree_path.as_deref().map(canonical_or_self))
        .collect();
    let orphans: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .filter(|path| !referenced.contains(&canonical_or_self(path)))
        .collect();
    if orphans.is_empty() {
        println!("No orphan worktrees in {}", worktrees_dir.display());
        return;
    }
    for path in &orphans {
        if apply {
            match fs::remove_dir_all(path) {
                Ok(()) => println!("Removed orphan worktree {}", path.display()),
                Err(error) => eprintln!(
                    "Could not remove orphan worktree {}: {error}",
                    path.display()
                ),
            }
        } else {
            println!("Orphan worktree {}", path.display());
        }
    }
    if !apply {
        println!(
            "Run with --apply to remove {} orphan worktree(s).",
            orphans.len()
        );
    }
}

fn canonical_or_self(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

fn local_config(data_dir: Option<PathBuf>) -> ServerConfig {
    let mut cfg = ServerConfig::default();
    if let Some(dir) = data_dir {
        cfg.data_dir = dir;
    }
    cfg
}

fn local_store(data_dir: Option<PathBuf>) -> CliResult<SqliteStore> {
    let cfg = local_config(data_dir);
    t3_core::create_private_dir(&cfg.data_dir)?;
    Ok(SqliteStore::open(cfg.db_path(), cfg.storage)?)
}

fn print_stats(models: &[OrchModelStat], db: &DbStats) {
    println!(
        "{:<14} {:<32} {:>7} {:>7} {:>7} {:>9} {:>10} {:>12} {:>12}",
        "Provider",
        "Model",
        "Runs",
        "Done",
        "Failed",
        "Success",
        "Avg ms",
        "Input tokens",
        "Out tokens"
    );
    for row in models {
        println!(
            "{:<14} {:<32} {:>7} {:>7} {:>7} {:>8.1}% {:>10.0} {:>12} {:>12}",
            row.provider.as_str(),
            row.model,
            row.total_runs,
            row.completed_runs,
            row.failed_runs,
            row.success_rate * 100.0,
            row.avg_duration_ms,
            row.total_input_tokens,
            row.total_output_tokens
        );
    }
    println!();
    for (label, count) in [
        ("Database bytes", db.db_size_bytes),
        ("Threads", db.threads_count),
        ("Runs", db.runs_count),
        ("Items", db.turn_items_count),
        ("Raw payload bytes", db.raw_payload_bytes),
        ("Stored payload bytes", db.stored_payload_bytes),
        ("Deduplicated blobs", db.dedup_blobs_count),
        ("Attachments", db.attachments_count),
    ] {
        println!("{label:<24} {count:>12}");
    }
    println!(
        "{:<24} {:>12.2}",
        "Stored / raw ratio", db.compression_ratio
    );
}

fn print_tunnel(hostname: &str, port: u16) -> CliResult<()> {
    if hostname.len() > 253
        || !hostname.contains('.')
        || hostname.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-')
        })
    {
        return Err("hostname must be a fully qualified DNS name".into());
    }
    println!(
        "ingress:\n  - hostname: \"{hostname}\"\n    service: http://127.0.0.1:{port}\n  - service: http_status:404"
    );
    println!(
        "\n# Zero Trust Access\n# - Create a self-hosted Access application for {hostname}.\n# - Add an Allow policy for the intended identities.\n# - Enable the application for the entire hostname, including /ws.\n# - Start t3d with --auth cloudflare-access --cf-team-domain <team-domain>\n#   --cf-audience <application-audience> --port {port}."
    );
    Ok(())
}

fn setup_agy_mcp_args(executable: &Path, url: Option<&str>, remove: bool) -> Vec<String> {
    if remove {
        vec!["mcp".into(), "remove".into(), "t3rs".into()]
    } else {
        let mut args = vec![
            "mcp".into(),
            "add".into(),
            "t3rs".into(),
            executable.to_string_lossy().into_owned(),
            "--".into(),
            "mcp".into(),
        ];
        if let Some(server_url) = url {
            args.push("--url".into());
            args.push(server_url.into());
        }
        args
    }
}

fn format_agy_mcp_command(args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push("agy".to_string());
    for arg in args {
        if arg.chars().any(char::is_whitespace) {
            parts.push(format!("'{}'", arg.replace('\'', "'\\''")));
        } else {
            parts.push(arg.clone());
        }
    }
    parts.join(" ")
}

async fn run_setup_agy_mcp(
    url: Option<String>,
    remove: bool,
    dry_run: bool,
) -> CliResult<ExitCode> {
    let executable = std::env::current_exe()?;
    let args = setup_agy_mcp_args(&executable, url.as_deref(), remove);
    let rendered = format_agy_mcp_command(&args);
    if dry_run {
        println!("{rendered}");
        return Ok(ExitCode::SUCCESS);
    }
    let status = match tokio::process::Command::new("agy")
        .args(&args)
        .env_remove("T3_BEARER_TOKEN")
        .env_remove("T3_MCP_TOKEN")
        .status()
        .await
    {
        Ok(status) => status,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let fallback = ServerConfig::default().providers.antigravity_bin;
            tokio::process::Command::new(&fallback)
                .args(&args)
                .env_remove("T3_BEARER_TOKEN")
                .env_remove("T3_MCP_TOKEN")
                .status()
                .await
                .map_err(|_| {
                    format!(
                        "could not execute `agy` (tried PATH and {})",
                        fallback.display()
                    )
                })?
        }
        Err(err) => return Err(err.into()),
    };
    if !status.success() {
        return Err(format!("`{rendered}` exited with {status}").into());
    }
    Ok(ExitCode::SUCCESS)
}

fn print_plist(port: u16, config: Option<PathBuf>) -> CliResult<()> {
    let executable = std::env::current_exe()?;
    let mut args = vec![
        executable.to_string_lossy().into_owned(),
        "serve".into(),
        "--port".into(),
        port.to_string(),
    ];
    if let Some(path) = config {
        args.extend([
            "--config".into(),
            std::path::absolute(path)?.to_string_lossy().into_owned(),
        ]);
    }
    println!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.local.t3code-rs</string>
  <key>ProgramArguments</key>
  <array>"#
    );
    for arg in args {
        let escaped = arg
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        println!("    <string>{escaped}</string>");
    }
    println!(
        "  </array>\n  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <true/>\n</dict>\n</plist>"
    );
    Ok(())
}

fn format_threads_table(value: &Value) -> String {
    let Some(rows) = value.as_array() else {
        return format!("{value}\n");
    };
    if rows.is_empty() {
        return "No threads.\n".into();
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<36}  {:<12}  {:<24}  {:<19}  Title",
        "ID", "Status", "Model", "Updated"
    );
    for row in rows {
        let id = row["id"].as_str().unwrap_or("-");
        let status = row["status"].as_str().unwrap_or("-");
        let model = format_model_label(&row["model_selection"]);
        let updated = format_timestamp(row["updated_at"].as_str().unwrap_or("-"));
        let raw_title = row["title"].as_str().unwrap_or("");
        let title = if row["archived"].as_bool() == Some(true) {
            format!("{} (archived)", truncate_single_line(raw_title, 40))
        } else {
            truncate_single_line(raw_title, 50)
        };
        let _ = writeln!(
            out,
            "{:<36}  {:<12}  {:<24}  {:<19}  {}",
            truncate_single_line(id, 36),
            truncate_single_line(status, 12),
            truncate_single_line(&model, 24),
            updated,
            title
        );
    }
    out
}

fn format_model_label(selection: &Value) -> String {
    let provider = selection["provider"].as_str().unwrap_or("");
    let model = selection["model"].as_str().unwrap_or("");
    match (provider.is_empty(), model.is_empty()) {
        (true, true) => "-".into(),
        (true, false) => model.into(),
        (false, true) => provider.into(),
        (false, false) => format!("{provider}/{model}"),
    }
}

fn format_limits_table(value: &Value) -> String {
    let Some(providers) = value["providers"].as_array() else {
        return format!("{value}\n");
    };
    if providers.is_empty() {
        return "No provider limits.\n".into();
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<14}  {:<12}  {:<12}  {:<20}  {:>7}  {:>7}  Resets / Note",
        "Provider", "Plan", "Status", "Window", "Used", "Left"
    );
    for provider in providers {
        let label = provider["label"]
            .as_str()
            .or_else(|| provider["provider"].as_str())
            .unwrap_or("-");
        let plan = provider["plan"].as_str().unwrap_or("-");
        let status = provider["status"].as_str().unwrap_or("-");
        let message = provider["message"].as_str().unwrap_or("");
        let windows = provider["windows"].as_array();
        match windows {
            Some(windows) if !windows.is_empty() => {
                for (index, window) in windows.iter().enumerate() {
                    let window_label = window["label"].as_str().unwrap_or("-");
                    let used = format_percent(window["used_percent"].as_f64());
                    let left = format_percent(window["remaining_percent"].as_f64());
                    let resets = window["resets_at"]
                        .as_str()
                        .map(format_timestamp)
                        .unwrap_or_else(|| "-".into());
                    let note = if index == 0 && !message.is_empty() && resets == "-" {
                        truncate_single_line(message, 40)
                    } else {
                        resets
                    };
                    let _ = writeln!(
                        out,
                        "{:<14}  {:<12}  {:<12}  {:<20}  {:>7}  {:>7}  {}",
                        truncate_single_line(if index == 0 { label } else { "" }, 14),
                        truncate_single_line(if index == 0 { plan } else { "" }, 12),
                        truncate_single_line(if index == 0 { status } else { "" }, 12),
                        truncate_single_line(window_label, 20),
                        used,
                        left,
                        note
                    );
                }
            }
            _ => {
                let note = if message.is_empty() {
                    "-".into()
                } else {
                    truncate_single_line(message, 40)
                };
                let _ = writeln!(
                    out,
                    "{:<14}  {:<12}  {:<12}  {:<20}  {:>7}  {:>7}  {}",
                    truncate_single_line(label, 14),
                    truncate_single_line(plan, 12),
                    truncate_single_line(status, 12),
                    "-",
                    "-",
                    "-",
                    note
                );
            }
        }
    }
    out
}

fn format_percent(value: Option<f64>) -> String {
    match value {
        Some(pct) => format!("{pct:.0}%"),
        None => "-".into(),
    }
}

fn format_timestamp(ts: &str) -> String {
    let trimmed = ts.trim();
    if trimmed.len() >= 19 && trimmed.is_ascii() {
        trimmed[..19].replace('T', " ")
    } else {
        truncate_single_line(trimmed, 19)
    }
}

fn truncate_single_line(input: &str, max_chars: usize) -> String {
    let collapsed = input.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = collapsed.chars().count();
    if count <= max_chars {
        return collapsed;
    }
    if max_chars <= 3 {
        return collapsed.chars().take(max_chars).collect();
    }
    let prefix: String = collapsed.chars().take(max_chars - 3).collect();
    format!("{prefix}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_threads_defaults_and_flags() {
        let cli = Cli::try_parse_from(["t3d", "threads"]).unwrap();
        match cli.command {
            Command::Threads {
                url,
                token,
                archived,
                limit,
                json,
            } => {
                assert_eq!(url, "http://127.0.0.1:3774");
                assert_eq!(token, None);
                assert!(!archived);
                assert_eq!(limit, 30);
                assert!(!json);
            }
            _ => panic!("expected Threads command"),
        }

        let cli = Cli::try_parse_from([
            "t3d",
            "threads",
            "--url",
            "http://localhost:4000",
            "--token",
            "secret-tok",
            "--archived",
            "--limit",
            "12",
            "--json",
        ])
        .unwrap();
        match cli.command {
            Command::Threads {
                url,
                token,
                archived,
                limit,
                json,
            } => {
                assert_eq!(url, "http://localhost:4000");
                assert_eq!(token.as_deref(), Some("secret-tok"));
                assert!(archived);
                assert_eq!(limit, 12);
                assert!(json);
            }
            _ => panic!("expected Threads command"),
        }
    }

    #[test]
    fn parse_send_command() {
        let id = Uuid::new_v4();
        let id_str = id.to_string();
        let cli = Cli::try_parse_from(["t3d", "send", &id_str, "hello world"]).unwrap();
        match cli.command {
            Command::Send {
                thread_id,
                prompt,
                url,
                token,
                behavior,
                json,
            } => {
                assert_eq!(thread_id, id);
                assert_eq!(prompt, "hello world");
                assert_eq!(url, "http://127.0.0.1:3774");
                assert_eq!(token, None);
                assert_eq!(behavior, None);
                assert!(!json);
            }
            _ => panic!("expected Send command"),
        }

        let cli = Cli::try_parse_from([
            "t3d",
            "send",
            &id_str,
            "follow up",
            "--behavior",
            "queue",
            "--token",
            "tok",
            "--json",
        ])
        .unwrap();
        match cli.command {
            Command::Send {
                behavior,
                token,
                json,
                ..
            } => {
                assert_eq!(behavior.as_deref(), Some("queue"));
                assert_eq!(token.as_deref(), Some("tok"));
                assert!(json);
            }
            _ => panic!("expected Send command"),
        }
    }

    #[test]
    fn parse_interrupt_and_limits_commands() {
        let id = Uuid::new_v4();
        let id_str = id.to_string();
        let cli = Cli::try_parse_from([
            "t3d",
            "interrupt",
            &id_str,
            "--url",
            "http://127.0.0.1:3888",
            "--token",
            "tok",
        ])
        .unwrap();
        match cli.command {
            Command::Interrupt {
                thread_id,
                url,
                token,
            } => {
                assert_eq!(thread_id, id);
                assert_eq!(url, "http://127.0.0.1:3888");
                assert_eq!(token.as_deref(), Some("tok"));
            }
            _ => panic!("expected Interrupt command"),
        }

        let cli = Cli::try_parse_from(["t3d", "limits", "--refresh", "--json"]).unwrap();
        match cli.command {
            Command::Limits {
                url,
                token,
                refresh,
                json,
            } => {
                assert_eq!(url, "http://127.0.0.1:3774");
                assert_eq!(token, None);
                assert!(refresh);
                assert!(json);
            }
            _ => panic!("expected Limits command"),
        }
    }

    #[test]
    fn format_tables_handles_empty_and_populated_payloads() {
        assert_eq!(format_threads_table(&json!([])), "No threads.\n");
        let threads_out = format_threads_table(&json!([{
            "id": "00000000-0000-0000-0000-000000000001",
            "status": "running",
            "model_selection": { "provider": "codex", "model": "gpt-5" },
            "updated_at": "2026-11-25T10:20:30Z",
            "title": "Implement CLI\ncontrol",
            "archived": false
        }]));
        assert!(threads_out.contains("00000000-0000-0000-0000-000000000001"));
        assert!(threads_out.contains("codex/gpt-5"));
        assert!(threads_out.contains("2026-11-25 10:20:30"));
        assert!(threads_out.contains("Implement CLI control"));

        assert_eq!(
            format_limits_table(&json!({ "providers": [] })),
            "No provider limits.\n"
        );
        let limits_out = format_limits_table(&json!({
            "providers": [
                {
                    "provider": "claude",
                    "label": "Claude",
                    "plan": "Max",
                    "status": "ok",
                    "windows": [
                        {
                            "label": "Session (5h)",
                            "used_percent": 25.0,
                            "remaining_percent": 75.0,
                            "resets_at": "2026-11-25T15:00:00Z"
                        }
                    ]
                },
                {
                    "provider": "codex",
                    "label": "Codex",
                    "plan": null,
                    "status": "unavailable",
                    "message": "not signed in",
                    "windows": []
                }
            ]
        }));
        assert!(limits_out.contains("Claude"));
        assert!(limits_out.contains("25%"));
        assert!(limits_out.contains("75%"));
        assert!(limits_out.contains("not signed in"));
    }

    #[test]
    fn mcp_command_configures_env_fallbacks_and_parses_flags() {
        use clap::CommandFactory;

        let cmd = Cli::command();
        let mcp = cmd
            .find_subcommand("mcp")
            .expect("mcp subcommand should exist");
        let url_arg = mcp
            .get_arguments()
            .find(|arg| arg.get_id() == "url")
            .expect("url arg");
        assert_eq!(
            url_arg.get_env().and_then(|v| v.to_str()),
            Some("T3_SERVER_URL")
        );
        let thread_arg = mcp
            .get_arguments()
            .find(|arg| arg.get_id() == "thread_id")
            .expect("thread_id arg");
        assert_eq!(
            thread_arg.get_env().and_then(|v| v.to_str()),
            Some("T3_THREAD_ID")
        );

        let tid = Uuid::new_v4();
        let tid_str = tid.to_string();
        let cli = Cli::try_parse_from([
            "t3d",
            "mcp",
            "--url",
            "http://127.0.0.1:4100",
            "--token",
            "tok",
            "--thread-id",
            &tid_str,
        ])
        .unwrap();
        match cli.command {
            Command::Mcp {
                url,
                token,
                thread_id,
            } => {
                assert_eq!(url, "http://127.0.0.1:4100");
                assert_eq!(token, "tok");
                assert_eq!(thread_id, Some(tid));
            }
            _ => panic!("expected Mcp command"),
        }
    }

    #[test]
    fn setup_agy_mcp_parses_and_builds_zero_secret_commands() {
        let cli = Cli::try_parse_from(["t3d", "setup-agy-mcp", "--dry-run"]).unwrap();
        match cli.command {
            Command::SetupAgyMcp {
                url,
                remove,
                dry_run,
            } => {
                assert_eq!(url, None);
                assert!(!remove);
                assert!(dry_run);
            }
            _ => panic!("expected SetupAgyMcp command"),
        }

        let cli = Cli::try_parse_from([
            "t3d",
            "setup-agy-mcp",
            "--url",
            "http://127.0.0.1:4200",
            "--remove",
        ])
        .unwrap();
        match cli.command {
            Command::SetupAgyMcp {
                url,
                remove,
                dry_run,
            } => {
                assert_eq!(url.as_deref(), Some("http://127.0.0.1:4200"));
                assert!(remove);
                assert!(!dry_run);
            }
            _ => panic!("expected SetupAgyMcp command"),
        }

        let add_args = setup_agy_mcp_args(Path::new("/usr/local/bin/t3d"), None, false);
        assert_eq!(
            format_agy_mcp_command(&add_args),
            "agy mcp add t3rs /usr/local/bin/t3d -- mcp"
        );
        assert!(add_args.iter().all(|arg| !arg.contains("token")));

        let custom_args = setup_agy_mcp_args(
            Path::new("/usr/local/bin/t3d"),
            Some("http://127.0.0.1:3999"),
            false,
        );
        assert_eq!(
            format_agy_mcp_command(&custom_args),
            "agy mcp add t3rs /usr/local/bin/t3d -- mcp --url http://127.0.0.1:3999"
        );

        let remove_args = setup_agy_mcp_args(Path::new("/usr/local/bin/t3d"), None, true);
        assert_eq!(format_agy_mcp_command(&remove_args), "agy mcp remove t3rs");
    }
}
