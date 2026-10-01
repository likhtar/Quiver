//! Config hot reload: file watching + SIGHUP → load/validate/diff/apply.
//!
//! Contract: a reload is all-or-nothing. Parse errors, validation issues,
//! or an invalid channel login REJECT the whole reload — the running
//! config stays untouched. There is no half-applied state, ever.

use notify::{EventKind, Watcher as _};
use notify_debouncer_full::DebouncedEvent;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use quiver_config::Validate;

use crate::config::ChatConfig;
use crate::live::{Action, LiveConfig, SharedLive, planned_actions};
use crate::serve::SharedBadges;

const DEBOUNCE: Duration = Duration::from_millis(500);

/// Does this debounced batch mean the config file actually changed?
///
/// Matching the watched file name is not enough on its own. The reload
/// path reads the config back (`quiver_config::load_from_path`), and that
/// read raises `IN_OPEN` on the very file we watch, which notify reports
/// as `EventKind::Access`. Treating a read as a change makes every reload
/// schedule the next one, so a single save spins the watcher at the
/// debounce rate until the process restarts. Only change-producing kinds
/// may pass.
fn batch_touches_config(events: &[DebouncedEvent], file_name: &OsStr, watch_file: &Path) -> bool {
    events.iter().any(|e| {
        if matches!(e.kind, EventKind::Access(_)) {
            return false;
        }
        e.paths.iter().any(|p| p.file_name() == Some(file_name))
            || e.paths.iter().any(|p| p == watch_file)
    })
}

pub(crate) struct ReloadCtx {
    pub live: SharedLive,
    pub messages: crate::engine::SharedState,
    pub tx: tokio::sync::broadcast::Sender<String>,
    pub badges: SharedBadges,
    pub custom_badges: crate::badges::SharedBadgeCache,
    pub custom_css: crate::serve::SharedCss,
    pub emotes: crate::emotes::SharedEmotes,
    pub filters: crate::filters::SharedCompiled,
    pub feed_swap: mpsc::UnboundedSender<String>,
    pub fe_watch: mpsc::UnboundedSender<Option<PathBuf>>,
    pub rebind: Arc<Notify>,
    /// Generation bumped on channel swap: the reward-info/coin refresher
    /// and EventSub re-target immediately (watch = multi-consumer, no
    /// lost wakeups, unlike Notify's single permit).
    pub channel_changed: watch::Sender<std::time::Instant>,
}

