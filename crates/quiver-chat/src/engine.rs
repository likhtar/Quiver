//! Render engine: turns chat events into widget render state and
//! broadcasts wire messages to connected browser clients.
//!
//! The engine owns the message LIFECYCLE (arrival, count cap, time-based
//! expiry) and the widget stays a dumb renderer: every state change is
//! announced as a wire frame so late joiners get correct snapshots.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::filters::MsgKind;
use quiver_twitch::{
    Badge, ChatMessage, CheerEvent, EmoteRef, Event, GifRef, GiftSubEvent, MessageDeleted,
    MysteryGiftEvent, RaidEvent, RedeemEvent, RewardInfo, SubEvent,
};
use serde::Serialize;
use tracing::{info, warn};

/// Rich chat events (subs/gifts/raids) on the wire.
/// Transient by design: announced once, never part of snapshots.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireEvent {
    Sub {
        user_login: String,
        display_name: String,
        is_resub: bool,
        tier: String,
        cumulative_months: u64,
        streak_months: Option<u64>,
        message: Option<String>,
    },
    GiftSub {
        gifter_login: Option<String>,
        gifter_display_name: Option<String>,
        recipient_login: String,
        recipient_display_name: String,
        tier: String,
        cumulative_months: u64,
        num_gifted_months: u64,
    },
    MysteryGift {
        gifter_login: Option<String>,
        gifter_display_name: Option<String>,
        mass_gift_count: u64,
        sender_total_gifts: Option<u64>,
        tier: String,
    },
    Raid {
        from_login: String,
        from_display_name: String,
        viewers: u64,
    },
    Redeem {
        user_login: String,
        display_name: String,
        /// Resolved reward title when channel OAuth exists; None → the
        /// widget renders a generic label.
        reward_title: Option<String>,
        /// Reward icon URL (Twitch coin icon) when resolvable; the widget
        /// falls back to a text icon.
        reward_image: Option<String>,
        /// Channel-points icon URL (the channel's coin), resolved
        /// anonymously via GQL. Used by the `{points_icon}` template token.
        points_image: Option<String>,
        reward_color: Option<String>,
        reward_id: String,
        user_input: String,
    },
    Cheer {
        user_login: String,
        display_name: String,
        bits: u64,
        message: String,
    },
}

impl From<SubEvent> for WireEvent {
    fn from(s: SubEvent) -> Self {
        Self::Sub {
            user_login: s.user_login,
            display_name: s.display_name,
            is_resub: s.is_resub,
            tier: s.tier,
            cumulative_months: s.cumulative_months,
            streak_months: s.streak_months,
            message: s.message,
        }
    }
}

impl From<GiftSubEvent> for WireEvent {
    fn from(g: GiftSubEvent) -> Self {
        Self::GiftSub {
            gifter_login: g.gifter_login,
            gifter_display_name: g.gifter_display_name,
            recipient_login: g.recipient_login,
            recipient_display_name: g.recipient_display_name,
            tier: g.tier,
            cumulative_months: g.cumulative_months,
            num_gifted_months: g.num_gifted_months,
        }
    }
}

impl From<MysteryGiftEvent> for WireEvent {
    fn from(m: MysteryGiftEvent) -> Self {
        Self::MysteryGift {
            gifter_login: m.gifter_login,
            gifter_display_name: m.gifter_display_name,
            mass_gift_count: m.mass_gift_count,
            sender_total_gifts: m.sender_total_gifts,
            tier: m.tier,
        }
    }
}

impl From<RaidEvent> for WireEvent {
    fn from(r: RaidEvent) -> Self {
        Self::Raid {
            from_login: r.from_login,
            from_display_name: r.from_display_name,
            viewers: r.viewers,
        }
    }
}

impl From<RedeemEvent> for WireEvent {
    fn from(r: RedeemEvent) -> Self {
        Self::Redeem {
            user_login: r.user_login,
            display_name: r.display_name,
            // Title/image resolution happens at emit time (Helix cache); the
            // untyped fallback renders as "a channel point reward".
            reward_title: None,
            reward_image: None,
            points_image: None,
            reward_color: None,
            reward_id: r.reward_id.clone(),
            user_input: r.user_input,
        }
    }
}

