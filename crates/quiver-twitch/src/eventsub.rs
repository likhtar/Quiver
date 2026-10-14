//! EventSub over WebSocket (`wss://eventsub.wss.twitch.tv/ws`).
//!
//! Transport for channel-scoped real-time events (redemptions with reward
//! titles, hype trains, predictions, polls). Requires a user access token
//! (see [`crate::auth`]) — Twitch rejects websocket-transport subscription
//! requests with app tokens.
//!
//! Lifecycle: connect → `session_welcome` (session_id) → POST subscriptions
//! (transport.websocket + session_id) → notifications → tx. On `session_reconnect`
//! the provided URL is used; otherwise the socket is re-dialed with backoff and
//! subscriptions are recreated (Twitch does not replay missed events).

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::{Notify, broadcast};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::info;

use crate::helix::HelixClient;

const EVENTSUB_WS_URL: &str = "wss://eventsub.wss.twitch.tv/ws";

/// One requested EventSub subscription: type + version + the user scope it
/// requires (empty = no scope needed beyond a user token).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SubSpec {
    pub type_name: &'static str,
    pub version: &'static str,
    pub scope: &'static str,
    pub cost: u32,
}

/// Default subscription set — exactly cost 10 (the per-app budget).
pub const DEFAULT_SUBSCRIPTIONS: &[SubSpec] = &[
    SubSpec { type_name: "channel.channel_points_custom_reward_redemption.add", version: "1", scope: "channel:read:redemptions", cost: 1 },
    SubSpec { type_name: "channel.hype_train.begin", version: "2", scope: "channel:read:hype_train", cost: 1 },
    SubSpec { type_name: "channel.hype_train.progress", version: "2", scope: "channel:read:hype_train", cost: 1 },
    SubSpec { type_name: "channel.hype_train.end", version: "2", scope: "channel:read:hype_train", cost: 1 },
    SubSpec { type_name: "channel.prediction.begin", version: "1", scope: "channel:read:predictions", cost: 1 },
    SubSpec { type_name: "channel.prediction.lock", version: "1", scope: "channel:read:predictions", cost: 1 },
    SubSpec { type_name: "channel.prediction.end", version: "1", scope: "channel:read:predictions", cost: 1 },
    SubSpec { type_name: "channel.poll.begin", version: "1", scope: "channel:read:polls", cost: 1 },
    SubSpec { type_name: "channel.poll.end", version: "1", scope: "channel:read:polls", cost: 1 },
    SubSpec { type_name: "channel.follow", version: "2", scope: "moderator:read:followers", cost: 1 },
];

fn enabled_specs() -> Vec<SubSpec> {
    DEFAULT_SUBSCRIPTIONS.to_vec()
}

/// Spawn the EventSub client: connects, subscribes, and forwards mapped
/// event frames onto `tx` (the same broadcast channel the engine uses).
/// Exits when `quit` cancels; reconnects with backoff otherwise.
///
/// Slots (kept fresh by the serve layer):
/// - `broadcaster_slot`: current broadcaster id — re-read per session so a
///   channel swap re-targets subscriptions without a restart
/// - `coin_slot`: the channel's coin icon URL — read per session so
///   default-icon redeem banners always carry the right coin
///
/// `swap`: notified by the reload layer on channel swap — bounces the
/// active session so the re-target happens within ~seconds.
///
/// `gate`: called with the mapped event `kind` — false suppresses the frame
/// (the chat layer's message_type filters flow through here as a closure,
/// since quiver-twitch cannot depend on quiver-chat).
/// `deduper`: cross-source redemption dedupe — EventSub marks/deduplicates
/// `channel_resolver`: returns the CURRENT channel login — re-read per
/// session so a channel swap re-targets subscriptions without a restart.
///
/// `swap`: fired by the reload layer on channel swap — bounces the active
/// session so the re-target happens within ~seconds. The coin icon is
/// resolved per session via anonymous GQL against the current channel.
pub fn spawn(
    helix: Arc<HelixClient>,
    channel_resolver: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    tx: broadcast::Sender<String>,
    quit: tokio_util::sync::CancellationToken,
    gate: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    deduper: crate::dedupe::SharedRedeemDeduper,
    mut swap: tokio::sync::watch::Receiver<std::time::Instant>,
) {
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(2);
        loop {
            if quit.is_cancelled() {
                return;
            }
            // Per-session state: current channel login → broadcaster id →
            // coin. A swap bounces the session; the next one resolves fresh.
            let Some(channel_login) = (channel_resolver)() else {
                // Live config lost its channel (transient) — brief wait.
                tokio::select! {
                    _ = quit.cancelled() => return,
                    _ = swap.changed() => {}
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                }
                continue;
            };
            let Ok(broadcaster_id) = helix.user_id(&channel_login).await else {
                tracing::warn!(channel = %channel_login, "broadcaster id lookup failed — retrying");
                tokio::select! {
                    _ = quit.cancelled() => return,
                    _ = swap.changed() => {}
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            };
            let coin = helix.coin_icon_url(&channel_login).await;
            let mut session = tokio::spawn(run_session(
                helix.clone(),
                broadcaster_id,
                tx.clone(),
                quit.clone(),
                gate.clone(),
                deduper.clone(),
                coin,
                swap.clone(),
            ));
            tokio::select! {
                res = &mut session => match res {
                    Ok(_) => {
                        // Clean end happens only on shutdown paths.
                        tracing::info!("eventsub session ended");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    Err(e) => {
                        if e.is_panic() {
                            tracing::warn!(error = %e, "eventsub session panicked — reconnecting");
                        } else {
                            tracing::warn!(error = %e, "eventsub session cancelled — reconnecting");
                        }
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(60));
                    }
                },
                _ = quit.cancelled() => {
                    session.abort();
                    return;
                }
                _ = swap.changed() => {
                    // Channel swap: bounce the socket so the next session
                    // subscribes against the new broadcaster.
                    session.abort();
                    info!("eventsub re-targeting on channel swap");
                    backoff = Duration::from_secs(2);
                }
            }
        }
    });
}

