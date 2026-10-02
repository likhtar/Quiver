//! Widget host: HTTP server for the widget frontend plus a WebSocket feed.
//!
//! Hot-reload aware: everything here reads through [`LiveConfig`] so a
//! config reload changes behavior without restarting the process. The
//! listener itself rebinds when `server.listen` changes; WebSockets are
//! freed via a generation token and pages reconnect automatically.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::sync::{Notify, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::ChatConfig;
use crate::engine;
use crate::live::{LiveConfig, SharedLive};

const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A feed that lived this long counts as healthy; reset backoff after it.
const HEALTHY_FEED_RUNTIME: Duration = Duration::from_secs(300);
/// Deadline for every upstream badge/CSS fetch — reqwest's default has NO
/// timeout, and a hung URL would stall server boot (both fetches are
/// awaited before bind) or freeze a hot reload forever.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(10);

/// HTTP client for badge/CSS upstream fetches, with a request deadline.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(UPSTREAM_TIMEOUT)
        .build()
        .unwrap_or_else(|e| {
            warn!(error = %e, "http client build failed — falling back to default");
            reqwest::Client::new()
        })
}

pub(crate) type SharedBadges = Arc<RwLock<HashMap<String, String>>>;

#[derive(Clone)]
pub(crate) struct AppState {
    pub live: SharedLive,
    pub messages: engine::SharedState,
    pub tx: broadcast::Sender<String>,
    pub badges: SharedBadges,
    pub custom_badges: crate::badges::SharedBadgeCache,
    /// Resolved custom CSS text for the widget (inline or from URI source).
    pub custom_css: SharedCss,
    pub emotes: crate::emotes::SharedEmotes,
    /// Generation token: cancelled on rebind/shutdown so WS handlers
    /// return instead of blocking graceful drain forever.
    pub ws_token: CancellationToken,
}

pub(crate) struct FeedHandle {
    pub swap_tx: mpsc::UnboundedSender<String>,
}

const PLACEHOLDER_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>quiver-chat</title></head>
<body style="font-family: sans-serif; background:#18181b; color:#efeff1; padding:2em">
<h1>quiver-chat is running</h1>
<p>No widget frontend is being served. Point <code>server.widget_dist</code>
in your RON config at the widget directory:</p>
<pre>crates/quiver-chat/widget</pre>
<p>and reload the page. The WebSocket feed is live at <code>/ws</code> regardless.</p>
</body></html>
"#;

