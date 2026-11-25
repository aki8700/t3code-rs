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
    AuthConfig, DbStats, DiffSummary, ModelSelection, OrchModelStat, ProviderId, ServerConfig,
    Store,
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
    /// Show changed files and optional unified patches for a thread workspace.
    Diff {
        thread_id: Uuid,
        #[arg(long, default_value = DEFAULT_URL)]
        url: String,
        #[arg(long, env = "T3_BEARER_TOKEN", hide_env_values = true)]
        token: Option<String>,
        /// Include whitespace-only changes in the diff.
        #[arg(long)]
        no_ignore_whitespace: bool,
        /// Print unified patch hunks below the changed files table.
        #[arg(long)]
        patch: bool,
        #[arg(long)]
        json: bool,
    },
    /// Merge a child worktree thread into its parent thread.
    Merge {
        parent_id: Uuid,
        child_id: Uuid,
        #[arg(long, default_value = DEFAULT_URL)]
        url: String,
        #[arg(long, env = "T3_BEARER_TOKEN", hide_env_values = true)]
        token: Option<String>,
        /// Optional commit message for the merge.
        #[arg(long)]
        message: Option<String>,
        /// Retain the child worktree and branch after merging.
        #[arg(long)]
        no_cleanup: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show active runs, pending approval requests, queued threads, and active shells.
    Monitor {
        #[arg(long, default_value = DEFAULT_URL)]
        url: String,
        #[arg(long, env = "T3_BEARER_TOKEN", hide_env_values = true)]
        token: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Export a thread transcript as Markdown (or raw JSON with --json).
    Export {
        thread_id: Uuid,
        #[arg(long, default_value = DEFAULT_URL)]
        url: String,
        #[arg(long, env = "T3_BEARER_TOKEN", hide_env_values = true)]
        token: Option<String>,
        #[arg(long, default_value_t = 500)]
        limit: usize,
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
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
        Command::Diff {
            thread_id,
            url,
            token,
            no_ignore_whitespace,
            patch,
            json,
        } => {
            let diff =
                client::get_thread_diff(&url, token.as_deref(), thread_id, !no_ignore_whitespace)
                    .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&diff)?);
            } else {
                print!("{}", format_diff_output(&diff, patch));
            }
        }
        Command::Merge {
            parent_id,
            child_id,
            url,
            token,
            message,
            no_cleanup,
            json,
        } => {
            let result = client::merge_child_worktree(
                &url,
                token.as_deref(),
                parent_id,
                child_id,
                message.as_deref(),
                !no_cleanup,
            )
            .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                print!("{}", format_merge_result(&result));
            }
            if merge_has_conflict(&result) {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Monitor { url, token, json } => {
            let snapshot = client::get_monitor(&url, token.as_deref()).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot)?);
            } else {
                print!("{}", format_monitor_table(&snapshot));
            }
        }
        Command::Export {
            thread_id,
            url,
            token,
            limit,
            output,
            json,
        } => {
            let rendered = if json {
                let detail =
                    client::get_thread_detail(&url, token.as_deref(), thread_id, limit).await?;
                format!("{}\n", serde_json::to_string_pretty(&detail)?)
            } else {
                client::export_thread_markdown(&url, token.as_deref(), thread_id).await?
            };
            if let Some(path) = output {
                write_private_file(&path, rendered.as_bytes())?;
                println!("Exported thread {thread_id} to {}", path.display());
            } else {
                print!("{rendered}");
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

fn format_diff_output(diff: &DiffSummary, include_patch: bool) -> String {
    let mut out = String::new();
    let branch = diff.branch.as_deref().unwrap_or("-");
    if diff.clean || diff.files.is_empty() {
        let _ = writeln!(out, "Workspace clean (branch: {branch}).");
        return out;
    }
    let _ = writeln!(
        out,
        "Branch: {}  Files: {}  +{} -{}",
        branch,
        diff.files.len(),
        diff.total_additions,
        diff.total_deletions
    );
    let _ = writeln!(out, "{:<10}  {:>6}  {:>6}  Path", "Status", "+Add", "-Del");
    for file in &diff.files {
        let adds = format!("+{}", file.additions);
        let dels = format!("-{}", file.deletions);
        let _ = writeln!(
            out,
            "{:<10}  {:>6}  {:>6}  {}",
            truncate_single_line(&file.status, 10),
            adds,
            dels,
            file.path
        );
    }
    if include_patch {
        for file in &diff.files {
            if file.patch.is_empty() {
                continue;
            }
            let _ = writeln!(out, "\n--- {} ({}) ---", file.path, file.status);
            out.push_str(&file.patch);
            if !file.patch.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    out
}

fn merge_has_conflict(value: &Value) -> bool {
    value["merged"].as_bool() != Some(true)
        || value["conflict_files"]
            .as_array()
            .is_some_and(|files| !files.is_empty())
}

fn format_merge_result(value: &Value) -> String {
    let mut out = String::new();
    let merged = value["merged"].as_bool().unwrap_or(false);
    let branch = value["branch"].as_str().unwrap_or("-");
    let child_id = value["child_thread_id"].as_str().unwrap_or("-");
    let summary = value["summary"].as_str().unwrap_or("");
    let cleaned_up = value["cleaned_up"].as_bool().unwrap_or(false);
    let conflicts: Vec<&str> = value["conflict_files"]
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    if merged && conflicts.is_empty() {
        let _ = writeln!(out, "Merged branch {branch} (child {child_id})");
        if !summary.is_empty() {
            let _ = writeln!(out, "Summary: {summary}");
        }
        let _ = writeln!(
            out,
            "Cleanup: {}",
            if cleaned_up { "cleaned up" } else { "retained" }
        );
    } else {
        let _ = writeln!(out, "Merge conflict on branch {branch} (child {child_id})");
        if !summary.is_empty() {
            let _ = writeln!(out, "Summary: {summary}");
        }
        if !conflicts.is_empty() {
            let _ = writeln!(out, "Conflict files:");
            for path in conflicts {
                let _ = writeln!(out, "  - {path}");
            }
        }
    }
    out
}

fn format_monitor_table(value: &Value) -> String {
    let active_runs = value["active_runs"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let pending_requests = value["pending_requests"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let queued_threads = value["queued_thread_ids"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let active_shells = value["active_shells"].as_u64().unwrap_or(0);

    let mut out = String::new();
    let _ = writeln!(
        out,
        "Active runs: {}  Pending approvals: {}  Queued threads: {}  Active shells: {}",
        active_runs.len(),
        pending_requests.len(),
        queued_threads.len(),
        active_shells
    );

    if !active_runs.is_empty() {
        let _ = writeln!(out, "\nActive runs:");
        let _ = writeln!(
            out,
            "{:<36}  {:<36}  {:<12}  {:<24}  {:>7}  Title",
            "Thread", "Run", "Status", "Model", "Pending"
        );
        for run in active_runs {
            let thread_id = run["thread_id"].as_str().unwrap_or("-");
            let run_id = run["run_id"].as_str().unwrap_or("-");
            let status = run["status"].as_str().unwrap_or("-");
            let model = format_model_label(&run["model_selection"]);
            let pending = run["pending_count"].as_u64().unwrap_or(0);
            let title = truncate_single_line(run["title"].as_str().unwrap_or(""), 40);
            let _ = writeln!(
                out,
                "{:<36}  {:<36}  {:<12}  {:<24}  {:>7}  {}",
                truncate_single_line(thread_id, 36),
                truncate_single_line(run_id, 36),
                truncate_single_line(status, 12),
                truncate_single_line(&model, 24),
                pending,
                title
            );
        }
    }

    if !pending_requests.is_empty() {
        let _ = writeln!(out, "\nPending approvals:");
        let _ = writeln!(
            out,
            "{:<36}  {:<36}  {:<16}  {:<19}  Title",
            "Request", "Thread", "Kind", "Created"
        );
        for req in pending_requests {
            let id = req["id"].as_str().unwrap_or("-");
            let thread_id = req["thread_id"].as_str().unwrap_or("-");
            let kind = req["kind"].as_str().unwrap_or("-");
            let created = format_timestamp(req["created_at"].as_str().unwrap_or("-"));
            let title = truncate_single_line(req["title"].as_str().unwrap_or(""), 44);
            let _ = writeln!(
                out,
                "{:<36}  {:<36}  {:<16}  {:<19}  {}",
                truncate_single_line(id, 36),
                truncate_single_line(thread_id, 36),
                truncate_single_line(kind, 16),
                created,
                title
            );
        }
    }

    if !queued_threads.is_empty() {
        let _ = writeln!(out, "\nQueued threads:");
        for thread_id in queued_threads {
            let label = thread_id.as_str().unwrap_or("-");
            let _ = writeln!(out, "  - {label}");
        }
    }

    out
}

fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents)?;
    file.flush()
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

    #[test]
    fn parse_tier5_commands_defaults_and_flags() {
        let parent_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let parent_str = parent_id.to_string();
        let child_str = child_id.to_string();

        // diff defaults
        let cli = Cli::try_parse_from(["t3d", "diff", &parent_str]).unwrap();
        match cli.command {
            Command::Diff {
                thread_id,
                url,
                token,
                no_ignore_whitespace,
                patch,
                json,
            } => {
                assert_eq!(thread_id, parent_id);
                assert_eq!(url, DEFAULT_URL);
                assert_eq!(token, None);
                assert!(!no_ignore_whitespace);
                assert!(!patch);
                assert!(!json);
            }
            _ => panic!("expected Diff command"),
        }

        // diff with flags
        let cli = Cli::try_parse_from([
            "t3d",
            "diff",
            &parent_str,
            "--url",
            "http://localhost:4000",
            "--token",
            "tok-diff",
            "--no-ignore-whitespace",
            "--patch",
            "--json",
        ])
        .unwrap();
        match cli.command {
            Command::Diff {
                url,
                token,
                no_ignore_whitespace,
                patch,
                json,
                ..
            } => {
                assert_eq!(url, "http://localhost:4000");
                assert_eq!(token.as_deref(), Some("tok-diff"));
                assert!(no_ignore_whitespace);
                assert!(patch);
                assert!(json);
            }
            _ => panic!("expected Diff command"),
        }

        // merge defaults & flags
        let cli = Cli::try_parse_from(["t3d", "merge", &parent_str, &child_str]).unwrap();
        match cli.command {
            Command::Merge {
                parent_id: p,
                child_id: c,
                url,
                token,
                message,
                no_cleanup,
                json,
            } => {
                assert_eq!(p, parent_id);
                assert_eq!(c, child_id);
                assert_eq!(url, DEFAULT_URL);
                assert_eq!(token, None);
                assert_eq!(message, None);
                assert!(!no_cleanup);
                assert!(!json);
            }
            _ => panic!("expected Merge command"),
        }

        let cli = Cli::try_parse_from([
            "t3d",
            "merge",
            &parent_str,
            &child_str,
            "--message",
            "Merge child task",
            "--no-cleanup",
            "--token",
            "tok-merge",
            "--json",
        ])
        .unwrap();
        match cli.command {
            Command::Merge {
                message,
                no_cleanup,
                token,
                json,
                ..
            } => {
                assert_eq!(message.as_deref(), Some("Merge child task"));
                assert!(no_cleanup);
                assert_eq!(token.as_deref(), Some("tok-merge"));
                assert!(json);
            }
            _ => panic!("expected Merge command"),
        }

        // monitor defaults & flags
        let cli = Cli::try_parse_from(["t3d", "monitor"]).unwrap();
        match cli.command {
            Command::Monitor { url, token, json } => {
                assert_eq!(url, DEFAULT_URL);
                assert_eq!(token, None);
                assert!(!json);
            }
            _ => panic!("expected Monitor command"),
        }
        let cli = Cli::try_parse_from([
            "t3d",
            "monitor",
            "--url",
            "http://127.0.0.1:4555",
            "--token",
            "tok-mon",
            "--json",
        ])
        .unwrap();
        match cli.command {
            Command::Monitor { url, token, json } => {
                assert_eq!(url, "http://127.0.0.1:4555");
                assert_eq!(token.as_deref(), Some("tok-mon"));
                assert!(json);
            }
            _ => panic!("expected Monitor command"),
        }

        // export defaults & flags
        let cli = Cli::try_parse_from(["t3d", "export", &parent_str]).unwrap();
        match cli.command {
            Command::Export {
                thread_id,
                url,
                token,
                limit,
                output,
                json,
            } => {
                assert_eq!(thread_id, parent_id);
                assert_eq!(url, DEFAULT_URL);
                assert_eq!(token, None);
                assert_eq!(limit, 500);
                assert_eq!(output, None);
                assert!(!json);
            }
            _ => panic!("expected Export command"),
        }
        let cli = Cli::try_parse_from([
            "t3d",
            "export",
            &parent_str,
            "--limit",
            "150",
            "--output",
            "/tmp/transcript.md",
            "--token",
            "tok-exp",
            "--json",
        ])
        .unwrap();
        match cli.command {
            Command::Export {
                limit,
                output,
                token,
                json,
                ..
            } => {
                assert_eq!(limit, 150);
                assert_eq!(output, Some(PathBuf::from("/tmp/transcript.md")));
                assert_eq!(token.as_deref(), Some("tok-exp"));
                assert!(json);
            }
            _ => panic!("expected Export command"),
        }
    }

    #[test]
    fn format_diff_table_patch_and_json_output() {
        use t3_core::DiffFile;

        let clean = DiffSummary {
            workspace_path: PathBuf::from("/repo"),
            branch: Some("main".into()),
            clean: true,
            total_additions: 0,
            total_deletions: 0,
            files: vec![],
        };
        assert_eq!(
            format_diff_output(&clean, false),
            "Workspace clean (branch: main).\n"
        );

        let dirty = DiffSummary {
            workspace_path: PathBuf::from("/repo"),
            branch: Some("feat/cli".into()),
            clean: false,
            total_additions: 12,
            total_deletions: 3,
            files: vec![
                DiffFile {
                    path: "crates/cli/src/main.rs".into(),
                    status: "modified".into(),
                    additions: 10,
                    deletions: 3,
                    patch: "@@ -1,3 +1,10 @@\n-old\n+new\n".into(),
                },
                DiffFile {
                    path: "README.md".into(),
                    status: "added".into(),
                    additions: 2,
                    deletions: 0,
                    patch: String::new(),
                },
            ],
        };

        let table_only = format_diff_output(&dirty, false);
        assert!(table_only.contains("Branch: feat/cli  Files: 2  +12 -3"));
        assert!(table_only.contains("modified"));
        assert!(table_only.contains("+10"));
        assert!(table_only.contains("-3"));
        assert!(table_only.contains("crates/cli/src/main.rs"));
        assert!(table_only.contains("README.md"));
        assert!(!table_only.contains("@@ -1,3 +1,10 @@"));

        let with_patch = format_diff_output(&dirty, true);
        assert!(with_patch.contains("--- crates/cli/src/main.rs (modified) ---"));
        assert!(with_patch.contains("@@ -1,3 +1,10 @@"));
        assert!(!with_patch.contains("--- README.md"));

        let json_str = serde_json::to_string_pretty(&dirty).unwrap();
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["branch"], "feat/cli");
        assert_eq!(parsed["total_additions"], 12);
        assert_eq!(parsed["files"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn format_merge_result_handles_success_and_conflicts() {
        let ok_payload = json!({
            "merged": true,
            "child_thread_id": "00000000-0000-0000-0000-000000000002",
            "branch": "t3rs/child-1",
            "summary": "Merged feature (abc1234)",
            "conflict_files": [],
            "cleaned_up": true
        });
        assert!(!merge_has_conflict(&ok_payload));
        let ok_text = format_merge_result(&ok_payload);
        assert!(ok_text.contains("Merged branch t3rs/child-1"));
        assert!(ok_text.contains("Summary: Merged feature (abc1234)"));
        assert!(ok_text.contains("Cleanup: cleaned up"));

        let conflict_payload = json!({
            "merged": false,
            "child_thread_id": "00000000-0000-0000-0000-000000000002",
            "branch": "t3rs/child-2",
            "summary": "merge conflicted in 2 files",
            "conflict_files": ["src/lib.rs", "Cargo.toml"],
            "cleaned_up": false
        });
        assert!(merge_has_conflict(&conflict_payload));
        let conflict_text = format_merge_result(&conflict_payload);
        assert!(conflict_text.contains("Merge conflict on branch t3rs/child-2"));
        assert!(conflict_text.contains("Conflict files:"));
        assert!(conflict_text.contains("- src/lib.rs"));
        assert!(conflict_text.contains("- Cargo.toml"));

        let conflict_json = serde_json::to_string_pretty(&conflict_payload).unwrap();
        let parsed: Value = serde_json::from_str(&conflict_json).unwrap();
        assert_eq!(parsed["merged"], false);
        assert_eq!(parsed["conflict_files"][0], "src/lib.rs");
    }

    #[test]
    fn format_monitor_table_handles_idle_and_populated_snapshots() {
        let idle = json!({
            "active_runs": [],
            "pending_requests": [],
            "queued_thread_ids": [],
            "active_shells": 0
        });
        assert_eq!(
            format_monitor_table(&idle),
            "Active runs: 0  Pending approvals: 0  Queued threads: 0  Active shells: 0\n"
        );

        let busy = json!({
            "active_runs": [{
                "thread_id": "00000000-0000-0000-0000-000000000001",
                "project_id": "00000000-0000-0000-0000-000000000009",
                "run_id": "00000000-0000-0000-0000-000000000010",
                "title": "Refactor CLI commands",
                "model_selection": { "provider": "claude", "model": "claude-sonnet-4-5" },
                "status": "running",
                "pending_count": 1
            }],
            "pending_requests": [{
                "id": "00000000-0000-0000-0000-000000000020",
                "thread_id": "00000000-0000-0000-0000-000000000001",
                "run_id": "00000000-0000-0000-0000-000000000010",
                "kind": "command_approval",
                "title": "Run cargo test",
                "payload": {},
                "created_at": "2026-11-25T10:30:00Z"
            }],
            "queued_thread_ids": [
                "00000000-0000-0000-0000-000000000030"
            ],
            "active_shells": 2
        });
        let out = format_monitor_table(&busy);
        assert!(
            out.contains(
                "Active runs: 1  Pending approvals: 1  Queued threads: 1  Active shells: 2"
            )
        );
        assert!(out.contains("claude/claude-sonnet-4-5"));
        assert!(out.contains("Refactor CLI commands"));
        assert!(out.contains("command_approval"));
        assert!(out.contains("Run cargo test"));
        assert!(out.contains("2026-11-25 10:30:00"));
        assert!(out.contains("00000000-0000-0000-0000-000000000030"));

        let json_str = serde_json::to_string_pretty(&busy).unwrap();
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["active_shells"], 2);
        assert_eq!(parsed["queued_thread_ids"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn export_write_private_file_enforces_0600_and_truncates() {
        let dir = std::env::temp_dir().join(format!("t3-cli-export-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("transcript.md");

        write_private_file(&path, b"# Redacted Export\n\nSecret: [REDACTED]\n").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# Redacted Export\n\nSecret: [REDACTED]\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);

            // Simulate a pre-existing file with looser permissions and verify tightening + truncation.
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }

        write_private_file(&path, b"# Shorter\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "# Shorter\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_export_markdown_calls_server_export_endpoint_and_writes_private_file() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let thread_id = Uuid::from_u128(7);
        let expected_path = format!("/api/threads/{thread_id}/export");
        let redacted_md = "# Server Export\n\n- Key: [REDACTED]\n";

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/markdown; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                redacted_md.len(),
                redacted_md
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
            req
        });

        let dir = std::env::temp_dir().join(format!("t3-cli-run-export-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let out_path = dir.join("out.md");

        let cli = Cli::try_parse_from([
            "t3d",
            "export",
            &thread_id.to_string(),
            "--url",
            &format!("http://{addr}"),
            "--token",
            "export-tok",
            "-o",
            out_path.to_str().unwrap(),
        ])
        .unwrap();

        let code = run(cli).await.unwrap();
        let req = server.await.unwrap();

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(req.starts_with(&format!("GET {expected_path} HTTP/1.1\r\n")));
        assert_eq!(fs::read_to_string(&out_path).unwrap(), redacted_md);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&out_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_export_json_calls_thread_detail_endpoint_and_writes_private_file() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let thread_id = Uuid::from_u128(8);
        let expected_path = format!("/api/threads/{thread_id}?limit=25");
        let detail_body = r#"{"thread":{"id":"00000000-0000-0000-0000-000000000008","title":"JSON Test"},"items":[]}"#;

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                detail_body.len(),
                detail_body
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
            req
        });

        let dir = std::env::temp_dir().join(format!("t3-cli-run-export-json-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let out_path = dir.join("out.json");

        let cli = Cli::try_parse_from([
            "t3d",
            "export",
            &thread_id.to_string(),
            "--url",
            &format!("http://{addr}"),
            "--limit",
            "25",
            "--json",
            "-o",
            out_path.to_str().unwrap(),
        ])
        .unwrap();

        let code = run(cli).await.unwrap();
        let req = server.await.unwrap();

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(req.starts_with(&format!("GET {expected_path} HTTP/1.1\r\n")));
        let written: Value = serde_json::from_str(&fs::read_to_string(&out_path).unwrap()).unwrap();
        assert_eq!(written["thread"]["title"], "JSON Test");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&out_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let _ = fs::remove_dir_all(&dir);
    }
}