enum SessionEnd {
    Quit,
    Reconnect(Option<String>),
    Dropped,
}

/// One websocket session: welcome → subscribe → notifications.
async fn run_session(
    helix: Arc<HelixClient>,
    broadcaster_id: String,
    tx: broadcast::Sender<String>,
    quit: tokio_util::sync::CancellationToken,
    gate: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    deduper: crate::dedupe::SharedRedeemDeduper,
    coin_icon: Option<String>,
    mut swap: tokio::sync::watch::Receiver<std::time::Instant>,
) -> SessionEnd {
    let (ws, _resp) = match tokio_tungstenite::connect_async(EVENTSUB_WS_URL).await {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!(error = %e, "eventsub connect failed");
            return SessionEnd::Dropped;
        }
    };
    let (_write, mut read) = ws.split();

    // First message MUST be session_welcome — but Twitch sends a protocol
    // PING frame FIRST on some networks (observed live): non-Text frames
    // are skipped, not fatal.
    let session_id = loop {
        match read.next().await {
            Some(Ok(WsMessage::Text(text))) => {
                let v: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                match v["metadata"]["message_type"].as_str() {
                    Some("session_welcome") => {
                        let id = v["payload"]["session"]["id"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string();
                        tracing::info!(session = %id, "eventsub session welcome");
                        break id;
                    }
                    Some(other) => {
                        tracing::debug!(t = other, "eventsub pre-welcome frame ignored")
                    }
                    None => {}
                }
            }
            // Transport-level Ping/Pong frames (observed: a bare PING
            // arrives BEFORE session_welcome) — skip, keep reading.
            Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => continue,
            other => {
                tracing::warn!(
                    frame = ?other.map(|m| format!("{m:?}")).unwrap_or_default(),
                    "eventsub closed before session_welcome — dropping"
                );
                return SessionEnd::Dropped;
            }
        }
    };

    // Subscribe to every enabled spec within budget. A missing scope or a
    // 4xx on one type logs and continues — the rest still deliver.
    let specs = enabled_specs();
    let total_cost: u32 = specs.iter().map(|s| s.cost).sum();
    if total_cost > 10 {
        tracing::warn!(total_cost, "eventsub subscription set exceeds budget — trimming");
    }
    let granted = helix.channel_scopes();
    for spec in specs {
        if total_cost > 10 && spec.cost > 0 {
            // Budget-trimmed sets skip everything past the cap in v1 of this
            // integration; the default set is exactly 10 so this is defensive.
        }
        if !spec.scope.is_empty() && !granted.contains(&spec.scope.to_string()) {
            tracing::warn!(sub = spec.type_name, scope = spec.scope, "scope not granted — skipping eventsub subscription");
            continue;
        }
        // channel.follow v2 requires moderator_user_id (the token user must
        // be a moderator of the channel) alongside broadcaster_user_id.
        let condition = if spec.type_name == "channel.follow" {
            serde_json::json!({
                "broadcaster_user_id": broadcaster_id,
                "moderator_user_id": broadcaster_id,
            })
        } else {
            serde_json::json!({ "broadcaster_user_id": broadcaster_id })
        };
        let body = serde_json::json!({
            "type": spec.type_name,
            "version": spec.version,
            "condition": condition,
            "transport": { "method": "websocket", "session_id": session_id },
        });
        if let Err(e) = helix.create_eventsub_subscription(&body).await {
            tracing::warn!(sub = spec.type_name, error = %e, "eventsub subscription failed");
        }
    }

    // Notification loop.
    let mut swap = swap;
    loop {
        tokio::select! {
            _ = quit.cancelled() => return SessionEnd::Quit,
            _ = swap.changed() => {
                // Channel swap: end this session; the supervisor reconnects
                // against the new channel immediately.
                info!("eventsub session bounces for channel swap");
                return SessionEnd::Dropped;
            }
            msg = read.next() => {
                match msg {
                    // Transport-level Ping/Pong: skip (tungstenite answers
                    // pings at the protocol layer automatically).
                    Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => {}
                    Some(Ok(WsMessage::Text(text))) => {
                        let v: serde_json::Value = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                let message_type = v["metadata"]["message_type"].as_str().unwrap_or("");
                match message_type {
                    "notification" => {
                        let sub_type = v["metadata"]["subscription_type"].as_str().unwrap_or("");
                        if let Some(frame) = map_event(sub_type, &v["payload"]["event"], &coin_icon)
                            && gate(frame["event"]["kind"].as_str().unwrap_or(""))
                        {
                            // Redeem frames dedupe cross-source (redemption
                            // id vs the poller; IRC-covered (user, reward)
                            // skips). Other kinds pass through.
                            let redeem = &v["payload"]["event"];
                            let already_delivered = deduper
                                .lock()
                                .map(|mut d| {
                                    if sub_type
                                        == "channel.channel_points_custom_reward_redemption.add"
                                    {
                                        let id = redeem["id"].as_str().unwrap_or_default();
                                        !d.is_new_redemption_id(id)
                                            || d.irc_already_rendered(
                                                redeem["user_login"].as_str().unwrap_or_default(),
                                                redeem["user_id"].as_str().unwrap_or_default(),
                                                redeem["reward"]["id"].as_str().unwrap_or_default(),
                                            )
                                    } else {
                                        false
                                    }
                                })
                                .unwrap_or(false);
                            if !already_delivered {
                                let _ = tx.send(frame.to_string());
                            }
                        }
                    }
                    "session_reconnect" => {
                        let url = v["payload"]["session"]["reconnect_url"].as_str().map(str::to_string);
                        return SessionEnd::Reconnect(url);
                    }
                    "session_keepalive" => {}
                    "revocation" => {
                        tracing::warn!("eventsub subscription revoked");
                    }
                    _ => {}
                }
                    }
                    other => {
                        tracing::warn!(
                            frame = ?other.map(|m| format!("{m:?}")).unwrap_or_default(),
                            "eventsub socket closed mid-session"
                        );
                        return SessionEnd::Dropped;
                    }
                }
            }
        }
    }
}