/// Run the chat tool until Ctrl-C. Returns when shut down.
pub async fn run(cfg: ChatConfig, config_path: PathBuf) -> anyhow::Result<()> {
    let live: SharedLive = Arc::new(RwLock::new(LiveConfig::from(&cfg)));
    let (tx, _rx) = broadcast::channel::<String>(256);
    let messages = Arc::new(Mutex::new(engine::EngineState::new(
        cfg.theme.max_messages as usize,
    )));

    // Lint the RESOLVED css (inline text or the file/uri content).
let css_cache_dir = cfg
        .badges
        .as_ref()
        .and_then(|b| b.cache_dir.clone())
        .unwrap_or_else(crate::badges::default_cache_dir);
    let custom_css: SharedCss = Arc::new(RwLock::new(
        resolve_custom_css(&cfg.theme.custom_css, &http_client(), &css_cache_dir).await,
    ));
    let role_css = &cfg.theme.role_css;
    crate::config::report_css_lint(
        custom_css.read().ok().and_then(|c| c.clone()).as_deref(),
        role_css.as_ref(),
    );

    // One fetch at boot. A failure here is NOT fatal: `spawn_badge_supervisor`
    // below keeps retrying while the map is empty, so booting without network
    // self-heals instead of leaving the widget badge-less until a restart.
    let creds = cfg
        .twitch
        .client_id
        .clone()
        .zip(cfg.twitch.client_secret.clone());
    let initial_badges = match &creds {
        Some((id, secret)) => match load_badge_map(id, secret, &cfg.twitch.channel).await {
            Ok(map) => map,
            Err(e) => {
                warn!(error = %e, "badge lookup failed at startup — retrying in background");
                HashMap::new()
            }
        },
        None => {
            info!("no twitch api credentials configured — badges disabled");
            HashMap::new()
        }
    };
    info!(badge_count = initial_badges.len(), "badge map ready");
    let badges: SharedBadges = Arc::new(RwLock::new(initial_badges));

    let initial_emotes = crate::emotes::load_third_party_emotes(
        cfg.twitch
            .client_id
            .as_deref()
            .zip(cfg.twitch.client_secret.as_deref()),
        &cfg.twitch.channel,
    )
    .await;
    let emotes: crate::emotes::SharedEmotes = Arc::new(RwLock::new(initial_emotes));

    let custom_badges = match &cfg.badges {
        Some(badge_cfg) => {
            let http = http_client();
            match crate::badges::resolve_full(badge_cfg, &http).await {
                Ok(state) => {
                    info!(
                        count = state.resolved.definitions.len(),
                        "custom badge cache ready"
                    );
                    Arc::new(RwLock::new(Some(state)))
                }
                Err(e) => {
                    warn!(error = %e, "custom badge resolution failed — starting without");
                    Arc::new(RwLock::new(None))
                }
            }
        }
        None => Arc::new(RwLock::new(None)),
    };

    let quit = CancellationToken::new();
    let rebind = Arc::new(Notify::new());

    spawn_badge_supervisor(badges.clone(), live.clone(), quit.clone());

    // Channel-scoped Helix (reward titles, redemptions) — present only when
    // `--auth` has stored a user token for this client_id.
    let helix = Arc::new(
        quiver_twitch::HelixClient::new(
            cfg.twitch.client_id.clone().unwrap_or_default(),
            cfg.twitch.client_secret.clone().unwrap_or_default(),
        )
        .expect("helix client builds")
        .with_channel_auth(),
    );
    let reward_titles: engine::SharedRewardTitles = Arc::new(RwLock::new(HashMap::new()));
    let redeem_deduper: engine::SharedRedeemDeduper =
        Arc::new(std::sync::Mutex::new(quiver_twitch::RedeemDeduper::default()));
    // Channel coin icon (GQL, anonymous): what Twitch shows for rewards
    // without a custom uploaded icon. None = fetch failed/unresolved —
    // the widget falls back to its SVG coin.
    let coin_icon: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
    // Event-driven refresh signals (no ticker — see refresher below):
    // - swap_gen: bumped by the reload layer on SwapChannel (a bounce
    //   also retargets EventSub via broadcaster_slot)
    // - miss_gen: bumped by pump/poller on a reward-cache miss (new
    //   reward mid-stream) — self-healing without idle polling
    //   (watch::Sender/Receiver: multi-consumer, no lost wakeups, no
    //   permit stealing — the Notify variant had exactly that bug)
    let (swap_tx, swap_rx) = tokio::sync::watch::channel(std::time::Instant::now());
    let miss_gen = Arc::new(Notify::new());
    if helix.has_channel_auth() {
        info!(
            user = ?helix.channel_login(),
            scopes = helix.channel_scopes().len(),
            "channel oauth loaded — channel-scoped features enabled"
        );
        let helix = helix.clone();
        let reward_titles = reward_titles.clone();
        let live_for_titles = live.clone();
        let quit = quit.clone();
        let coin_for_refresh = coin_icon.clone();
        let mut swap_for_refresh = swap_rx.clone();
        let miss_for_refresh = miss_gen.clone();
        tokio::spawn(async move {
            // Refresh once per trigger: immediately on boot, on channel
            // swap, or on a cache miss (cooldown 10s so a burst of misses
            // coalesces). No idle ticker.
            const COOLDOWN: Duration = Duration::from_secs(10);
            let mut last_run = Option::<tokio::time::Instant>::None;
            async fn refresh(
                helix: &quiver_twitch::HelixClient,
                live: &SharedLive,
                coin: &Arc<RwLock<Option<String>>>,
                reward_titles: &engine::SharedRewardTitles,
            ) {
                let broadcaster = match live.read().map(|l| l.channel.clone()) {
                    Ok(ch) => ch,
                    Err(_) => return,
                };
                // Channel coin (anonymous GQL) — what Twitch shows for
                // default-icon rewards on this channel.
                match helix.coin_icon_url(&broadcaster).await {
                    Some(url) => {
                        if let Ok(mut c) = coin.write() {
                            *c = Some(url);
                        }
                    }
                    None => warn!("coin icon lookup failed — keeping previous"),
                }
                match crate::serve::channel_broadcaster_id(helix, &broadcaster).await {
                    Some(bid) => {
                        match helix.custom_reward_titles(&bid).await {
                            Ok(map) => {
                                let with_icon =
                                    map.values().filter(|i| i.image_url.is_some()).count();
                                info!(
                                    rewards = map.len(),
                                    with_icon,
                                    "reward info cache refreshed"
                                );
                                if let Ok(mut t) = reward_titles.write() {
                                    *t = map;
                                }
                            }
                            Err(e) => warn!(error = %e, "reward title refresh failed"),
                        }
                    }
                    None => {}
                }
            }

            refresh(&helix, &live_for_titles, &coin_for_refresh, &reward_titles).await;
            last_run = Some(tokio::time::Instant::now());
            let mut swap_seen = *swap_for_refresh.borrow();
            loop {
                let swap_fut = swap_for_refresh.changed();
                let miss_fut = miss_for_refresh.notified();
                tokio::select! {
                    _ = quit.cancelled() => return,
                    _ = swap_fut => {
                        swap_seen = *swap_for_refresh.borrow_and_update();
                        info!("channel swapped — refreshing reward info + coin");
                    }
                    _ = miss_fut => {
                        if let Some(t) = last_run
                            && tokio::time::Instant::now().duration_since(t) < COOLDOWN
                        {
                            continue; // recent refresh — a single miss-burst already covered
                        }
                        debug!("reward cache miss — refreshing reward info");
                    }
                }
                refresh(&helix, &live_for_titles, &coin_for_refresh, &reward_titles).await;
                last_run = Some(tokio::time::Instant::now());
            }
        });
    }

    // Ctrl-C cancels `quit`; every component observes it.
    {
        let quit = quit.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutdown signal received");
            quit.cancel();
        });
    }

    // Fail-closed honesty: a filter that cannot compile must never boot a
    // tool that silently moderates nothing. (validate() already rejects
    // broken patterns; this is the belt to those suspenders.)
    let compiled_filters = crate::filters::CompiledFilters::compile(&cfg.filters)
        .map_err(|e| anyhow::anyhow!("filters failed to compile: {e}"))?;
    let filters: crate::filters::SharedCompiled = Arc::new(RwLock::new(Some(compiled_filters)));

    if helix.has_channel_auth() {
        // Redemption poller (needs `filters`, defined just above). See Phase C.
        // Redemption poller: emits redeem events for rewards that never
        // reach IRC chat (input-free redemptions are invisible to PRIVMSG).
        // IRC-driven duplicates are suppressed by the shared dedupe ring.
        let helix = helix.clone();
        let reward_titles = reward_titles.clone();
        let live_for_poll = live.clone();
        let quit = quit.clone();
        let tx_for_poll = tx.clone();
        let filters_for_poll = filters.clone();
        let deduper_for_poll = redeem_deduper.clone();
        let coin_for_poll = coin_icon.clone();
        let miss_for_poll = miss_gen.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(10));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = quit.cancelled() => return,
                    _ = ticker.tick() => {
                        let broadcaster = match live_for_poll.read().map(|l| l.channel.clone()) {
                            Ok(ch) => ch,
                            Err(_) => continue,
                        };
                        let Some(bid) = crate::serve::channel_broadcaster_id(&helix, &broadcaster).await else {
                            continue;
                        };
                        let Ok(redemptions) = helix.open_redemptions(&bid).await else {
                            continue;
                        };
                        let now = now_epoch_secs();
                        for r in redemptions {
                            // Only recent ones: the poll is the SAFETY NET for
                            // no-text rewards, not a replay of history.
                            let age = now.saturating_sub(r.created_at_secs());
                            if age > 120 {
                                continue;
                            }
                            // Unique-redemption-id gate (vs EventSub) and the
                            // IRC-coverage check: if IRC just rendered this
                            // (user, reward), the poller skips.
                            let delivered = deduper_for_poll
                                .lock()
                                .map(|mut d| {
                                    d.is_new_redemption_id(&r.id)
                                        && !d.irc_already_rendered(
                                            &r.user_login,
                                            &r.user_id,
                                            &r.reward_id,
                                        )
                                })
                                .unwrap_or(true);
                            if !delivered {
                                continue;
                            }
                            let info = reward_titles
                                .read()
                                .ok()
                                .and_then(|m| m.get(&r.reward_id).cloned());
                            let (title, mut image) = info
                                .map(|i| (Some(i.title), i.image_url))
                                .unwrap_or((None, None));
                            // Cache miss = reward added mid-stream: trigger
                            // the reactive refresher (cooldown in refresher).
                            if title.is_none() {
                                miss_for_poll.notify_one();
                            }
                            // Default-icon rewards: channel coin fallback.
                            if image.is_none() {
                                image = coin_for_poll.read().ok().and_then(|c| c.clone());
                            }
                            if !crate::filters::CompiledFilters::permits_event(
                                &filters_for_poll,
                                crate::filters::MsgKind::Redeem,
                                &r.user_login,
                                &r.user_display_name,
                                &r.user_id,
                                &r.user_input,
                            ) {
                                continue;
                            }
                            let wire = serde_json::json!({
                                "type": "event",
                                "event": {
                                    "kind": "redeem",
                                    "user_login": r.user_login,
                                    "display_name": r.user_display_name,
                                    "reward_title": title,
                                    "reward_image": image,
                                    "user_input": r.user_input,
                                }
                            });
                            let _ = tx_for_poll.send(wire.to_string());
                        }
                    }
                }
            }
        });

    }

    let feed = spawn_feed(
        live.clone(),
        messages.clone(),
        tx.clone(),
        filters.clone(),
        reward_titles.clone(),
        coin_icon.clone(),
        miss_gen.clone(),
        redeem_deduper.clone(),
    );

    // Frontend hot reload: watch the widget dir, push {type:reload} frames.
    let (fe_watch_tx, fe_watch_rx) = tokio::sync::mpsc::unbounded_channel::<Option<PathBuf>>();
    crate::reload::spawn_frontend_watcher(
        live.read().unwrap().widget_dist.clone(),
        fe_watch_rx,
        tx.clone(),
    );

    // EventSub WebSocket (Phase D): real-time redeems (with reward titles
    // inline), hype trains, predictions, polls — same broadcast channel.
    if helix.has_channel_auth() {
        // EventSub WebSocket: real-time redeems (with reward titles inline),
        // hype trains, predictions, polls. Same broadcast channel. The
        // message_type filters gate each mapped frame via the `gate` closure
        // (kind string -> MsgKind -> permits_event).
        let helix = helix.clone();
        let quit = quit.clone();
        let tx_for_es = tx.clone();
        let filters_for_es = filters.clone();
        let deduper_for_es = redeem_deduper.clone();
        let swap_for_es = swap_rx.clone();
        // Current channel login resolver for EventSub (reads live per call).
        let live_for_es = live.clone();
        let channel_resolver: Arc<dyn Fn() -> Option<String> + Send + Sync> =
            Arc::new(move || live_for_es.read().ok().and_then(|l| Some(l.channel.clone())));
        let gate = Arc::new(move |kind: &str| {
            let Some(kind_enum) = crate::filters::MsgKind::parse(kind) else {
                return false;
            };
            crate::filters::CompiledFilters::permits_event(
                &filters_for_es,
                kind_enum,
                "",
                "",
                "",
                "",
            )
        });
        tokio::spawn(async move {
            // EventSub re-targets on channel swap: per session it resolves
            // the CURRENT channel (login via the resolver closure),
            // broadcaster id, and coin — then bounces its socket when a swap
            // lands mid-stream.
            quiver_twitch::eventsub::spawn(
                helix,
                channel_resolver,
                tx_for_es,
                quit,
                gate,
                deduper_for_es,
                swap_for_es,
            );
        });
    }

    let ctx = crate::reload::ReloadCtx {
        live: live.clone(),
        messages: messages.clone(),
        tx: tx.clone(),
        badges: badges.clone(),
        feed_swap: feed.swap_tx,
        fe_watch: fe_watch_tx,
        custom_badges: custom_badges.clone(),
        custom_css: custom_css.clone(),
        emotes: emotes.clone(),
        filters: filters.clone(),
        rebind: rebind.clone(),
        channel_changed: swap_tx,
    };
    crate::reload::spawn_watcher(config_path, ctx, quit.clone());

    loop {
        let addr = live.read().unwrap().listen.clone();
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                warn!(%addr, %e, "cannot bind — retrying in 2s (fix server.listen in the config)");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        // Per-binding tokens: `stop_bind` ends this server instance,
        // `ws_token` (its child) frees WebSocket handlers on drain.
        let stop_bind = quit.child_token();
        let ws_token = stop_bind.child_token();
        let app = router(AppState {
            live: live.clone(),
            messages: messages.clone(),
            tx: tx.clone(),
            badges: badges.clone(),
            custom_badges: custom_badges.clone(),
            custom_css: custom_css.clone(),
            emotes: emotes.clone(),
            ws_token,
        });

        info!(%addr, "widget server listening");

        // SINGLE consumer rule: the graceful-shutdown future watches ONLY
        // `stop_bind`. The `rebind` Notify has exactly one waiter — the
        // outer select below — which then cancels `stop_bind`. Two
        // notified() waiters would race for notify_one's single permit.
        let mut serve = std::pin::pin!(
            axum::serve(listener, app)
                .with_graceful_shutdown({
                    let stop_bind = stop_bind.clone();
                    async move { stop_bind.cancelled().await }
                })
                .into_future()
        );

        enum Stop {
            Quit,
            Rebind,
        }
        let stop = tokio::select! {
            r = &mut serve => {
                r?;
                Stop::Quit
            }
            _ = quit.cancelled() => Stop::Quit,
            _ = rebind.notified() => {
                stop_bind.cancel();
                Stop::Rebind
            }
        };

        // Let graceful drain finish (WS handlers see the token and exit).
        serve.as_mut().await?;

        match stop {
            Stop::Quit => break,
            Stop::Rebind => info!("listener rebound"),
        }
    }

    info!("widget server stopped");
    Ok(())
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(|| async { "ok" }))
        .route("/badge-cache/{hash}", get(badge_cache_handler))
        .route("/badge-file/{hash}", get(badge_file_handler))
        .route(
            "/emotes.json",
            get(|State(state): State<AppState>| async move {
                let map = state.emotes.read().map(|m| m.clone()).unwrap_or_default();
                (
                    [(header::CONTENT_TYPE, "application/json")],
                    [(header::CACHE_CONTROL, "no-cache")],
                    serde_json::json!({ "providers": map }).to_string(),
                )
            }),
        )
        .fallback(get(static_fallback))
        .with_state(state)
}

