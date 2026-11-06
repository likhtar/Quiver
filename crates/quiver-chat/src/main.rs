//! quiver-chat — OBS-Twitch chat widget engine.
//!
//! Headless tool: reads a RON config, joins a Twitch channel anonymously,
//! serves the widget frontend + WebSocket feed for the OBS browser source.

use std::path::PathBuf;

use clap::Parser;
use quiver_chat::config::ChatConfig;
use quiver_config::{Validate, write_schema};

#[derive(Debug, Parser)]
#[command(name = "quiver-chat", about = "OBS-Twitch chat widget engine")]
struct Args {
    /// Path to the RON config file.
    #[arg(long, default_value = "chat.ron")]
    config: PathBuf,

    /// Print a fully commented sample config to stdout and exit.
    #[arg(long)]
    sample_config: bool,

    /// Print the JSON Schema of the config model to stdout and exit.
    /// This is the machine-readable contract the Quiver UI consumes.
    #[arg(long)]
    print_schema: bool,

    /// Run the Twitch Device Code Flow to authorize channel-scoped calls
    /// (redeem names, redemptions, EventSub). Prints a code + URL, waits for
    /// approval, stores tokens in the XDG OAuth store, and exits.
    #[arg(long)]
    auth: bool,

    /// Print the stored OAuth state as a JSON snippet for the Quiver UI
    /// ({client_id, channel_login, scopes, token_path}) and exit.
    #[arg(long)]
    auth_print: bool,

    /// Comma-separated scope override for --auth (default: the full set).
    #[arg(long)]
    scopes: Option<String>,
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    init_tracing();

    if args.sample_config {
        let generated =
            quiver_config::generate_default::<ChatConfig>("quiver-chat --sample-config")
                .map_err(|e| anyhow::anyhow!("generation failed: {e}"))?;
        print!("{generated}");
        return Ok(());
    }

    if args.print_schema {
        let mut stdout = std::io::stdout().lock();
        write_schema::<ChatConfig>(&mut stdout)?;
        return Ok(());
    }

    let cfg: ChatConfig = quiver_chat::config::load(&args.config)?;

    if args.auth {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(run_auth_flow(&cfg, args.scopes.as_deref()));
        return Ok(());
    }
    if args.auth_print {
        print_auth_state(&cfg);
        return Ok(());
    }

    let issues = cfg.validate();
    if !issues.is_empty() {
        eprintln!("config {} is invalid:", args.config.display());
        for issue in &issues {
            eprintln!("  - {issue}");
        }
        std::process::exit(2);
    }

    tracing::info!(config = %args.config.display(), "configuration loaded and validated");

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(quiver_chat::serve::run(cfg, args.config))
}

/// Open the verification URL in the user's default browser, OS-agnostic.
/// Best-effort: the URL stays printed for manual fallback on headless boxes.
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    {
        // `open <url>` — macOS default-handler launch.
        match std::process::Command::new("open").arg(url).spawn() {
            Ok(_) => return,
            Err(e) => eprintln!("note: could not open browser automatically ({e})"),
        }
    }
    #[cfg(target_os = "windows")]
    {
        // `start` treats the first quoted argument as the window title, so
        // an empty title precedes the URL.
        match std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
        {
            Ok(_) => return,
            Err(e) => eprintln!("note: could not open browser automatically ({e})"),
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // xdg-open covers desktop Linux/BSD; headless boxes will fail here
        // and the printed URL is the fallback.
        match std::process::Command::new("xdg-open").arg(url).spawn() {
            Ok(_) => return,
            Err(e) => eprintln!("note: could not open browser automatically ({e})"),
        }
    }
}