impl From<CheerEvent> for WireEvent {
    fn from(c: CheerEvent) -> Self {
        Self::Cheer {
            user_login: c.user_login,
            display_name: c.display_name,
            bits: c.bits,
            message: c.text,
        }
    }
}

/// Badge reference on the wire (`id`/`version` pair).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireBadge {
    pub id: String,
    pub version: String,
}

impl From<&Badge> for WireBadge {
    fn from(b: &Badge) -> Self {
        Self {
            id: b.id.clone(),
            version: b.version.clone(),
        }
    }
}

/// Emote position on the wire. `start`/`end` are RAW UTF-16 code-unit
/// offsets into `text` (Twitch's wire convention, no char normalization;
/// end EXCLUSIVE). The widget slices with JS `String.slice` — same unit
/// space — which is why placement works. NOT Rust char indices.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireEmote {
    pub id: String,
    pub start: usize,
    pub end: usize,
}

impl From<&EmoteRef> for WireEmote {
    fn from(e: &EmoteRef) -> Self {
        Self {
            id: e.id.clone(),
            start: e.start,
            end: e.end,
        }
    }
}

/// GIF position on the wire (Twitch GIF Keyboard). `start`/`end` are RAW
/// UTF-16 code-unit offsets into `text` (wire convention), end EXCLUSIVE —
/// same gotcha as WireEmote: NOT Rust char indices. URL is the signed
/// GIPHY media URL — render as-is.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireGif {
    pub id: String,
    pub start: usize,
    pub end: usize,
    pub url: String,
}

impl From<&GifRef> for WireGif {
    fn from(g: &GifRef) -> Self {
        Self {
            id: g.id.clone(),
            start: g.start,
            end: g.end,
            url: g.url.clone(),
        }
    }
}

/// Reply thread header data on the wire.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireReplyParent {
    pub message_id: String,
    pub user_login: String,
    pub display_name: String,
    pub text: String,
}

impl From<&quiver_twitch::ReplyParent> for WireReplyParent {
    fn from(rp: &quiver_twitch::ReplyParent) -> Self {
        Self {
            message_id: rp.message_id.clone(),
            user_login: rp.user_login.clone(),
            display_name: rp.display_name.clone(),
            text: rp.text.clone(),
        }
    }
}

/// One message as rendered by the widget.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RenderedMessage {
    pub id: String,
    pub user_login: String,
    /// Twitch numeric user id — needed for per-user custom badges.
    pub user_id: String,
    pub display_name: String,
    pub color: Option<String>,
    /// `/me` action message — rendered italic in the sender's color.
    pub is_action: bool,
    /// Present when this message replies to another; rendered as a compact
    /// header strip above the row content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<WireReplyParent>,
    pub text: String,
    pub emotes: Vec<WireEmote>,
    /// GIF positions; usually empty (backward-compatible wire).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub gifs: Vec<WireGif>,
    pub badges: Vec<WireBadge>,
}

impl From<ChatMessage> for RenderedMessage {
    fn from(cm: ChatMessage) -> Self {
        Self {
            id: cm.id,
            user_login: cm.user_login,
            user_id: cm.user_id.clone(),
            display_name: cm.display_name,
            color: cm.color,
            is_action: false, // twitch-irc strips /me markers; see TODO below
            reply_to: cm.reply_parent.as_ref().map(WireReplyParent::from),
            text: cm.text,
            emotes: cm.emotes.iter().map(WireEmote::from).collect(),
            gifs: cm.gifs.iter().map(WireGif::from).collect(),
            badges: cm.badges.iter().map(WireBadge::from).collect(),
        }
    }
}

// TODO(lifetime-followup): map PrivmsgMessage.is_action once it is carried
// through quiver-twitch's ChatMessage instead of dropped at the facade.

/// Bounded, age-aware message history.
#[derive(Debug)]
struct Enqueued {
    at: Instant,
    msg: RenderedMessage,
}

/// Shared handle used by the server and the pump task.
#[derive(Debug)]
pub struct EngineState {
    messages: VecDeque<Enqueued>,
    max_messages: usize,
}

impl EngineState {
    pub fn new(max_messages: usize) -> Self {
        Self {
            messages: VecDeque::new(),
            max_messages,
        }
    }