// ---- feed -----------------------------------------------------------------

/// Supervised Twitch feed. Keeps ONE source alive across config reloads:
/// channel swaps are performed live via part/join on the client handle.
///
/// The SUPERVISOR tier (this task) restarts `feed_session` when it PANICS
/// (JoinError = panic unwound through the session) — previously the spawned
/// task's JoinHandle was dropped and a panic permanently killed chat until
/// process restart. A clean session return happens only on the shutdown
/// signal (Swap(None): swap_tx dropped by the caller), which stops
/// supervision. swap_rx lives in an Arc<Mutex> so it survives task panics.
fn spawn_feed(
    live: SharedLive,
    messages: engine::SharedState,
    tx: broadcast::Sender<String>,
    filters: crate::filters::SharedCompiled,
    reward_titles: engine::SharedRewardTitles,
    coin_icon: Arc<RwLock<Option<String>>>,
    miss_gen: Arc<Notify>,
    redeem_deduper: engine::SharedRedeemDeduper,
) -> FeedHandle {
    let (swap_tx, swap_rx) = mpsc::unbounded_channel::<String>();
    let swap_rx = Arc::new(tokio::sync::Mutex::new(swap_rx));
    tokio::spawn(async move {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            let session = tokio::spawn(feed_session(
                live.clone(),
                messages.clone(),
                tx.clone(),
                filters.clone(),
                reward_titles.clone(),
                coin_icon.clone(),
                miss_gen.clone(),
                redeem_deduper.clone(),
                swap_rx.clone(),
            ));
            match session.await {
                Ok(()) => break, // shutdown signal only
                Err(e) => {
                    if e.is_panic() {
                        warn!(error = %e, "chat feed task panicked — restarting");
                    } else {
                        warn!(error = %e, "chat feed task cancelled — restarting");
                    }
                    // Swap requests queued during the backoff are drained by
                    // the next incarnation (unbounded queue + shared rx).
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    });
    FeedHandle { swap_tx }
}

// Join the new channel FIRST, part the old one only after success.
/// One incarnation of the supervised feed: connect with backoff, pump until
/// the source ends or the swap queue closes. Panics ARE possible (poisoned
/// locks, library internals) — the supervisor restarts on JoinError.
async fn feed_session(
    live: SharedLive,
    messages: engine::SharedState,
    tx: broadcast::Sender<String>,
    filters: crate::filters::SharedCompiled,
    reward_titles: engine::SharedRewardTitles,
    coin_icon: Arc<RwLock<Option<String>>>,
    miss_gen: Arc<Notify>,
    redeem_deduper: engine::SharedRedeemDeduper,
    swap_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>>,
) {
    let mut backoff = INITIAL_BACKOFF;
    'outer: loop {
        // Poisoned lock degrades to a reconnect (empty channel) instead of
        // killing the feed — nothing here may panic by design.
        let channel = live.read().map(|l| l.channel.clone()).unwrap_or_default();
        let Ok(mut source) = quiver_twitch::IrcChatSource::connect_anonymous(channel.clone())
        else {
            warn!(%channel, "could not start chat feed");
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
            continue;
        };
        info!(%channel, "joining twitch chat");

        // The channel this connection is actually joined to. Starts at the
        // connect target and tracks every successful swap. Using a mutable
        // local (rather than re-reading the outer `channel` snapshot) means
        // a second swap parts the CURRENT channel, not the one the session
        // originally joined.
        let mut joined = channel;


        'session: loop {
            let started_at = Instant::now();

            enum FeedEvent {
                Ended,
                Swap(Option<String>),
            }
            // Scope the pump future so `source` frees for the swap path.
            let event = {
                let mut pump_fut = std::pin::pin!(engine::pump(
                    live.clone(),
                    messages.clone(),
                    tx.clone(),
                    filters.clone(),
                    reward_titles.clone(),
                    coin_icon.clone(),
                    miss_gen.clone(),
                    redeem_deduper.clone(),
                    &mut source
                ));
                tokio::select! {
                    _ = &mut pump_fut => FeedEvent::Ended,
                    swapped = async { swap_rx.lock().await.recv().await } => {
                        FeedEvent::Swap(swapped)
                    }
                }
            };

            match event {
                FeedEvent::Ended => {
                    if started_at.elapsed() > HEALTHY_FEED_RUNTIME {
                        backoff = INITIAL_BACKOFF;
                    }
                    warn!("chat feed ended");
                    break 'session;
                }
                FeedEvent::Swap(None) => break 'outer, // watcher gone: shutdown
                FeedEvent::Swap(Some(new_channel)) => {
                    // Stale-swap guard: swap requests can pile up in the
                    // queue while the feed is down (`swap_rx` is only
                    // polled inside 'session). live already reflects the
                    // newest config, so a queued target that differs from
                    // live.channel is stale — applying it would part the
                    // just-joined channel and re-join a superseded one,
                    // and pump would then drop every message (straggler
                    // gate mismatch) with no recovery.
                    let want = live.read().map(|l| l.channel.clone()).unwrap_or_default();
                    if new_channel == joined {
                        // Already there (duplicate or stale) — nothing to do.
                        continue 'session;
                    }
                    if new_channel != want {
                        debug!(queued = %new_channel, want = %want, "stale channel swap ignored");
                        continue 'session;
                    }
                    // swap_channel joins FIRST and parts the OLD channel only
                    // on success, so on join failure the old feed keeps
                    // flowing (supervisor contract).
                    swap_channel(&source, &joined, &new_channel, &messages, &tx);
                    joined = new_channel;
                    continue 'session;
                }
            }
        }

        warn!(sleep_secs = backoff.as_secs(), "restarting chat feed");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}
/// The new login was validated before the reload was accepted, so join
/// failures here are logged and non-fatal — and the old feed MUST keep
/// flowing, which is exactly why the part cannot happen first: parting
/// before a failed join would leave the connection in NEITHER channel
/// (the previous ordering contradicted this comment).
fn swap_channel(
    source: &quiver_twitch::IrcChatSource,
    old: &str,
    new: &str,
    messages: &engine::SharedState,
    tx: &broadcast::Sender<String>,
) {
    let client = source.client();
    if let Err(e) = client.join(new.to_string()) {
        warn!(%new, %e, "join failed after channel swap — staying on old channel");
        return;
    }
    // `part` is fire-and-forget (returns `()`, queued by the library):
    // once the join succeeds we are guaranteed to be in the new channel,
    // even if the server processes the PART after the JOIN.
    client.part(old.to_string());
    let cleared = messages.lock().map(|mut m| m.clear()).unwrap_or_default();
    debug!(count = cleared.len(), "history cleared on channel swap");
    let _ = tx.send(r#"{"type":"clear"}"#.to_string());
    info!(from = %old, to = %new, "chat channel swapped");
}

// ---- badges ---------------------------------------------------------------

/// Broadcaster id for the channel via app-token Helix; None on failure
/// (the refresher treats it as "nothing to do this tick").
async fn channel_broadcaster_id(helix: &quiver_twitch::HelixClient, channel: &str) -> Option<String> {
    match helix.user_id(channel).await {
        Ok(id) => Some(id),
        Err(e) => {
            warn!(channel = %channel, error = %e, "broadcaster id lookup failed");
            None
        }
    }
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Fetch the native Twitch badge map (`set_id` -> CDN URL).
///
/// Returns `Err` when the LOOKUP failed (offline, bad credentials, Helix
/// error). That is deliberately distinct from `Ok(empty)`: a caller that
/// collapses both into one map caches a transient network blip as "this
/// channel has no badges" and never recovers on its own.
pub(crate) async fn load_badge_map(
    client_id: &str,
    client_secret: &str,
    channel_login: &str,
) -> anyhow::Result<HashMap<String, String>> {
    let helix = quiver_twitch::HelixClient::new(client_id, client_secret)?;
    // A failed channel lookup is NOT fatal — the global badge set still
    // renders without a broadcaster id.
    let broadcaster_id = match helix.user_id(channel_login).await {
        Ok(id) => Some(id),
        Err(e) => {
            warn!(
                channel = %channel_login,
                error = %e,
                "channel id lookup failed — using global badges only"
            );
            None
        }
    };
    Ok(helix.badge_map(broadcaster_id.as_deref()).await?)
}

/// Exponential backoff for [`spawn_badge_supervisor`], capped at
/// [`MAX_BACKOFF`]. Pure so the retry schedule is table-tested.
fn badge_retry_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_BACKOFF)
}

/// Keep retrying the native badge fetch until it succeeds.
///
/// The boot fetch in [`run`] happens exactly once, so a machine that starts
/// without network (or while Twitch is unreachable) used to stay badge-less
/// FOREVER: `planned_actions` only schedules `RefreshBadges` when the
/// channel or the credentials change, so neither a config reload nor a
/// SIGHUP could recover it — only a process restart could.
///
/// This exits as soon as the map is non-empty, so a successful boot costs no
/// extra Helix call and there is no idle polling afterwards. Later transient
/// failures are handled by `Action::RefreshBadges` keeping the previous map.
fn spawn_badge_supervisor(badges: SharedBadges, live: SharedLive, quit: CancellationToken) {
    tokio::spawn(async move {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            // Already populated (the boot fetch won) — nothing to supervise.
            if badges.read().map(|b| !b.is_empty()).unwrap_or(false) {
                return;
            }
            let (creds, channel) = live
                .read()
                .map(|l| (l.creds.clone(), l.channel.clone()))
                .unwrap_or_default();
            // Missing credentials is a configuration state, not a transient
            // failure — `Action::RefreshBadges` fires when creds show up.
            let Some((client_id, client_secret)) = creds else {
                return;
            };
            match load_badge_map(&client_id, &client_secret, &channel).await {
                Ok(map) if !map.is_empty() => {
                    info!(badge_count = map.len(), "badge map recovered");
                    if let Ok(mut b) = badges.write() {
                        *b = map;
                    }
                    return;
                }
                // Helix always returns the global badge set for a valid
                // client, so an empty map here means something is still wrong.
                Ok(_) => warn!("badge lookup returned no badges — retrying"),
                Err(e) => warn!(error = %e, "badge lookup failed — retrying"),
            }
            tokio::select! {
                _ = quit.cancelled() => return,
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = badge_retry_backoff(backoff);
        }
    });
}

// ---- wire -----------------------------------------------------------------

/// Meta block shared by snapshots and config-update frames.
pub(crate) fn meta_value(
    live: &LiveConfig,
    badges: &HashMap<String, String>,
    custom_badges: Option<&crate::badges::ResolvedCustomBadges>,
    custom_css: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "theme": {
            "font_size_px": live.theme.font_size_px,
            "max_messages": live.theme.max_messages,
            "overflow_mode": live.theme.overflow_mode,
            "event_banner_secs": live.theme.event_banner_secs,
        },
        "badges": badges,
        "custom_css": custom_css,
        "role_css": live.theme.role_css,
        "emote_flags": {
            "twitch": live.emotes.twitch,
            "unicode": live.emotes.unicode,
            "seventv": live.emotes.seventv,
            "bttv": live.emotes.bttv,
            "ffz": live.emotes.ffz,
        },
        "custom_badges": custom_badges,
    })
}

