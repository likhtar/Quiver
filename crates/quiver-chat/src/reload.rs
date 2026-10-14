//! Config hot reload: file watching + SIGHUP → load/validate/diff/apply.
//!
//! Contract: a reload is all-or-nothing. Parse errors, validation issues,
//! or an invalid channel login REJECT the whole reload — the running
//! config stays untouched. There is no half-applied state, ever.

use notify::{EventKind, Watcher as _};
use notify_debouncer_full::{DebounceEventResult, DebouncedEvent};
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

/// The debounced event result a `notify-debouncer-full` handler receives.
type FsBatch = DebounceEventResult;

/// A debouncer using the runtime-recommended watcher + cache.
type Debouncer = notify_debouncer_full::Debouncer<
    notify::RecommendedWatcher,
    notify_debouncer_full::RecommendedCache,
>;

/// Messages the bridge thread handles: filesystem batches and watch-target
/// updates (a reload can add/remove the credentials file or repoint a
/// `file://` custom CSS source).
enum WatchMsg {
    Fs(FsBatch),
    Targets(Vec<PathBuf>),
}

/// Does this debounced batch touch ANY watched file?
///
/// Matching the watched file name is not enough on its own. The reload path
/// reads the config back (`quiver_config::load_from_path`), and that read
/// raises `IN_OPEN` on the very file we watch, which notify reports as
/// `EventKind::Access`. Treating a read as a change makes every reload
/// schedule the next one, so a single save spins the watcher at the debounce
/// rate until the process restarts. Only change-producing kinds may pass.
/// The same guard covers the sidecar files (separate credentials file,
/// `file://` CSS) we now also watch AND read back on every reload.
fn batch_touches_any(events: &[DebouncedEvent], targets: &[PathBuf]) -> bool {
    events.iter().any(|e| {
        if matches!(e.kind, EventKind::Access(_)) {
            return false;
        }
        e.paths
            .iter()
            .any(|p| targets.iter().any(|t| path_matches(p, t)))
    })
}

/// Does `p` (as reported by notify) refer to the watched file `t`? Exact
/// path, or same parent + same file name — notify sometimes reports a
/// sibling spelling for atomic-save renames.
fn path_matches(p: &Path, t: &Path) -> bool {
    p == t || (p.file_name() == t.file_name() && p.parent() == t.parent())
}

/// Unique parent directories of the watched files — one `watch()` per dir.
/// Atomic-save editors REPLACE the file (temp + rename); a directory watch
/// survives that.
fn watch_dirs(targets: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = targets
        .iter()
        .map(|t| {
            t.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        })
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

/// (Re)build the debouncer and watch every directory holding a target.
fn install_watch(
    debouncer: &mut Option<Debouncer>,
    targets: &[PathBuf],
    fs_tx: &std::sync::mpsc::Sender<WatchMsg>,
) {
    let tx = fs_tx.clone();
    let mut d = match notify_debouncer_full::new_debouncer(DEBOUNCE, None, move |res| {
        let _ = tx.send(WatchMsg::Fs(res));
    }) {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "config watcher unavailable — SIGHUP still works");
            *debouncer = None;
            return;
        }
    };
    let dirs = watch_dirs(targets);
    for dir in &dirs {
        if let Err(e) = d.watch(dir, notify::RecursiveMode::NonRecursive) {
            warn!(dir = %dir.display(), error = %e, "cannot watch config directory");
        }
    }
    if !dirs.is_empty() {
        debug!(dirs = ?dirs, "watching for config changes");
    }
    *debouncer = Some(d);
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
    /// Re-target the config/credentials/CSS watch set after every reload.
    pub watch_targets: mpsc::UnboundedSender<Vec<PathBuf>>,
    pub rebind: Arc<Notify>,
    /// Generation bumped on channel swap: the reward-info/coin refresher
    /// and EventSub re-target immediately (watch = multi-consumer, no
    /// lost wakeups, unlike Notify's single permit).
    pub channel_changed: watch::Sender<std::time::Instant>,
}