    /// Append a message; returns ids EVICTED by the count cap (oldest
    /// first) so they can be announced to clients as expired.
    pub fn push(&mut self, msg: RenderedMessage) -> Vec<String> {
        let mut evicted = Vec::new();
        while self.messages.len() >= self.max_messages {
            if let Some(old) = self.messages.pop_front() {
                evicted.push(old.msg.id);
            }
        }
        self.messages.push_back(Enqueued {
            at: Instant::now(),
            msg,
        });
        evicted
    }

    /// Drop messages older than `lifetime`; returns their ids oldest-first.
    pub fn sweep_expired(&mut self, now: Instant, lifetime: Duration) -> Vec<String> {
        let mut expired = Vec::new();
        while let Some(front) = self.messages.front() {
            if now.duration_since(front.at) >= lifetime {
                let old = self.messages.pop_front().expect("front existed");
                expired.push(old.msg.id);
            } else {
                break;
            }
        }
        expired
    }

    /// Apply a new max_messages cap. Shrinking evicts oldest immediately;
    /// returns their ids so they can be announced to clients.
    pub fn set_max(&mut self, max_messages: usize) -> Vec<String> {
        self.max_messages = max_messages;
        let mut evicted = Vec::new();
        while self.messages.len() > self.max_messages {
            if let Some(old) = self.messages.pop_front() {
                evicted.push(old.msg.id);
            }
        }
        evicted
    }

    /// Drop all history (channel switch); returns the removed ids.
    pub fn clear(&mut self) -> Vec<String> {
        self.messages.drain(..).map(|e| e.msg.id).collect()
    }

    /// Remove a single message by id; returns whether it was present.
    pub fn remove_by_id(&mut self, id: &str) -> bool {
        let before = self.messages.len();
        self.messages.retain(|e| e.msg.id != id);
        self.messages.len() != before
    }

    pub fn messages(&self) -> impl Iterator<Item = &RenderedMessage> {
        self.messages.iter().map(|e| &e.msg)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.messages.len()
    }
}

/// Shared handle used by the server and the pump task.
pub type SharedState = Arc<Mutex<EngineState>>;

/// reward_id -> reward display info (title + icon URL), resolved via Helix
/// when channel OAuth exists. Refreshed periodically by the serve layer; the
/// pump only reads it.
pub type SharedRewardTitles = Arc<RwLock<HashMap<String, RewardInfo>>>;

/// Cross-source redemption deduplication lives in quiver-twitch
/// ([`quiver_twitch::dedupe`]) so the EventSub client can share the same
/// ring. The IRC path NEVER suppresses its emissions — every redemption
/// message that arrives is its own redemption and must render.
pub type SharedRedeemDeduper = quiver_twitch::dedupe::SharedRedeemDeduper;

/// Single filter decision point for the pump. No compiled filters = pass.
fn permitted(
    filters: &crate::filters::SharedCompiled,
    kind: MsgKind,
    login: &str,
    display_name: &str,
    user_id: &str,
    badges: &[Badge],
    content: &str,
) -> bool {
    let Ok(guard) = filters.read() else {
        return true; // poisoned lock: fail-open rather than drop chat
    };
    match guard.as_ref() {
        Some(f) => f.permits(&crate::filters::PermitCtx {
            kind,
            login,
            display_name,
            user_id,
            badges,
            content,
        }),
        None => true,
    }
}

fn send_event<E: Into<WireEvent>>(tx: &tokio::sync::broadcast::Sender<String>, ev: E) {
    let wire: WireEvent = ev.into();
    let frame = serde_json::json!({ "type": "event", "event": wire });
    let _ = tx.send(frame.to_string());
}