fn snapshot_frame(app: &AppState) -> Option<String> {
    let live = app.live.read().ok()?;
    let badges = app.badges.read().ok()?;
    let custom_badges = app.custom_badges.read().ok()?;
    let custom = custom_badges.as_ref().map(|c| &c.resolved);
    let css = app.custom_css.read().ok().and_then(|c| c.clone());
    let st = app.messages.lock().ok()?;
    let messages: Vec<&engine::RenderedMessage> = st.messages().collect();
    Some(
        serde_json::json!({
            "type": "snapshot",
            "messages": messages,
            "meta": meta_value(&live, &badges, custom, css.as_deref()),
        })
        .to_string(),
    )
}

// ---- handlers -------------------------------------------------------------

/// Origin policy for the WebSocket feed.
///
/// The widget always connects same-origin (`${location.host}/ws`, main.js),
/// so a cross-origin upgrade is either a misconfigured custom client or CSWSH
/// (cross-site WebSocket hijacking): a web page from elsewhere opening
/// `ws://<host>:<port>/ws` and streaming the raw chat feed. Browsers send
/// `Origin` on every WebSocket handshake, so its absence is also rejected.
fn is_allowed_origin(origin: Option<&header::HeaderValue>, listen: &str) -> bool {
    let Some(origin) = origin.and_then(|o| o.to_str().ok()) else {
        return false;
    };
    let Some((listen_host, listen_port)) = parse_listen(listen) else {
        return false;
    };
    let Some(origin) = parse_origin(origin) else {
        return false;
    };
    if origin.port != listen_port {
        return false;
    }
    match listen_host.as_str() {
        // All interfaces: LAN clients can legitimately use any hostname, so
        // only port equality is enforced (documented relaxation).
        "" | "0.0.0.0" | "::" => true,
        // Loopback-bound listener: only loopback origins. This kills CSWSH
        // in the default topology even though an attacker can guess the port.
        h if is_loopback_host(h) => is_loopback_host(origin.host),
        // Concrete non-loopback address: require an exact host match.
        h => h == origin.host.to_ascii_lowercase(),
    }
}