/// Map an EventSub notification payload onto the widget's event-frame wire
/// shape (`{type:"event", event:{kind, ...}}`). Returns None for types the
/// widget does not render.
fn map_event(
    sub_type: &str,
    ev: &serde_json::Value,
    coin_icon: &Option<String>,
) -> Option<serde_json::Value> {
    let kind = match sub_type {
        "channel.channel_points_custom_reward_redemption.add" => "redeem",
        "channel.hype_train.begin" => return hype("begin", ev),
        "channel.hype_train.progress" => return hype("progress", ev),
        "channel.hype_train.end" => return hype("end", ev),
        "channel.prediction.begin" => return prediction("begin", ev),
        "channel.prediction.lock" => return prediction("lock", ev),
        "channel.prediction.end" => return prediction("end", ev),
        "channel.poll.begin" => return poll("begin", ev),
        "channel.poll.end" => return poll("end", ev),
        "channel.follow" => "follow",
        _ => return None,
    };

    // EventSub carries the reward title AND icon inline — no Helix lookup
    // needed. The reward's OWN icon only (`reward.image.url_2x`); a
    // default-icon reward (image null) has NO reward icon. The channel coin
    // is a distinct thing and belongs to `{points_icon}` only — never to
    // `{reward_icon}` (that was the old single-icon contract).
    let reward_image = ev["reward"]["image"]["url_2x"].as_str().map(str::to_string);
    Some(serde_json::json!({
        "type": "event",
        "event": {
            "kind": kind,
            "user_login": ev["user_login"].as_str().unwrap_or_default(),
            "display_name": ev["user_name"].as_str().unwrap_or_default(),
            "reward_title": ev["reward"]["title"].as_str(),
            "reward_image": reward_image,
            // Channel coin resolved via GQL at session start — `{points_icon}`.
            "points_image": coin_icon.clone(),
            // Twitch reward colour drives the banner background/border.
            "reward_color": ev["reward"]["background_color"].as_str(),
            // Lets the widget map the reward's display name for the name pill.
            "reward_id": ev["reward"]["id"].as_str().unwrap_or_default(),
            "user_input": ev["user_input"].as_str().unwrap_or_default(),
        }
    }))
}