/// Consume events from `source`, announce every state change on `tx`,
/// sweep expired messages every second. Returns when the source ends.
///
/// Reads `live` continuously: message lifetime and the expected channel
/// login are picked up on every tick/event, so config reloads take effect
/// without restarting the feed. Messages from other channels (stragglers
/// around a channel swap) are dropped silently.
pub async fn pump(
    live: crate::live::SharedLive,
    state: SharedState,
    tx: tokio::sync::broadcast::Sender<String>,
    filters: crate::filters::SharedCompiled,
    reward_titles: SharedRewardTitles,
    coin_icon: Arc<RwLock<Option<String>>>,
    reward_miss_notify: Arc<Notify>,
    redeem_deduper: SharedRedeemDeduper,
    source: &mut quiver_twitch::IrcChatSource,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            event = source.next_event() => match event {
                None => {
                    warn!("chat feed ended");
                    return;
                }
                Some(Event::ChatMessage(cm)) => {
                    let expected_channel = live.read().map(|l| l.channel.clone()).unwrap_or_default();
                    if cm.channel_login != expected_channel {
                        continue; // straggler from a swapped-away channel
                    }
                    // Command-style filtering is a content-regex concern
                    // (e.g. `^!`); every chat message is kind=message.
                    if !permitted(&filters, MsgKind::Message, &cm.user_login, &cm.display_name, &cm.user_id, &cm.badges, &cm.text) {
                        continue; // filtered: never reaches history or clients
                    }
                    let rendered = RenderedMessage::from(cm);
                    // Push to history AND build the wire frame under ONE
                    // lock section — snapshots (serve::snapshot_frame) hold
                    // the same lock, so "frame buffered in the broadcast
                    // channel" ⟺ "mutation already visible to a concurrent
                    // snapshot read". WS join/resync handlers rely on this
                    // invariant to drain buffered frames safely: a frame
                    // discarded at drain time is guaranteed in the snapshot.
                    let (evicted, frame) = state
                        .lock()
                        .map(|mut st| {
                            let evicted = st.push(rendered.clone());
                            let frame = serde_json::json!({ "type": "message", "message": rendered });
                            (evicted, frame.to_string())
                        })
                        .unwrap_or_default();
                    let _ = tx.send(frame);
                    if !evicted.is_empty() {
                        let frame = serde_json::json!({ "type": "expire", "ids": evicted });
                        let _ = tx.send(frame.to_string());
                    }
                }
                Some(Event::Sub(s)) => {
                    if permitted(&filters, MsgKind::Sub, &s.user_login, &s.display_name, "", &[], "") {
                        send_event(&tx, s);
                    }
                }
                Some(Event::GiftSub(g)) => {
                    if permitted(&filters, MsgKind::GiftSub, &g.recipient_login, &g.recipient_display_name, "", &[], "") {
                        send_event(&tx, g);
                    }
                }
                Some(Event::MysteryGift(m)) => {
                    if permitted(&filters, MsgKind::MysteryGift, "", "", "", &[], "") {
                        send_event(&tx, m);
                    }
                }
                Some(Event::MessageDeleted(MessageDeleted { message_id, .. })) => {
                    // Drop from history so future snapshots exclude it, and
                    // announce so connected clients hide it immediately.
                    if let Ok(mut st) = state.lock() {
                        st.remove_by_id(&message_id);
                    }
                    let frame = serde_json::json!({ "type": "delete", "id": message_id });
                    let _ = tx.send(frame.to_string());
                }
                Some(Event::ChatCleared) => {
                    // Whole chat cleared by a moderator: empty history and
                    // tell clients to wipe — same wire shape as a channel
                    // swap, which the widget already handles.
                    if let Ok(mut st) = state.lock() {
                        st.clear();
                    }
                    let _ = tx.send(r#"{"type":"clear"}"#.to_string());
                }
                Some(Event::Raid(r)) => {
                    if permitted(&filters, MsgKind::Raid, &r.from_login, &r.from_display_name, "", &[], "") {
                        send_event(&tx, r);
                    }
                }
                Some(Event::Redeem(r)) => {
                    // Redemption input is user text: treat it as content for
                    // content filters. The reward title resolves from the
                    // shared Helix cache when channel OAuth exists; a miss
                    // renders as the generic "channel point reward" label.
                    if permitted(&filters, MsgKind::Redeem, &r.user_login, &r.display_name, &r.user_id, &[], &r.user_input) {
                        // IRC ALWAYS renders a redemption — each message is
                        // its own redemption (rapid repeats included). We
                        // only MARK it so the poller/EventSub don't ALSO
                        // deliver the same redemption moments later.
                        let _ = redeem_deduper
                            .lock()
                            .map(|mut d| d.mark_irc(&r.user_login, &r.user_id, &r.reward_id));
                        let (reward_title, mut reward_image, reward_color) = reward_titles
                            .read()
                            .ok()
                            .and_then(|m| m.get(&r.reward_id).cloned())
                            .map(|info| (Some(info.title), info.image_url, info.background_color))
                            .unwrap_or((None, None, None));
                        // Cache miss = a reward added to the channel
                        // mid-stream: trigger the reactive refresher so the
                        // NEXT redemption carries fresh info.
                        if reward_title.is_none() {
                            reward_miss_notify.notify_one();
                        }
                        // Default-icon rewards: fall back to the channel coin
                        // (fetched anonymously via GQL by the serve layer).
                        let points_image = coin_icon
                            .read()
                            .ok()
                            .and_then(|c| c.clone());
                        if reward_image.is_none() {
                            reward_image = points_image.clone();
                        }
                        info!(
                            reward_cached = reward_titles
                                .read()
                                .map(|m| m.contains_key(&r.reward_id))
                                .unwrap_or(false),
                            reward_title = ?reward_title,
                            reward_image = ?reward_image,
                            "redeem frame emitted"
                        );
                        let mut wire = WireEvent::from(r);
                        if let WireEvent::Redeem {
                            reward_title: rt,
                            reward_image: ri,
                            points_image: pi,
                            reward_color: rc,
                            ..
                        } = &mut wire
                        {
                            *rt = reward_title;
                            *ri = reward_image;
                            *pi = points_image;
                            *rc = reward_color;
                        }
                        send_event(&tx, wire);
                    }
                }
                Some(Event::Cheer(c)) => {
                    // Cheer text is user content: content filters apply.
                    if permitted(
                        &filters,
                        MsgKind::Cheer,
                        &c.user_login,
                        &c.display_name,
                        &c.user_id,
                        &[],
                        &c.text,
                    ) {
                        send_event(&tx, c);
                    }
                }
                Some(_) => {}
            },
            _ = ticker.tick() => {
                let lifetime = live
                    .read()
                    .map(|l| Duration::from_secs(l.theme.message_lifetime_secs))
                    .unwrap_or(Duration::from_secs(60));
                let expired = state
                    .lock()
                    .map(|mut st| st.sweep_expired(Instant::now(), lifetime))
                    .unwrap_or_default();
                if !expired.is_empty() {
                    let frame = serde_json::json!({ "type": "expire", "ids": expired });
                    let _ = tx.send(frame.to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str) -> RenderedMessage {
        RenderedMessage {
            id: id.to_string(),
            user_login: "u".into(),
            user_id: "1".into(),
            display_name: "U".into(),
            color: None,
            is_action: false,
            reply_to: None,
            text: "t".into(),
            emotes: vec![],
            gifs: vec![],
            badges: vec![],
        }
    }

    /// Ground truth BY HAND:
    /// ages 130s / 65s / 5s with lifetime 60s → first two expired, last stays.
    #[test]
    fn sweep_expires_exactly_the_old_ones() {
        let now = Instant::now();
        let mut st = EngineState::new(10);
        for (id, age) in [("a", 130), ("b", 65), ("c", 5)] {
            st.push(msg(id));
            if let Some(front) = st.messages.back_mut() {
                front.at = now - Duration::from_secs(age);
            }
        }
        assert_eq!(st.len(), 3);

        let expired = st.sweep_expired(now, Duration::from_secs(60));
        assert_eq!(expired, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(st.len(), 1);
        assert_eq!(st.messages().next().map(|m| m.id.as_str()), Some("c"));
    }

    /// Ground truth BY HAND: max=2, pushing a third evicts exactly the first.
    #[test]
    fn push_evicts_oldest_beyond_cap() {
        let mut st = EngineState::new(2);
        assert!(st.push(msg("one")).is_empty());
        assert!(st.push(msg("two")).is_empty());
        let evicted = st.push(msg("three"));
        assert_eq!(evicted, vec!["one".to_string()]);
        assert_eq!(st.len(), 2);
        let ids: Vec<_> = st.messages().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["two", "three"]);
    }

    /// Ground truth BY HAND: shrinking 3→1 evicts exactly the two oldest.
    #[test]
    fn set_max_shrink_evicts_oldest_in_order() {
        let mut st = EngineState::new(3);
        for id in ["a", "b", "c"] {
            st.push(msg(id));
        }
        let evicted = st.set_max(1);
        assert_eq!(evicted, vec!["a".to_string(), "b".to_string()]);
        let ids: Vec<_> = st.messages().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["c"]);
    }

    /// Growing the cap evicts nothing.
    #[test]
    fn set_max_grow_keeps_everything() {
        let mut st = EngineState::new(2);
        for id in ["a", "b"] {
            st.push(msg(id));
        }
        assert!(st.set_max(5).is_empty());
        assert_eq!(st.len(), 2);
    }

    /// Ground truth BY HAND: clear removes everything, in order.
    #[test]
    fn clear_drops_all_and_reports_ids() {
        let mut st = EngineState::new(5);
        for id in ["x", "y"] {
            st.push(msg(id));
        }
        assert_eq!(st.clear(), vec!["x".to_string(), "y".to_string()]);
        assert_eq!(st.len(), 0);
    }

    /// Ground truth BY HAND: remove_by_id drops exactly one message by id.
    #[test]
    fn remove_by_id_deletes_the_matching_message() {
        let mut st = EngineState::new(5);
        for id in ["a", "b", "c"] {
            st.push(msg(id));
        }
        assert!(st.remove_by_id("b"));
        let ids: Vec<&str> = st.messages().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["a", "c"]);

        // Removing an absent id is a no-op returning false.
        assert!(!st.remove_by_id("z"));
        assert_eq!(st.len(), 2);
    }

    /// Ground truth BY HAND: wire frames are internally-tagged JSON.
    #[test]
    fn wire_events_serialize_with_kind_tag() {
        let raid = WireEvent::Raid {
            from_login: "raider".into(),
            from_display_name: "Raider".into(),
            viewers: 42,
        };
        let v = serde_json::to_value(&raid).expect("serializes");
        assert_eq!(v["kind"], "raid");
        assert_eq!(v["viewers"], 42);
        assert_eq!(v["from_display_name"], "Raider");

        let gift = WireEvent::MysteryGift {
            gifter_login: None,
            gifter_display_name: None,
            mass_gift_count: 10,
            sender_total_gifts: None,
            tier: "1000".into(),
        };
        let v = serde_json::to_value(&gift).expect("serializes");
        assert_eq!(v["kind"], "mystery_gift");
        assert!(v["gifter_login"].is_null());

        let cheer = WireEvent::from(CheerEvent {
            channel_login: "chan".into(),
            user_id: "7".into(),
            user_login: "cheerer".into(),
            display_name: "Cheerer".into(),
            bits: 500,
            text: "nice".into(),
        });
        let v = serde_json::to_value(&cheer).expect("serializes");
        assert_eq!(v["kind"], "cheer");
        assert_eq!(v["bits"], 500);
        assert_eq!(v["display_name"], "Cheerer");
        assert_eq!(v["message"], "nice");
    }

    /// Ground truth BY HAND: fresh message within lifetime never swept.
    #[test]
    fn fresh_message_survives_sweep() {
        let now = Instant::now();
        let mut st = EngineState::new(5);
        st.push(msg("fresh"));
        let expired = st.sweep_expired(now, Duration::from_secs(60));
        assert!(expired.is_empty());
        assert_eq!(st.len(), 1);
    }

    /// Ground truth BY HAND: mapping carries emotes/badges positions over.
    #[test]
    fn chat_message_maps_fully_to_rendered() {
        let cm = ChatMessage {
            id: "m1".into(),
            channel_login: "chan".into(),
            user_id: "77".into(),
            user_login: "user".into(),
            display_name: "User".into(),
            color: Some("#123456".into()),
            badges: vec![Badge {
                id: "moderator".into(),
                version: "1".into(),
            }],
            emotes: vec![EmoteRef {
                id: "25".into(),
                start: 4,
                end: 9,
            }],
            gifs: vec![GifRef {
                id: "g1".into(),
                start: 11,
                end: 16,
                url: "https://media.giphy.com/x".into(),
            }],
            text: "hey Kappa world".into(),
            reply_parent: None,
        };
        let r = RenderedMessage::from(cm);
        assert_eq!(
            r.gifs,
            vec![WireGif {
                id: "g1".into(),
                start: 11,
                end: 16,
                url: "https://media.giphy.com/x".into()
            }]
        );
        assert_eq!(
            r.emotes,
            vec![WireEmote {
                id: "25".into(),
                start: 4,
                end: 9
            }]
        );
        assert_eq!(
            r.badges,
            vec![WireBadge {
                id: "moderator".into(),
                version: "1".into()
            }]
        );
        assert_eq!(r.text, "hey Kappa world");
        assert_eq!(r.color.as_deref(), Some("#123456"));
    }
}