struct Origin<'a> {
    host: &'a str,
    port: u16,
}

/// "host:port" (or "[v6]:port") → (normalized host, port).
fn parse_listen(s: &str) -> Option<(String, u16)> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = after.strip_prefix(':')?.parse().ok()?;
        return Some((host.to_ascii_lowercase(), port));
    }
    let (host, port) = s.rsplit_once(':')?;
    Some((host.to_ascii_lowercase(), port.parse().ok()?))
}

/// "http[s]://host[:port]" → host + port (scheme default when omitted).
fn parse_origin(s: &str) -> Option<Origin<'_>> {
    let (rest, is_https) = if let Some(r) = s.strip_prefix("https://") {
        (r, true)
    } else if let Some(r) = s.strip_prefix("http://") {
        (r, false)
    } else {
        return None;
    };
    let (host, explicit) = split_host_port(rest);
    let port = explicit.unwrap_or(if is_https { 443 } else { 80 });
    Some(Origin { host, port })
}

/// "host", "host:port", "[v6]", "[v6]:port" → (host, explicit port).
fn split_host_port(s: &str) -> (&str, Option<u16>) {
    if let Some(rest) = s.strip_prefix('[') {
        match rest.split_once(']') {
            Some((host, after)) => (host, after.strip_prefix(':').and_then(|p| p.parse().ok())),
            None => (s, None),
        }
    } else {
        match s.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().ok()),
            None => (s, None),
        }
    }
}