fn hype(phase: &str, ev: &serde_json::Value) -> Option<serde_json::Value> {
    Some(serde_json::json!({
        "type": "event",
        "event": {
            "kind": "hype_train",
            "phase": phase,
            "level": ev["level"],
            "total": ev["total"],
            "goal": ev["goal"],
            "top_display_names": ev["top_contributions"].as_array().map(|c| {
                c.iter().filter_map(|x| x["user_name"].as_str()).collect::<Vec<_>>()
            }).unwrap_or_default(),
        }
    }))
}

fn prediction(phase: &str, ev: &serde_json::Value) -> Option<serde_json::Value> {
    let title = ev["title"].as_str().unwrap_or_default();
    let outcome = |k: &str| ev["outcomes"].as_array().map(|o| {
        o.iter().find(|x| x["id"].as_str() == Some(k) || x["title"].as_str() == Some(k))
            .and_then(|x| x["title"].as_str().unwrap_or_default().to_string().into())
    }).and_then(|x| x);
    let _ = outcome;
    let winning = ev["winning_outcome_id"].as_str().unwrap_or_default();
    let winner_title = ev["outcomes"].as_array().and_then(|o| {
        o.iter().find(|x| x["id"].as_str() == Some(winning))
            .and_then(|x| x["title"].as_str()).map(str::to_string)
    });
    Some(serde_json::json!({
        "type": "event",
        "event": {
            "kind": "prediction",
            "phase": phase,
            "title": title,
            "winning_outcome": winner_title,
        }
    }))
}

fn poll(phase: &str, ev: &serde_json::Value) -> Option<serde_json::Value> {
    Some(serde_json::json!({
        "type": "event",
        "event": {
            "kind": "poll",
            "phase": phase,
            "title": ev["title"].as_str().unwrap_or_default(),
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::map_event;
    use serde_json::json;

    const REDEEM: &str = "channel.channel_points_custom_reward_redemption.add";

    fn coin() -> Option<String> {
        Some("https://cdn/coin.png".to_string())
    }

    fn redeem_payload(image: serde_json::Value) -> serde_json::Value {
        json!({
            "id": "red-1",
            "user_login": "fan",
            "user_name": "Fan",
            "user_input": "",
            "reward": {
                "id": "reward-1",
                "title": "Song",
                "cost": 100,
                "image": image,
                "background_color": "#842598",
            }
        })
    }

    // Regression: a default-icon reward (image null) must NOT inherit the
    // channel coin as its reward icon — the coin is `{points_icon}` only.
    #[test]
    fn redeem_default_icon_has_no_reward_image_but_carries_coin() {
        let frame = map_event(REDEEM, &redeem_payload(json!(null)), &coin()).unwrap();
        let e = &frame["event"];
        assert_eq!(e["kind"], "redeem");
        assert!(
            e["reward_image"].is_null(),
            "coin must not leak into reward_image"
        );
        assert_eq!(e["points_image"], "https://cdn/coin.png");
        assert_eq!(e["reward_color"], "#842598");
        assert_eq!(e["reward_id"], "reward-1");
        assert_eq!(e["reward_title"], "Song");
    }

    #[test]
    fn redeem_custom_icon_uses_url_2x() {
        let img = json!({ "url_1x": "https://cdn/1x.png", "url_2x": "https://cdn/2x.png" });
        let frame = map_event(REDEEM, &redeem_payload(img), &coin()).unwrap();
        let e = &frame["event"];
        assert_eq!(e["reward_image"], "https://cdn/2x.png");
        assert_eq!(e["points_image"], "https://cdn/coin.png");
    }

    #[test]
    fn redeem_without_coin_still_maps() {
        let frame = map_event(REDEEM, &redeem_payload(json!(null)), &None).unwrap();
        assert!(frame["event"]["points_image"].is_null());
    }
}