/// Watch the config's parent directory (atomic-save editors REPLACE the
/// file; a directory watch survives that) filtered on the file name.
/// SIGHUP triggers the same pipeline manually.
pub(crate) fn spawn_watcher(config_path: PathBuf, ctx: ReloadCtx, quit: CancellationToken) {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<()>();

    // Bridge thread: owns the debouncer (it stops on Drop) and forwards
    // matching events into async-land.
    {
        let watch_file = config_path.clone();
        let file_name = watch_file
            .file_name()
            .map(std::ffi::OsStr::to_os_string)
            .unwrap_or_default();
        let dir: PathBuf = watch_file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        std::thread::spawn(move || {
            let (std_tx, std_rx) = std::sync::mpsc::channel();
            let mut debouncer =
                match notify_debouncer_full::new_debouncer(DEBOUNCE, None, move |res| {
                    let _ = std_tx.send(res);
                }) {
                    Ok(d) => d,
                    Err(e) => {
                        warn!(error = %e, "config watcher unavailable — SIGHUP still works");
                        return;
                    }
                };
            if let Err(e) = debouncer.watch(&dir, notify::RecursiveMode::NonRecursive) {
                warn!(dir = %dir.display(), error = %e, "cannot watch config directory");
                return;
            }
            debug!(path = %watch_file.display(), "watching for config changes");
            for res in std_rx {
                match res {
                    Ok(events) => {
                        if batch_touches_config(&events, &file_name, &watch_file) {
                            let _ = event_tx.send(());
                        }
                    }
                    Err(e) => warn!(errors = ?e, "config watch error"),
                }
            }
        });
    }

    tokio::spawn(async move {
        // SIGHUP where available; absent platform = never-firing future.
        #[cfg(unix)]
        let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .map(Some)
            .unwrap_or_else(|e| {
                warn!(error = %e, "SIGHUP handler unavailable");
                None
            });

        let mut watcher_alive = true;

        loop {
            // `watcher_alive`: when the bridge thread exits (watcher setup
            // failed, dir deleted, permission change) it drops event_tx,
            // making event_rx.recv() return None instantly — without this
            // guard the select would re-arm on None every poll and busy-
            // spin `apply_reload` on a core forever. We latch to SIGHUP-
            // only instead.
            let event_fut = async {
                if watcher_alive {
                    event_rx.recv().await
                } else {
                    Some(std::future::pending::<()>().await)
                }
            };
            #[cfg(unix)]
            {
                let hup_fut = async {
                    match hangup.as_mut() {
                        Some(h) => {
                            let _ = h.recv().await;
                            info!("SIGHUP received");
                        }
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    _ = quit.cancelled() => return,
                    event = event_fut => {
                        if watcher_alive && event.is_none() {
                            warn!("config watcher exited — falling back to SIGHUP-only reloads");
                            watcher_alive = false;
                        }
                    }
                    _ = hup_fut => (),
                }
            }
            #[cfg(not(unix))]
            tokio::select! {
                _ = quit.cancelled() => return,
                event = event_fut => {
                    if watcher_alive && event.is_none() {
                        warn!("config watcher exited — file reloads disabled");
                        watcher_alive = false;
                    }
                }
            }

            apply_reload(&config_path, &ctx).await;
        }
    });
}

async fn apply_reload(path: &Path, ctx: &ReloadCtx) {
    // 1. Load. Any failure rejects the whole reload.
    let new_cfg: ChatConfig = match quiver_config::load_from_path(path) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "config reload rejected (parse failed) — keeping current config");
            return;
        }
    };

    // 2. Semantic validation.
    let issues = new_cfg.validate();
    if !issues.is_empty() {
        let summary: Vec<String> = issues.iter().map(|i| i.to_string()).collect();
        warn!(
            issues = ?summary,
            "config reload rejected (validation failed) — keeping current config"
        );
        return;
    }

    // custom_css lint happens post-resolution (inline text or fetched
    // content); role_css still lints from raw config.
    crate::config::report_css_lint(None, new_cfg.theme.role_css.as_ref());

    // 3. Diff against what is running.
    let old_live = match ctx.live.read() {
        Ok(l) => l.clone(),
        Err(_) => return,
    };
    let new_live = LiveConfig::from(&new_cfg);
    let actions = planned_actions(&old_live, &new_live);
    if actions.is_empty() {
        debug!("config unchanged");
        return;
    }

    // 4. Pre-flight: reject invalid swap targets BEFORE touching anything.
    if let Some(Action::SwapChannel(ch)) =
        actions.iter().find(|a| matches!(a, Action::SwapChannel(_)))
        && let Err(e) = twitch_irc_validate_channel(ch)
    {
        warn!(
            channel = %ch,
            error = %e,
            "config reload rejected (invalid channel login) — keeping current config"
        );
        return;
    }
    // Same contract for filters: a pattern that cannot compile rejects the
    // whole reload — moderation must never silently stop working.
    if actions.contains(&Action::ApplyFilters)
        && let Err(e) = crate::filters::CompiledFilters::compile(&new_live.filters)
    {
        warn!(
            error = %e,
            "config reload rejected (filters failed to compile) — keeping current config"
        );
        return;
    }

    // 5. Publish the new live view FIRST so every reader observes the new
    //    config atomically. This is what makes a reload atomic for readers:
    //    pump's channel gate (engine.rs), message lifetime, and the HTTP
    //    layer's listen/widget_dist reads all see the new config from here
    //    on. Swapping only at the end left a mixed-state window spanning the
    //    network-awaited actions below (RefreshEmotes/RefreshBadges/
    //    RefreshCss/ResolveBadges can take seconds), during which pump's
    //    expected_channel gate still read the OLD login and dropped the new
    //    channel's messages, and a Rebind notify could fire before live held
    //    the new listen address.
    if let Ok(mut live) = ctx.live.write() {
        *live = new_live.clone();
    } else {
        warn!("config reload aborted (live lock poisoned) — keeping current config");
        return;
    }

    // 6. Apply in canonical order (see planned_actions docs). Store writes
    //    and the feed swap now run against the already-published config; the
    //    SwapChannel action follows within microseconds (SetMax/SetWidgetDist
    //    are non-blocking), so the straggler gate at most drops the old
    //    channel for a tiny window instead of the new channel for seconds.
    for action in &actions {
        apply_action(ctx, action, &new_live).await;
    }

    info!(actions = ?actions, "configuration reloaded");
}