fn is_loopback_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    h == "localhost" || h == "::1" || h.starts_with("127.")
}

async fn ws_handler(
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
    req: Request,
) -> Response {
    let listen = state.live.read().unwrap().listen.clone();
    if !is_allowed_origin(req.headers().get(header::ORIGIN), &listen) {
        warn!("cross-origin websocket upgrade denied");
        return (StatusCode::FORBIDDEN, "cross-origin websocket denied").into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(socket, state))
        .into_response()
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    // Subscribe FIRST, drain buffered frames, THEN snapshot.
    //
    // engine::pump broadcasts the message frame under the same lock it
    // mutates history under, so:
    //   - every frame buffered at drain time predates the snapshot's state
    //     read — its mutation is already in the snapshot, safe to discard;
    //   - every mutation AFTER the snapshot read is not in the snapshot and
    //     is delivered live via rx — no loss.
    // The only residual race (a mutation between the drain and the snapshot
    // read) shows up as a DUPLICATE, which the widget's message handler
    // dedupes by message id.
    //
    // The old order (snapshot → send().await → subscribe) lost real
    // messages on every (re)connect: the send await is a real window under
    // TCP backpressure and broadcasts in it reached neither the snapshot
    // nor the not-yet-created receiver.
    let mut rx = state.tx.subscribe();
    loop {
        match rx.try_recv() {
            Ok(_) => continue, // buffered pre-snapshot frame: already in snapshot
            Err(broadcast::error::TryRecvError::Empty)
            | Err(broadcast::error::TryRecvError::Lagged(_)) => break,
            Err(broadcast::error::TryRecvError::Closed) => return, // senders dropped
        }
    }
    let Some(snapshot) = snapshot_frame(&state) else {
        return;
    };
    if socket.send(Message::Text(snapshot.into())).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            res = rx.recv() => match res {
                Ok(text) => {
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    debug!(missed = n, "ws client lagged, resyncing");
                    let Some(snapshot) = snapshot_frame(&state) else { return };
                    if socket.send(Message::Text(snapshot.into())).await.is_err() {
                        return;
                    }
                }
                Err(_) => return,
            },
            _ = state.ws_token.cancelled() => return,
            _ = socket.recv() => return,
        }
    }
}