/// Twitch Device Code Flow: prints a code, waits for approval, persists the
/// token pair to the XDG OAuth store, resolves the authed channel, and
/// prints a short summary. Scope list overridable via `--scopes a,b,c`.
async fn run_auth_flow(cfg: &ChatConfig, scopes_override: Option<&str>) {
    let Some(client_id) = &cfg.twitch.client_id else {
        eprintln!("--auth requires twitch.client_id (and client_secret) in the config");
        std::process::exit(2);
    };
    let client_secret = cfg.twitch.client_secret.as_deref().unwrap_or_default();

    let scopes: Vec<&str> = match scopes_override {
        Some(raw) => raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect(),
        None => quiver_twitch::DEFAULT_SCOPES.to_vec(),
    };

    let http = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("http client unavailable: {e}");
            std::process::exit(2);
        }
    };
    let secret: Option<&str> = (!client_secret.is_empty()).then_some(client_secret);

    let flow = quiver_twitch::auth::DeviceFlow::new(&http, client_id, secret);
    let challenge = match flow.start(&scopes).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("could not start device flow: {e}");
            std::process::exit(2);
        }
    };

    println!("1. Open:  {}", challenge.verification_uri);
    println!("2. Enter: {}", challenge.user_code);
    open_browser(&challenge.verification_uri);
    println!(
        "Waiting for approval (auto-polling every {}s, code valid {}s)...",
        challenge.interval, challenge.expires_in
    );

    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(challenge.expires_in);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_secs(challenge.interval.max(2))).await;
        match flow.poll(&challenge.device_code).await {
            Ok(quiver_twitch::auth::TokenPoll::Pending) => continue,
            Ok(quiver_twitch::auth::TokenPoll::Granted(mut tokens)) => {
                // Resolve the authed user's login so we can warn about
                // channel ownership and label the store.
                let helix = quiver_twitch::HelixClient::new(client_id, client_secret)
                    .expect("helix client builds");
                helix.set_channel_tokens(tokens.clone());
                match helix.me().await {
                    Ok((_id, login)) => tokens.channel_login = Some(login.clone()),
                    Err(e) => eprintln!("note: authorized, but channel lookup failed: {e}"),
                }
                quiver_twitch::auth::persist_tokens(&tokens).ok();
                println!(
                    "Authorized: {}",
                    tokens.channel_login.as_deref().unwrap_or("<unknown>")
                );
                println!("Granted scopes: {}", tokens.scopes.join(" "));
                println!(
                    "Stored at: {}",
                    quiver_twitch::ChannelToken::store_path().display()
                );
                if let Some(login) = &tokens.channel_login
                    && !login.eq_ignore_ascii_case(&cfg.twitch.channel)
                {
                    eprintln!(
                        "WARNING: this token belongs to {login:?}, not the configured channel \
                         {:?}. Redemptions/HypeTrain/Predictions require the token user to be \
                         the broadcaster or a moderator of the channel.",
                        cfg.twitch.channel
                    );
                }
                return;
            }
            Err(e) => {
                eprintln!("device flow error: {e}");
                std::process::exit(2);
            }
        }
    }
    eprintln!("device code expired before approval — retry `--auth`");
    std::process::exit(2);
}

/// Print the stored OAuth state as a JSON snippet for the Quiver UI.
fn print_auth_state(cfg: &ChatConfig) {
    let Some(client_id) = &cfg.twitch.client_id else {
        eprintln!("--auth-print requires twitch.client_id in the config");
        std::process::exit(2);
    };
    let client_secret = cfg.twitch.client_secret.as_deref().unwrap_or_default();
    let http = reqwest::Client::new();
    let Some(token) = quiver_twitch::ChannelToken::load(http, client_id, client_secret) else {
        eprintln!(
            "no OAuth store for client_id {client_id:?} at {} — run `quiver-chat --auth` first",
            quiver_twitch::ChannelToken::store_path().display()
        );
        std::process::exit(1);
    };
    let out = serde_json::json!({
        "client_id": client_id,
        "channel_login": token.channel_login(),
        "scopes": token.scopes(),
        "token_path": quiver_twitch::ChannelToken::store_path(),
    });
    println!("{out}");
}