/// Watch the config file plus its sidecars — the optional separate
/// credentials file and a `file://` custom CSS source. Editors atomic-save
/// by REPLACE, so we watch the parent directory of each target and filter on
/// the file identity. The watch set is re-targetable: every reload re-derives
/// it (a reload can add/remove the credentials file or repoint the CSS
/// source). SIGHUP triggers the same pipeline manually where available.
pub(crate) fn spawn_watcher(
    config_path: PathBuf,
    initial_targets: Vec<PathBuf>,
    mut targets_rx: mpsc::UnboundedReceiver<Vec<PathBuf>>,
    ctx: ReloadCtx,
    quit: CancellationToken,
) {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<()>();

    // Bridge thread: owns the debouncer (it stops on Drop) and forwards
    // matching events into async-land. Watch-target updates arrive on the
    // same std channel (via a forwarder thread) so the bridge blocks on one
    // receiver only.
    std::thread::spawn(move || {
        let (fs_tx, std_rx) = std::sync::mpsc::channel::<WatchMsg>();

        // Forward control messages from async-land into the std channel.
        {
            let fs_tx = fs_tx.clone();
            std::thread::spawn(move || {
                while let Some(targets) = targets_rx.blocking_recv() {
                    if fs_tx.send(WatchMsg::Targets(targets)).is_err() {
                        break;
                    }
                }
            });
        }

        let mut targets = initial_targets;
        let mut debouncer: Option<Debouncer> = None;
        install_watch(&mut debouncer, &targets, &fs_tx);

        for msg in std_rx {
            match msg {
                WatchMsg::Fs(Ok(events)) => {
                    if batch_touches_any(&events, &targets) {
                        let _ = event_tx.send(());
                    }
                }
                WatchMsg::Fs(Err(errs)) => warn!(errors = ?errs, "config watch error"),
                WatchMsg::Targets(new_targets) => {
                    targets = new_targets;
                    install_watch(&mut debouncer, &targets, &fs_tx);
                }
            }
        }
    });

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
    // 1. Load. Any failure rejects the whole reload. Uses the merging
    //    loader so an external credentials file counts as a config input.
    let new_cfg: ChatConfig = match crate::config::load(path) {
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

    // Re-derive the watch set: this reload may have added/removed the
    // credentials file, or repointed custom_css at a different file:// source.
    let _ = ctx
        .watch_targets
        .send(crate::config::watch_targets(&new_cfg, path));

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

        for kind in [
            EventKind::Access(AccessKind::Open(AccessMode::Any)),
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            EventKind::Access(AccessKind::Any),
        ] {
            assert!(
                !batch_touches_any(&[ev(kind, &f)], std::slice::from_ref(&f)),
                "{kind:?} on the config must not trigger a reload"
            );
        }
    }

    /// Real changes still reload — otherwise hot reload itself is broken.
    #[test]
    fn config_watch_reacts_to_changes() {
        let f = watch_file();

        for kind in [
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
            EventKind::Create(CreateKind::File),
            EventKind::Remove(RemoveKind::File),
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
        ] {
            assert!(
                batch_touches_any(&[ev(kind, &f)], std::slice::from_ref(&f)),
                "{kind:?} on the config must trigger a reload"
            );
        }
    }

    /// Unrelated files in the same directory stay ignored, and a read
    /// batched with a change to another file is not a config change.
    #[test]
    fn config_watch_ignores_other_files() {
        let f = watch_file();
        let other = PathBuf::from("cfg").join("oauth.json");

        assert!(!batch_touches_any(
            &[ev(
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                &other
            )],
            std::slice::from_ref(&f)
        ));
        assert!(!batch_touches_any(
            &[
                ev(EventKind::Access(AccessKind::Open(AccessMode::Any)), &f),
                ev(EventKind::Modify(ModifyKind::Data(DataChange::Any)), &other),
            ],
            std::slice::from_ref(&f)
        ));
    }

    /// A read batched with a real change still reloads — the debouncer
    /// coalesces both into one batch, and dropping the batch would lose
    /// the change.
    #[test]
    fn config_watch_change_survives_batched_read() {
        let f = watch_file();

        assert!(batch_touches_any(
            &[
                ev(EventKind::Access(AccessKind::Open(AccessMode::Any)), &f),
                ev(EventKind::Modify(ModifyKind::Data(DataChange::Any)), &f),
            ],
            std::slice::from_ref(&f)
        ));
    }

    /// Sidecar targets (separate credentials file, a file:// CSS source)
    /// trigger the same reload; a read of a sidecar must still be ignored so
    /// the reload that reads it back does not feed itself.
    #[test]
    fn sidecar_targets_trigger_and_ignore_reads() {
        let cfg = watch_file();
        let creds = PathBuf::from("cfg").join("credentials.ron");
        let css = PathBuf::from("cfg").join("custom-widget.css");
        let targets = vec![cfg.clone(), creds.clone(), css.clone()];

        for t in [&creds, &css] {
            assert!(batch_touches_any(
                &[ev(EventKind::Modify(ModifyKind::Data(DataChange::Any)), t)],
                &targets
            ));
            assert!(!batch_touches_any(
                &[ev(EventKind::Access(AccessKind::Read), t)],
                &targets
            ));
        }
    }

    /// `watch_dirs` dedupes parent directories across targets and sorts them.
    #[test]
    fn watch_dirs_dedupes_and_sorts() {
        let targets = vec![
            PathBuf::from("cfg").join("chat.ron"),
            PathBuf::from("cfg").join("credentials.ron"),
            PathBuf::from("css").join("custom-widget.css"),
        ];
        assert_eq!(
            watch_dirs(&targets),
            vec![PathBuf::from("cfg"), PathBuf::from("css")]
        );
    }

    /// Exact target path matches; same file name in a different directory
    /// does not.
    #[test]
    fn path_matches_requires_same_location() {
        let t = PathBuf::from("cfg").join("chat.ron");
        assert!(path_matches(&PathBuf::from("cfg").join("chat.ron"), &t));
        assert!(!path_matches(&PathBuf::from("cfg").join("other.ron"), &t));
        assert!(!path_matches(&PathBuf::from("other").join("chat.ron"), &t));
    }
}