/// Static files resolved PER REQUEST from the live widget_dist path —
/// hot-swapping `server.widget_dist` needs no router rebuild.
/// Serve a cached custom badge body by content hash. Content-addressed →
/// immutable → browser caches aggressively with no revalidation.
async fn badge_cache_handler(
    State(state): State<AppState>,
    AxumPath(hash): AxumPath<String>,
) -> Response {
    let guard = match state.custom_badges.read() {
        Ok(g) => g,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "cache lock").into_response(),
    };
    let Some(cache) = guard.as_ref() else {
        return (StatusCode::NOT_FOUND, "no badge cache").into_response();
    };
    match crate::badges::read_cached_file(cache, &hash) {
        Some((bytes, content_type)) => (
            [
                (header::CONTENT_TYPE, content_type.as_str()),
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            bytes,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// Serve a file:// custom badge by url-hash — pure passthrough from the
/// canonical path (no caching: user-controlled local files are read fresh).
async fn badge_file_handler(
    State(state): State<AppState>,
    AxumPath(hash): AxumPath<String>,
) -> Response {
    let guard = match state.custom_badges.read() {
        Ok(g) => g,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "cache lock").into_response(),
    };
    let Some(cache) = guard.as_ref() else {
        return (StatusCode::NOT_FOUND, "no badge cache").into_response();
    };
    let Some(path) = cache.file_badges.get(&hash) else {
        return (StatusCode::NOT_FOUND, "unknown file badge").into_response();
    };
    match std::fs::read(path) {
        Ok(bytes) => {
            let ct = crate::badges::sniff_content_type(&bytes);
            (
                [
                    (header::CONTENT_TYPE, ct),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                bytes,
            )
                .into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "file badge missing").into_response(),
    }
}

async fn static_fallback(State(state): State<AppState>, req: Request) -> Response {
    let dist = state.live.read().unwrap().widget_dist.clone();
    let Some(dist) = dist.filter(|d| d.is_dir()) else {
        return Html(PLACEHOLDER_HTML).into_response();
    };

    let requested = req.uri().path().trim_start_matches('/');
    let rel = if requested.is_empty() {
        "index.html"
    } else {
        requested
    };
    let rel_path = PathBuf::from(rel);
    // Reject anything that is not a plain relative path (no `..`, absolute).
    if rel_path
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return (StatusCode::BAD_REQUEST, "bad path").into_response();
    }

    let mut full = dist.join(&rel_path);
    if full.is_dir() {
        full.push("index.html");
    }
    match std::fs::read(&full) {
        Ok(bytes) => {
            // Cache-busting: bake the widget dir's newest mtime into the
            // asset URLs in index.html. A cache that ignores no-cache
            // still cannot serve a stale file across a DIFFERENT URL.
            //
            // Only index.html goes through the text/replace path — every
            // other file must be served as raw bytes, or binary assets
            // (images/fonts in the widget dir) get silently mangled by
            // from_utf8_lossy.
            if full.ends_with("index.html") && dist.is_dir() {
                (
                    [
                        (header::CONTENT_TYPE, mime_of(&full)),
                        // Reloads must always pull fresh bytes from disk — without
                        // this, Chromium heuristically caches main.js and a stale
                        // copy keeps rendering no matter how many reloads fire.
                        (header::CACHE_CONTROL, "no-cache"),
                        // Legacy CEF builds may not trust no-cache alone.
                        (header::PRAGMA, "no-cache"),
                        (header::EXPIRES, "0"),
                    ],
                    static_body(bytes, &full, &dist),
                )
                    .into_response()
            } else {
                (
                    [
                        (header::CONTENT_TYPE, mime_of(&full)),
                        (header::CACHE_CONTROL, "no-cache"),
                        (header::PRAGMA, "no-cache"),
                        (header::EXPIRES, "0"),
                    ],
                    bytes,
                )
                    .into_response()
            }
        }
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// Response body for a static widget file.
///
/// Only `index.html` is treated as text (cache-busting version token
/// substitution); every other file — including binary assets like images
/// or fonts in the widget dir — must be served byte-identical. Running
/// those through `String::from_utf8_lossy` would silently corrupt them.
fn static_body(bytes: Vec<u8>, full: &Path, dist: &Path) -> Vec<u8> {
    if full.ends_with("index.html") && dist.is_dir() {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        text.replace("__QUIVER_VERSION__", &quiver_widget_version(dist).to_string())
            .into_bytes()
    } else {
        bytes
    }
}

/// Newest mtime (epoch millis) of any file under the widget dir
/// (recursive — assets live in subdirs like src/) — the cache-busting
/// version for asset URLs.
fn quiver_widget_version(dist: &Path) -> u64 {
    let mut newest = 0u64;
    let mut stack: Vec<PathBuf> = vec![dist.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata() {
                    if meta.is_dir() {
                        stack.push(entry.path());
                    } else if meta.is_file()
                        && let Ok(t) = meta.modified()
                        && let Ok(ms) = t.duration_since(std::time::UNIX_EPOCH)
                    {
                        newest = newest.max(ms.as_millis() as u64);
                    }
                }
            }
        }
    }
    newest
}

fn mime_of(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        Some("map") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Revalidation tokens for one http(s) custom_css source, stored as a
/// sidecar `css-{url-hash}.json` next to the body cache file.
#[derive(serde::Deserialize, serde::Serialize, Default)]
struct CssMeta {
    etag: Option<String>,
    last_modified: Option<String>,
}

/// Fetch CSS source bytes from a file:// or http(s):// URI — the same
/// scheme semantics as badges. file:// is a passthrough read (canonical
/// path); http(s) uses a conditional GET against a per-URL cache
/// (body `css-{url-hash}.bin` + meta sidecar), so a 304 or an unchanged
/// file is served from cache instead of an unconditional refetch, and the
/// stored file is actually READ BACK (previously it was written and never
/// consulted).
pub(crate) async fn fetch_css_bytes(
    uri: &str,
    http: &reqwest::Client,
    cache_dir: &std::path::Path,
) -> Result<Vec<u8>, String> {
    if uri.starts_with("file://") {
        let path_str = uri
            .strip_prefix("file://")
            .ok_or_else(|| format!("{uri}: invalid file URI"))?;
        let canonical = std::fs::canonicalize(path_str).map_err(|e| format!("{uri}: {e}"))?;
        return std::fs::read(&canonical).map_err(|e| format!("{uri}: {e}"));
    }
    if uri.starts_with("http://") || uri.starts_with("https://") {
        let uhash = crate::badges::sha256_hex_public(uri.as_bytes());
        let body_path = cache_dir.join(format!("css-{uhash}.bin"));
        let meta_path = cache_dir.join(format!("css-{uhash}.json"));

        let meta: Option<CssMeta> = std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());
        let body_exists = body_path.is_file();

        let mut req = http.get(uri);
        if let Some(m) = &meta {
            if let Some(etag) = &m.etag {
                req = req.header("If-None-Match", etag);
            }
            if let Some(lm) = &m.last_modified {
                req = req.header("If-Modified-Since", lm);
            }
        }
        let resp = req.send().await.map_err(|e| format!("{uri}: {e}"))?;
        if resp.status() == reqwest::StatusCode::NOT_MODIFIED && body_exists {
            // Conditional GET says our copy is current — serve it back.
            return std::fs::read(&body_path).map_err(|e| format!("{uri}: cache read failed: {e}"));
        }
        if !resp.status().is_success() {
            return Err(format!("{uri}: HTTP {}", resp.status()));
        }
        let (etag, lm) = {
            let e = resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let l = resp
                .headers()
                .get("last-modified")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            (e, l)
        };
        let bytes = resp.bytes().await.map_err(|e| format!("{uri}: {e}"))?.to_vec();
        // Persist body + revalidation tokens (badge-style storage).
        let _ = std::fs::create_dir_all(cache_dir);
        let _ = std::fs::write(&body_path, &bytes);
        let new_meta = CssMeta { etag, last_modified: lm };
        if new_meta.etag.is_some() || new_meta.last_modified.is_some() {
            if let Ok(json) = serde_json::to_string(&new_meta) {
                let _ = std::fs::write(&meta_path, json);
            }
        }
        return Ok(bytes);
    }
    Err(format!(
        "{uri}: unsupported scheme (expected file:// or http(s)://)"
    ))
}

// ---- resolved custom CSS -------------------------------------------------

/// Resolved custom CSS text shared between the meta builder and reloads.
pub(crate) type SharedCss = Arc<RwLock<Option<String>>>;

/// Resolve the configured custom CSS source to plain text (inline as-is,
/// URI via file passthrough / http fetch). Fails softly: returns None on
/// any error with a WARN — live rendering must never break on a bad CSS
/// source (same philosophy as badge skip-on-error).
///
/// `cache_dir`: the badge cache directory from `badges.cache_dir` (or its
/// default). Passing it in keeps CSS http caching colocated with/redirected
/// alongside the badge cache, honoring the user's override.
pub(crate) async fn resolve_custom_css(
    source: &Option<crate::config::CustomCssSource>,
    http: &reqwest::Client,
    cache_dir: &std::path::Path,
) -> Option<String> {
    let Some(src) = source else { return None };
    match src.resolve(http, cache_dir).await {
        Ok(text) => {
            crate::config::report_css_lint(Some(&text), None);
            Some(text)
        }
        Err(e) => {
            tracing::warn!(error = %e, "custom_css source failed to resolve — ignoring");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EmotesConfig, ThemeConfig};
    use indexmap::IndexMap;

    #[test]
    fn badge_retry_backoff_doubles_then_caps() {
        // The whole point of the supervisor is that a boot without network
        // recovers on its own, so the schedule must never stall and never
        // grow past MAX_BACKOFF (otherwise recovery time is unbounded).
        let mut b = INITIAL_BACKOFF;
        let mut seen = vec![b.as_secs()];
        for _ in 0..8 {
            b = badge_retry_backoff(b);
            seen.push(b.as_secs());
        }
        assert_eq!(seen, vec![2, 4, 8, 16, 32, 60, 60, 60, 60]);
        assert!(b <= MAX_BACKOFF);
    }

    fn live_with(custom: Option<&str>, role: Option<IndexMap<String, String>>) -> LiveConfig {
        LiveConfig {
            listen: "127.0.0.1:1".into(),
            widget_dist: None,
            channel: "chan".into(),
            creds: None,
            filters: crate::config::FiltersConfig::default(),
            badges: None,
            theme: ThemeConfig {
                font_size_px: 18,
                max_messages: 30,
                message_lifetime_secs: 60,
                custom_css: custom.map(|c| crate::config::CustomCssSource::Inline(c.to_string())),
                role_css: role,
                overflow_mode: crate::config::OverflowMode::Prune,
                event_banner_secs: 8,
            },
            emotes: EmotesConfig::default(),
        }
    }

    /// Ground truth by hand: meta mirrors exactly what was passed in.
    #[test]
    fn meta_carries_custom_and_role_css() {
        let mut roles = IndexMap::new();
        roles.insert("moderator".to_string(), ".msg{}".to_string());
        let live = live_with(Some("/*c*/"), Some(roles));
        let m = meta_value(&live, &HashMap::new(), None, Some("/*c*/"));

        assert_eq!(m["custom_css"], "/*c*/");
        assert_eq!(m["role_css"]["moderator"], ".msg{}");
        assert_eq!(m["theme"]["font_size_px"], 18);
    }

    /// #108: the meta frame is what the widget injects, and it injects the
    /// snippets in object order. Equal specificity + `!important` means the
    /// last one wins, so config order IS priority order — it must survive
    /// `serde_json`'s `preserve_order` serialisation.
    #[test]
    fn meta_role_css_keeps_config_order() {
        // Seven keys on purpose: a two-key map has a 50% chance of hashing
        // into the same order it was inserted in, which would let a HashMap
        // slip past this test.
        let mut roles = IndexMap::new();
        for (role, colour) in [
            ("subscriber", "#ffd700"),
            ("founder", "#ffd700"),
            ("vip", "#e040fb"),
            ("moderator", "#00ff7f"),
            ("lead_moderator", "#8b4513"),
            ("global_mod", "#8b4513"),
            ("broadcaster", "#ff4500"),
        ] {
            roles.insert(
                role.to_string(),
                format!("div.msg.role-{role} {{ --c: {colour} !important; }}"),
            );
        }
        let live = live_with(None, Some(roles));
        let m = meta_value(&live, &HashMap::new(), None, None);

        let obj = m["role_css"].as_object().expect("role_css object");
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "subscriber",
                "founder",
                "vip",
                "moderator",
                "lead_moderator",
                "global_mod",
                "broadcaster",
            ]
        );
    }

    #[test]
    fn meta_omits_unset_css_fields_as_null() {
        let live = live_with(None, None);
        let m = meta_value(&live, &HashMap::new(), None, None);
        assert!(m["custom_css"].is_null());
        assert!(m["role_css"].is_null());
    }

    #[test]
    fn ws_origin_loopback_policy() {
        let h = |o: &str| header::HeaderValue::from_str(o).unwrap();
        let def = "127.0.0.1:4783";
        assert!(is_allowed_origin(Some(&h("http://localhost:4783")), def));
        assert!(is_allowed_origin(Some(&h("http://127.0.0.1:4783")), def));
        assert!(is_allowed_origin(Some(&h("http://[::1]:4783")), def));
        assert!(!is_allowed_origin(Some(&h("http://evil.example:4783")), def));
        assert!(!is_allowed_origin(Some(&h("http://localhost:9999")), def));
        assert!(!is_allowed_origin(Some(&h("http://evil.example:9999")), def));
        // No Origin header (non-browser client) is rejected outright.
        assert!(!is_allowed_origin(None, def));
        // Malformed origin strings are rejected.
        assert!(!is_allowed_origin(Some(&h("127.0.0.1:4783")), def));
        assert!(!is_allowed_origin(Some(&h("ftp://localhost:4783")), def));
    }

    #[test]
    fn ws_origin_all_interfaces_relaxes_host_but_keeps_port() {
        let h = |o: &str| header::HeaderValue::from_str(o).unwrap();
        let any = "0.0.0.0:4783";
        assert!(is_allowed_origin(Some(&h("http://192.168.1.10:4783")), any));
        assert!(!is_allowed_origin(Some(&h("http://192.168.1.10:1337")), any));
        assert!(!is_allowed_origin(None, any));
    }

    #[test]
    fn ws_origin_concrete_host_requires_exact_match() {
        let h = |o: &str| header::HeaderValue::from_str(o).unwrap();
        let lan = "192.168.1.5:4783";
        assert!(is_allowed_origin(Some(&h("http://192.168.1.5:4783")), lan));
        assert!(!is_allowed_origin(Some(&h("http://192.168.1.6:4783")), lan));
        assert!(!is_allowed_origin(Some(&h("http://192.168.1.5:1")), lan));
    }

    #[test]
    fn ws_origin_scheme_default_ports() {
        let h = |o: &str| header::HeaderValue::from_str(o).unwrap();
        // Listen on 80: origin without explicit port (default http=80) matches.
        assert!(is_allowed_origin(Some(&h("http://localhost")), "127.0.0.1:80"));
        // Listen on 443: https origin without explicit port matches.
        assert!(is_allowed_origin(Some(&h("https://localhost")), "127.0.0.1:443"));
        // But http-origin default port does NOT match a 443 listener.
        assert!(!is_allowed_origin(Some(&h("https://localhost:80")), "127.0.0.1:443"));
    }

    #[test]
    fn static_body_passes_binary_assets_through_byte_identical() {
        // Invalid UTF-8 (a PNG-ish payload) must come back EXACTLY as served —
        // from_utf8_lossy would have replaced each bad byte with U+FFFD.
        let png = [0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x01, 0x02, 0xFF];
        let dist = std::path::Path::new("dist");
        let out = static_body(png.to_vec(), Path::new("dist/logo.png"), dist);
        assert_eq!(out, png, "binary asset was corrupted by the static fallback");
    }

    fn temp_widget_dir(html: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "quiver-test-widget-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), html).unwrap();
        dir
    }

    #[test]
    fn static_body_substitutes_version_token_only_in_index_html() {
        let dist = temp_widget_dir("");
        let html = b"<html><script src=\"app.js?v=__QUIVER_VERSION__\"></script></html>".to_vec();
        let out = static_body(html.clone(), &dist.join("index.html"), &dist);
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("__QUIVER_VERSION__"), "token was not substituted");
        // Non-index files never see the token substitution.
        let js = b"const v = \"__QUIVER_VERSION__\";".to_vec();
        let out = static_body(js.clone(), &dist.join("app.js"), &dist);
        assert_eq!(out, js, "non-index file must NOT be rewritten");
        let _ = std::fs::remove_dir_all(&dist);
    }
}