fn twitch_irc_validate_channel(login: &str) -> Result<(), String> {
    quiver_twitch::IrcChatSource::validate_channel_login(login).map_err(|e| e.to_string())
}

/// Frontend-watcher bridge: re-target the watched directory.
enum BridgeMsg {
    SetDir(Option<PathBuf>),
}

/// Watch the widget frontend directory; any change broadcasts ONE
/// coalesced `{type:"reload"}` frame per interval so connected pages
/// refresh themselves. Re-targetable at runtime via control messages
/// (config hot reload may move `server.widget_dist`).
pub(crate) fn spawn_frontend_watcher(
    initial_dir: Option<PathBuf>,
    mut ctrl_rx: mpsc::UnboundedReceiver<Option<PathBuf>>,
    tx: tokio::sync::broadcast::Sender<String>,
) {
    const MIN_INTERVAL: Duration = Duration::from_secs(1);

    // Trigger channel from the bridge thread into async-land.
    let (trig_tx, mut trig_rx) = mpsc::unbounded_channel::<()>();
    let (bridge_tx, bridge_rx) = std::sync::mpsc::channel::<BridgeMsg>();

    // Std thread owns the PollWatchers for the CURRENT dir. Watchers are
    // dropped (and stop watching) whenever the target changes.
    //
    // Raw PollWatcher, no debouncer layer: polling coalesces naturally and
    // the async side rate-limits frames; Debouncer(PollWatcher)+cache proved
    // unreliable on macOS while raw PollWatcher+Recursive works.
    {
        let trig_tx = trig_tx.clone();
        std::thread::spawn(move || {
            // Alive == watching. Underscore: never read, lifetime matters.
            let mut _watchers: Vec<notify::PollWatcher> = Vec::new();
            loop {
                match bridge_rx.recv() {
                    Ok(BridgeMsg::SetDir(dir)) => {
                        _watchers.clear();
                        let Some(d) = dir.filter(|d| d.is_dir()) else {
                            continue;
                        };
                        let tx2 = trig_tx.clone();
                        // Deterministic sub-second latency beats battery here:
                        // the dir is a handful of files and devs want instant
                        // reloads. FSEvents latency measured at 3-7s on this box.
                        let cfg = notify::Config::default()
                            .with_poll_interval(Duration::from_millis(300));
                        match notify::PollWatcher::new(
                            move |res: std::result::Result<notify::Event, notify::Error>| {
                                if res.is_ok() {
                                    let _ = tx2.send(());
                                }
                            },
                            cfg,
                        ) {
                            Ok(mut w) => match w.watch(&d, notify::RecursiveMode::Recursive) {
                                Ok(()) => {
                                    debug!(dir = %d.display(), "watching widget frontend");
                                    _watchers.push(w);
                                }
                                Err(e) => {
                                    warn!(dir = %d.display(), error = %e, "cannot watch widget dir")
                                }
                            },
                            Err(e) => warn!(error = %e, "frontend watcher unavailable"),
                        }
                    }
                    Err(_) => return, // controller dropped: shutdown
                }
            }
        });
    }

    tokio::spawn(async move {
        // Initial target.
        let _ = bridge_tx.send(BridgeMsg::SetDir(initial_dir));

        // Coalescing loop: at most one reload frame per MIN_INTERVAL, but a
        // trigger suppressed inside the window must be CATCH-UP'd — bundle
        // writes land within the same second and the trailing write must
        // not be dropped forever (the old code discarded it and only a
        // later write after the window would re-arm the reload).
        let mut last_sent: Option<tokio::time::Instant> = None;
        // Trailing-catch-up deadline: armed when a trigger is suppressed,
        // fires one MIN_INTERVAL after the last send.
        let mut until: Option<tokio::time::Instant> = None;
        loop {
            // Copy so the trailing arm can own it (Instant is Copy); set
            // in the trigger arm below for the NEXT loop iteration.
            let until_snapshot = until;
            tokio::select! {
                msg = ctrl_rx.recv() => {
                    // None = all senders dropped (shutdown).
                    match msg {
                        Some(dir) => { let _ = bridge_tx.send(BridgeMsg::SetDir(dir)); }
                        None => return,
                    }
                }
                _ = trig_rx.recv() => {
                    let now = tokio::time::Instant::now();
                    let due = match last_sent {
                        None => true,
                        Some(t) => now.duration_since(t) >= MIN_INTERVAL,
                    };
                    if due {
                        last_sent = Some(now);
                        until = None;
                        info!("widget frontend changed — reloading connected pages");
                        let _ = tx.send(r#"{"type":"reload"}"#.to_string());
                    } else {
                        // Suppressed inside the window: guarantee a trailing
                        // send one interval after the last one instead of
                        // dropping the change entirely.
                        if until.is_none() {
                            until = Some(last_sent.unwrap() + MIN_INTERVAL);
                        }
                    }
                }
                _ = async {
                    match until_snapshot {
                        Some(u) => tokio::time::sleep_until(u).await,
                        None => std::future::pending::<()>().await,
                    }
                }, if until_snapshot.is_some() => {
                    until = None;
                    last_sent = Some(tokio::time::Instant::now());
                    info!("widget frontend changed — reloading connected pages (trailing)");
                    let _ = tx.send(r#"{"type":"reload"}"#.to_string());
                }
            }
        }
    });
}

async fn apply_action(ctx: &ReloadCtx, action: &Action, new_live: &LiveConfig) {
    match action {
        Action::SetMax(n) => {
            let evicted = ctx
                .messages
                .lock()
                .map(|mut m| m.set_max(*n as usize))
                .unwrap_or_default();
            if !evicted.is_empty() {
                let frame = serde_json::json!({ "type": "expire", "ids": evicted });
                let _ = ctx.tx.send(frame.to_string());
            }
        }
        Action::SetWidgetDist(dir) => {
            // Re-target the frontend watcher; None disables it.
            let _ = ctx.fe_watch.send(dir.clone());
        }
        Action::SwapChannel(channel) => {
            // Supervisor parts/joins and broadcasts {"type":"clear"}.
            let _ = ctx.feed_swap.send(channel.clone());
            // Reward-info/coin refresher + EventSub re-target immediately
            // (multi-consumer watch — every consumer wakes).
            let _ = ctx.channel_changed.send(std::time::Instant::now());
        }
        Action::RefreshBadges => match &new_live.creds {
            Some((id, secret)) => {
                match crate::serve::load_badge_map(id, secret, &new_live.channel).await {
                    Ok(map) => {
                        info!(badge_count = map.len(), "badge map refreshed");
                        if let Ok(mut b) = ctx.badges.write() {
                            *b = map;
                        }
                    }
                    // Keep the previous map: overwriting a healthy one with an
                    // empty result turns a transient Helix failure into
                    // permanent badge-less widgets.
                    Err(e) => {
                        warn!(error = %e, "badge refresh failed — keeping previous badges");
                    }
                }
            }
            None => {
                if let Ok(mut b) = ctx.badges.write() {
                    b.clear();
                }
            }
        },
        Action::RefreshEmotes => {
            let map = crate::emotes::load_third_party_emotes(
                new_live
                    .creds
                    .as_ref()
                    .map(|(a, b)| (a.as_str(), b.as_str())),
                &new_live.channel,
            )
            .await;
            info!(emote_count = map.len(), "third-party emote map refreshed");
            if let Ok(mut e) = ctx.emotes.write() {
                *e = map;
            }
        }
        Action::ApplyFilters => match crate::filters::CompiledFilters::compile(&new_live.filters) {
            Ok(f) => {
                if let Ok(mut g) = ctx.filters.write() {
                    *g = Some(f);
                }
                info!("filters applied");
            }
            Err(e) => warn!(error = %e, "filter compile failed at apply — keeping old filters"),
        },
        Action::RefreshCss => {
            // Re-resolve the custom CSS source (inline / file / http) so a
            // config edit that switches sources takes effect immediately.
            // Thread the configured badge cache_dir so an http css source
            // keeps sharing it (incl. the user's override).
            let cache_dir = new_live
                .badges
                .as_ref()
                .and_then(|b| b.cache_dir.clone())
                .unwrap_or_else(crate::badges::default_cache_dir);
            let css = crate::serve::resolve_custom_css(
                &new_live.theme.custom_css,
                &crate::serve::http_client(),
                &cache_dir,
            )
            .await;
            if let Ok(mut c) = ctx.custom_css.write() {
                *c = css;
            }
            info!("custom css source resolved");
        }
        Action::BroadcastMeta => {
            // Build from new_live explicitly — it is the source of truth for
            // this reload (live is published before actions run). The store
            // snapshots (badges/custom_css) are read for the meta payload.
            let badges = ctx.badges.read().map(|b| b.clone()).unwrap_or_default();
            let custom = ctx
                .custom_badges
                .read()
                .ok()
                .and_then(|g| g.as_ref().map(|c| c.resolved.clone()));
            let css = ctx.custom_css.read().ok().and_then(|c| c.clone());
            let frame = serde_json::json!({
                "type": "config",
                "meta": crate::serve::meta_value(new_live, &badges, custom.as_ref(), css.as_deref()),
            });
            let _ = ctx.tx.send(frame.to_string());
        }
        Action::ResolveBadges => match &new_live.badges {
            Some(badge_cfg) => {
                let http = crate::serve::http_client();
                match crate::badges::resolve_full(badge_cfg, &http).await {
                    Ok(state) => {
                        if let Ok(mut g) = ctx.custom_badges.write() {
                            *g = Some(state);
                        }
                        info!("custom badge cache re-resolved");
                    }
                    Err(e) => {
                        warn!(error = %e, "custom badge re-resolution failed — keeping old cache");
                    }
                }
            }
            None => {
                if let Ok(mut g) = ctx.custom_badges.write() {
                    *g = None;
                }
                info!("custom badges removed");
            }
        },
        Action::Rebind => ctx.rebind.notify_one(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::Event;
    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, ModifyKind, RemoveKind,
        RenameMode,
    };
    use std::time::Instant;

    fn watch_file() -> PathBuf {
        PathBuf::from("cfg").join("chat.ron")
    }

    fn ev(kind: EventKind, path: &Path) -> DebouncedEvent {
        DebouncedEvent::new(
            Event::new(kind).add_path(path.to_path_buf()),
            Instant::now(),
        )
    }

    /// The reload path re-reads the config on every apply, which notify
    /// reports as `Access`. A batch of reads must not schedule a reload —
    /// otherwise each reload feeds the next and the watcher never idles.
    #[test]
    fn config_watch_ignores_reads() {
        let f = watch_file();
        let name = f.file_name().unwrap();

        for kind in [
            EventKind::Access(AccessKind::Open(AccessMode::Any)),
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            EventKind::Access(AccessKind::Any),
        ] {
            assert!(
                !batch_touches_config(&[ev(kind, &f)], name, &f),
                "{kind:?} on the config must not trigger a reload"
            );
        }
    }

    /// Real changes still reload — otherwise hot reload itself is broken.
    #[test]
    fn config_watch_reacts_to_changes() {
        let f = watch_file();
        let name = f.file_name().unwrap();

        for kind in [
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
            EventKind::Create(CreateKind::File),
            EventKind::Remove(RemoveKind::File),
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
        ] {
            assert!(
                batch_touches_config(&[ev(kind, &f)], name, &f),
                "{kind:?} on the config must trigger a reload"
            );
        }
    }

    /// Unrelated files in the same directory stay ignored, and a read
    /// batched with a change to another file is not a config change.
    #[test]
    fn config_watch_ignores_other_files() {
        let f = watch_file();
        let name = f.file_name().unwrap();
        let other = PathBuf::from("cfg").join("oauth.json");

        assert!(!batch_touches_config(
            &[ev(
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                &other
            )],
            name,
            &f
        ));
        assert!(!batch_touches_config(
            &[
                ev(EventKind::Access(AccessKind::Open(AccessMode::Any)), &f),
                ev(EventKind::Modify(ModifyKind::Data(DataChange::Any)), &other),
            ],
            name,
            &f
        ));
    }

    /// A read batched with a real change still reloads — the debouncer
    /// coalesces both into one batch, and dropping the batch would lose
    /// the change.
    #[test]
    fn config_watch_change_survives_batched_read() {
        let f = watch_file();
        let name = f.file_name().unwrap();

        assert!(batch_touches_config(
            &[
                ev(EventKind::Access(AccessKind::Open(AccessMode::Any)), &f),
                ev(EventKind::Modify(ModifyKind::Data(DataChange::Any)), &f),
            ],
            name,
            &f
        ));
    }
}
