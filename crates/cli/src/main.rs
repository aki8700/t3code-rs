use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::Value;
use std::{
    error::Error,
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
        #[arg(long, default_value = DEFAULT_URL)]
        url: String,
        #[arg(long, env = "T3_MCP_TOKEN", hide_env_values = true)]
        token: String,
        #[arg(long)]
        thread_id: Option<Uuid>,
    },
    /// Query the server's health endpoint.
    Status {
        #[arg(long, default_value = DEFAULT_URL)]
        url: String,
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
        Command::Status { url } => {
            let health = client::health(&url).await?;
            println!("{}", serde_json::to_string_pretty(&health)?);
            if health["ok"] != Value::Bool(true) {
                return Ok(ExitCode::FAILURE);
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
        }
        Command::ImportProjects { from, data_dir } => {
            let from = from.unwrap_or_else(|| home().join(".t3/userdata/statev2.sqlite"));
            let cfg = local_config(data_dir);
            if cfg.db_path().exists() && from.canonicalize()? == cfg.db_path().canonicalize()? {
                return Err("source and destination databases must differ".into());
            }
            let count = local_store(Some(cfg.data_dir))?
                .import_projects_from_t3_sqlite(&from)
                .await?;
            println!("Imported {count} project(s).");
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
