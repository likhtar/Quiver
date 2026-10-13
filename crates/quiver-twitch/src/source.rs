//! Chat source facade over `twitch-irc`.
//!
//! The library owns the hard parts: TLS, CAP negotiation, reconnect with
//! backoff, connection pooling, rate limiting, PING/PONG. This module maps
//! its message types onto Quiver's own [`Event`] model so tool crates never
//! depend on `twitch-irc` types directly.

use twitch_irc::login::StaticLoginCredentials;
use twitch_irc::message::{PrivmsgMessage, ServerMessage, UserNoticeEvent, UserNoticeMessage};
use twitch_irc::{ClientConfig, SecureTCPTransport, TwitchIRCClient};

use crate::error::TwitchError;
use crate::events::{
    Badge, ChatMessage, CheerEvent, EmoteRef, Event, GifRef, GiftSubEvent, MessageDeleted,
    MysteryGiftEvent, RaidEvent, RedeemEvent, SubEvent,
};

type Client = TwitchIRCClient<SecureTCPTransport, StaticLoginCredentials>;

/// Read-only anonymous chat source for one channel.
pub struct IrcChatSource {
    incoming: tokio::sync::mpsc::UnboundedReceiver<ServerMessage>,
    /// Kept alive on purpose: dropping the last client handle shuts the
    /// client (and its connections) down.
    _client: Client,
}

impl IrcChatSource {
    /// Offline validation for a channel login (no network).
    /// Used by hot reload to reject bad swap targets early.
    pub fn validate_channel_login(login: &str) -> Result<(), twitch_irc::validate::Error> {
        twitch_irc::validate::validate_login(login)
    }

    /// Clone of the underlying client handle — used for live channel
    /// swaps (`part`+`join`) without touching the event stream.
    pub fn client(&self) -> Client {
        self._client.clone()
    }

    /// Connect anonymously (justinfan — read-only, cannot send) and join
    /// `channel_login`. Connection is lazy: background tasks start once
    /// events are consumed.
    pub fn connect_anonymous(channel_login: String) -> Result<Self, TwitchError> {
        let config = ClientConfig::default();
        let (incoming, client) = Client::new(config);
        client.join(channel_login)?;
        Ok(Self {
            incoming,
            _client: client,
        })
    }

    /// Next event. Returns None when the stream has ended (client shut down).
    pub async fn next_event(&mut self) -> Option<Event> {
        loop {
            match self.incoming.recv().await? {
                ServerMessage::Privmsg(pm) => return Some(map_privmsg_event(pm)),
                ServerMessage::ClearMsg(cm) => {
                    return Some(Event::MessageDeleted(MessageDeleted {
                        message_id: cm.message_id,
                        sender_login: cm.sender_login,
                    }));
                }
                ServerMessage::ClearChat(cc) => {
                    // Only a WIDE clear empties the whole chat — bans/timeouts
                    // arrive as per-message CLEARMSG deletions instead.
                    if let twitch_irc::message::ClearChatAction::ChatCleared = cc.action {
                        return Some(Event::ChatCleared);
                    }
                }
                ServerMessage::UserNotice(un) => return Some(map_user_notice(un)),
                ServerMessage::Ping(ping) => {
                    // PingMessage keeps only the raw IRC frame; the token is
                    // its (optional) first parameter.
                    let token = ping.source.params.first().cloned().unwrap_or_default();
                    return Some(Event::Ping(token));
                }
                _ => continue, // unmodeled message types for now
            }
        }
    }
}

/// Map a PRIVMSG onto the right [`Event`]: a channel-point redemption
/// (carries `custom-reward-id`), a cheer (carries `bits`), else a plain
/// chat message. Redemptions win if both tags somehow coexist.
fn map_privmsg_event(pm: PrivmsgMessage) -> Event {
    // Channel point redemption: PRIVMSG rides a custom-reward-id tag.
    // Twitch sends the reward NAME in neither IRC tags nor this message —
    // resolve via Helix when channel OAuth exists.
    if let Some(reward_id) = pm.source.tags.0.get("custom-reward-id") {
        return Event::Redeem(RedeemEvent {
            channel_login: pm.channel_login,
            user_id: pm.sender.id,
            user_login: pm.sender.login,
            display_name: pm.sender.name,
            reward_id: reward_id.to_string(),
            user_input: pm.message_text,
        });
    }
    // Cheer (bits): PRIVMSG carrying a `bits` tag (positive integer).
    if let Some(bits) = pm.bits {
        return Event::Cheer(CheerEvent {
            channel_login: pm.channel_login,
            user_id: pm.sender.id,
            user_login: pm.sender.login,
            display_name: pm.sender.name,
            bits,
            text: pm.message_text,
        });
    }
    Event::ChatMessage(map_privmsg(pm))
}

/// Map a library PRIVMSG onto Quiver's own model.
fn map_privmsg(pm: PrivmsgMessage) -> ChatMessage {
    ChatMessage {
        id: pm.message_id,
        channel_login: pm.channel_login,
        user_id: pm.sender.id,
        user_login: pm.sender.login,
        display_name: pm.sender.name,
        color: pm.name_color.map(|c| c.to_string()),
        badges: pm
            .badges
            .into_iter()
            .map(|b| Badge {
                id: b.name,
                version: b.version,
            })
            .collect(),
        emotes: pm
            .emotes
            .into_iter()
            .map(|e| EmoteRef {
                id: e.id,
                start: e.char_range.start,
                end: e.char_range.end,
            })
            .collect(),
        text: pm.message_text,
        gifs: parse_gifs_tag(pm.source.tags.0.get("gifs").map(String::as_str)),
        reply_parent: pm.reply_parent.map(|rp| crate::events::ReplyParent {
            message_id: rp.message_id,
            user_login: rp.reply_parent_user.login,
            display_name: rp.reply_parent_user.name,
            text: rp.message_text,
        }),
    }
}

/// Parse the `gifs` IRC tag: `start-end|id|url[,start-end|id|url,...]`.
///
/// Indices are RAW UTF-16 code-unit offsets from the wire (Twitch's
/// convention), zero-based, inclusive end in the tag; the +1 makes end
/// exclusive. The unit space matches the widget's JS `String.slice`, so
/// ranges slice the correct visible text there — but they are NOT Rust
/// char indices (never `&text[start..end]` with them).
/// Malformed entries are skipped silently — never fatal.
fn parse_gifs_tag(raw: Option<&str>) -> Vec<GifRef> {
    let Some(raw) = raw else { return Vec::new() };
    let mut out = Vec::new();
    for item in raw.split(',') {
        let mut it = item.splitn(3, '|');
        let (Some(range), Some(id), Some(url)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        // range is "start-end"; +1 turns inclusive end into exclusive.
        let Some((s, e)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end)) = (s.parse::<usize>(), e.parse::<usize>()) else {
            continue;
        };
        let end_exclusive = end.saturating_add(1);
        if end_exclusive <= start {
            continue; // empty/inverted range
        }
        out.push(GifRef {
            id: id.to_string(),
            start,
            end: end_exclusive,
            url: url.to_string(),
        });
    }
    out
}

/// Map a library USERNOTICE onto Quiver's event model.
///
/// Modeled: sub/resub, subgift/anonsubgift, submysterygift/
/// anonsubmysterygift, raid. Everything else falls through to
/// [`Event::Other`] (rituals, gift upgrades, announcements, ...).
fn map_user_notice(un: UserNoticeMessage) -> Event {
    match un.event {
        UserNoticeEvent::SubOrResub {
            is_resub,
            cumulative_months,
            streak_months,
            sub_plan,
            ..
        } => Event::Sub(SubEvent {
            user_login: un.sender.login,
            display_name: un.sender.name,
            is_resub,
            tier: sub_plan,
            cumulative_months,
            streak_months,
            message: un.message_text,
        }),
        UserNoticeEvent::SubGift {
            is_sender_anonymous,
            cumulative_months,
            recipient,
            sub_plan,
            num_gifted_months,
            ..
        } => Event::GiftSub(GiftSubEvent {
            gifter_login: (!is_sender_anonymous).then(|| un.sender.login.clone()),
            gifter_display_name: (!is_sender_anonymous).then(|| un.sender.name.clone()),
            recipient_login: recipient.login,
            recipient_display_name: recipient.name,
            tier: sub_plan,
            cumulative_months,
            num_gifted_months,
        }),
        UserNoticeEvent::SubMysteryGift {
            mass_gift_count,
            sender_total_gifts,
            sub_plan,
        } => Event::MysteryGift(MysteryGiftEvent {
            // Non-anonymous: a real sender is always present — twitch-irc
            // routes every dummy-account gift into AnonSubMysteryGift, so
            // the sender here is genuinely named.
            gifter_login: Some(un.sender.login.clone()),
            gifter_display_name: Some(un.sender.name.clone()),
            mass_gift_count,
            sender_total_gifts,
            tier: sub_plan,
        }),
        UserNoticeEvent::AnonSubMysteryGift {
            mass_gift_count,
            sub_plan,
        } => Event::MysteryGift(MysteryGiftEvent {
            // Anonymous: NO gifter identity by definition. The old
            // anananonymousgifter login heuristic was unreachable — the
            // library routes every anonymous gift (dummy account id
            // 274598607, and every anonsubmysterygift msg-id) into this
            // variant. The variant is the reliable anonymity signal.
            gifter_login: None,
            gifter_display_name: None,
            mass_gift_count,
            sender_total_gifts: None,
            tier: sub_plan,
        }),
        UserNoticeEvent::Raid { viewer_count, .. } => Event::Raid(RaidEvent {
            from_login: un.sender.login,
            from_display_name: un.sender.name,
            viewers: viewer_count,
        }),
        _ => Event::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::TryFrom;
    use twitch_irc::message::IRCMessage;

    // Ground truth computed BY HAND:
    // text "hey Kappa world" → h0 e1 y2 ␣3 K4 a5 p6 p7 a8 ␣9 w10...
    // "Kappa" occupies chars 4..=8 inclusive → Range 4..9 exclusive.
    const LINE: &str = "@badge-info=;badges=broadcaster/1,subscriber/6;color=#FF0000;display-name=QuiverDev;emotes=25:4-8;id=abc-123;login=quiverdev;room-id=11148817;tmi-sent-ts=1594545155039;turbo=0;user-id=29803735 :quiverdev!quiverdev@quiverdev.tmi.twitch.tv PRIVMSG #quiverdev :hey Kappa world";

    fn parse_line_to_privmsg(line: &str) -> PrivmsgMessage {
        let irc_message = IRCMessage::parse(line).expect("line must parse as IRC");
        PrivmsgMessage::try_from(irc_message).expect("line must parse as PRIVMSG")
    }

    fn parse_line_to_event(line: &str) -> Event {
        let irc_message = IRCMessage::parse(line).expect("line must parse as IRC");
        let server = ServerMessage::try_from(irc_message).expect("must parse as ServerMessage");
        match server {
            ServerMessage::UserNotice(un) => map_user_notice(un),
            other => panic!("expected USERNOTICE, got {other:?}"),
        }
    }

    /// Slice `text` using UTF-16 code-unit offsets — exactly what the JS
    /// widget's `String.slice(start, end)` does. The wire (emote + gif
    /// ranges) is in UTF-16 units, NOT Rust chars, so this is the ONLY
    /// safe way to slice a range out of the text on the Rust side.
    /// Never use `&text[start..end]` — it panics or mis-slices as soon as
    /// a multibyte char precedes the range.
    fn slice_wire_range(text: &str, start: usize, end: usize) -> String {
        // Unit offset before each char boundary.
        let mut units = vec![0usize];
        for c in text.chars() {
            units.push(units.last().unwrap() + c.len_utf16());
        }
        // First char boundary at/after `start`, and at/after `end`.
        let s = units.iter().position(|&u| u >= start).unwrap_or(units.len() - 1);
        let e = units.iter().position(|&u| u >= end).unwrap_or(units.len() - 1);
        text.chars().skip(s).take(e.saturating_sub(s)).collect()
    }

    #[test]
    fn maps_reply_parent_tags() {
        // Ground truth BY HAND: reply to abc-123 ("original text") from Y.
        let line = format!(
            "@{base};reply-parent-msg-id=abc-123;reply-parent-user-id=99;reply-parent-user-login=y;reply-parent-display-name=Y;reply-parent-msg-body=original\\stext :x!x@x.tmi.twitch.tv PRIVMSG #chan :answering here",
            base = USERNOTICE_BASE_TAGS
        );
        let irc = IRCMessage::parse(&line).expect("parses");
        let pm = PrivmsgMessage::try_from(irc).expect("parses as privmsg");
        let cm = map_privmsg(pm);

        let rp = cm.reply_parent.expect("reply parent present");
        assert_eq!(rp.message_id, "abc-123");
        assert_eq!(rp.user_login, "y");
        assert_eq!(rp.display_name, "Y");
        // Tag unescaping turns \s into spaces.
        assert_eq!(rp.text, "original text");
        assert_eq!(cm.text, "answering here");

        // Non-reply messages must carry None.
        let plain = map_privmsg(parse_line_to_privmsg(LINE));
        assert!(plain.reply_parent.is_none());
    }

    #[test]
    fn maps_cheer_privmsg_to_cheer_event() {
        // bits=500 tag → Event::Cheer with the amount and text preserved.
        let line = "@badge-info=;badges=bits/100;color=;display-name=Cheerer;emotes=;id=c-1;login=cheerer;mod=0;room-id=11148817;subscriber=0;tmi-sent-ts=1594545155039;turbo=0;user-id=555;bits=500 :cheerer!cheerer@cheerer.tmi.twitch.tv PRIVMSG #chan :Cheer500 nice";
        let event = map_privmsg_event(parse_line_to_privmsg(line));
        match event {
            Event::Cheer(c) => {
                assert_eq!(c.bits, 500);
                assert_eq!(c.display_name, "Cheerer");
                assert_eq!(c.user_login, "cheerer");
                assert_eq!(c.user_id, "555");
                assert_eq!(c.text, "Cheer500 nice");
                assert_eq!(c.channel_login, "chan");
            }
            other => panic!("expected Cheer, got {other:?}"),
        }
    }

    #[test]
    fn maps_custom_reward_privmsg_to_redeem_event() {
        let line = "@badge-info=;badges=;color=;custom-reward-id=abc-reward;display-name=Redeemer;emotes=;id=r-1;login=redeemer;mod=0;room-id=11148817;subscriber=0;tmi-sent-ts=1594545155039;turbo=0;user-id=666 :redeemer!redeemer@redeemer.tmi.twitch.tv PRIVMSG #chan :song please";
        match map_privmsg_event(parse_line_to_privmsg(line)) {
            Event::Redeem(r) => {
                assert_eq!(r.reward_id, "abc-reward");
                assert_eq!(r.user_input, "song please");
            }
            other => panic!("expected Redeem, got {other:?}"),
        }
    }

    #[test]
    fn maps_plain_privmsg_to_chat_message() {
        match map_privmsg_event(parse_line_to_privmsg(LINE)) {
            Event::ChatMessage(cm) => assert_eq!(cm.text, "hey Kappa world"),
            other => panic!("expected ChatMessage, got {other:?}"),
        }
    }

    /// Official Twitch IRC-docs example: a Tier2/3 GIF Keyboard message.
    /// Text is the GIF's name; the `gifs` tag gives position/id/signed URL.
    /// Ground truth BY HAND: "[Y A Y Yes GIF by Djemilah Birnie]" is 34 chars
    /// (indices 0..=33). Exclusive end = 34. Slice must equal the full name.
    #[test]
    fn maps_gifs_tag_from_documented_example() {
        let line = format!(
            "@{base};gifs=0-33|joSNxeswxuc74Juo8X|https://media4.giphy.com/media/joSNxeswxuc74Juo8X/giphy.gif :twitchdev!twitchdev@twitchdev.tmi.twitch.tv PRIVMSG #twitch :[Y A Y Yes GIF by Djemilah Birnie]",
            base = USERNOTICE_BASE_TAGS
        );
        let irc = IRCMessage::parse(&line).expect("parses");
        let pm = PrivmsgMessage::try_from(irc).expect("parses as privmsg");
        let cm = map_privmsg(pm);

        assert_eq!(cm.text, "[Y A Y Yes GIF by Djemilah Birnie]");
        assert_eq!(cm.gifs.len(), 1);
        let g = &cm.gifs[0];
        assert_eq!(g.id, "joSNxeswxuc74Juo8X");
        assert_eq!(g.start, 0);
        assert_eq!(g.end, 34);
        assert!(
            g.url
                .starts_with("https://media4.giphy.com/media/joSNxeswxuc74Juo8X/")
        );
        // The gif range must slice the whole name text (in UTF-16 units —
        // Rust char slicing would NOT be safe once multibyte chars exist).
        assert_eq!(
            slice_wire_range(&cm.text, g.start, g.end),
            "[Y A Y Yes GIF by Djemilah Birnie]"
        );
    }

    /// Regression for #42: an emoji (2 UTF-16 units) before the GIF shifts
    /// the wire offsets away from char indices. The range must still slice
    /// the visible GIF name when handled in UTF-16 units (as the widget
    /// does with String.slice); a naive `&text[start..end]` would panic.
    #[test]
    fn gif_range_with_multibyte_prefix_slices_correct_text() {
        // "😀[Y A Y]" — 😀 = 2 UTF-16 units, " " = 1, "[" = 3 → range 3-9
        // inclusive on the wire, exclusive end 10 after +1.
        let line = format!(
            "@{base};gifs=3-9|joSNxeswxuc74Juo8X|https://media4.giphy.com/media/joSNxeswxuc74Juo8X/giphy.gif :twitchdev!twitchdev@twitchdev.tmi.twitch.tv PRIVMSG #twitch :😀 [Y A Y]",
            base = USERNOTICE_BASE_TAGS
        );
        let irc = IRCMessage::parse(&line).expect("parses");
        let pm = PrivmsgMessage::try_from(irc).expect("parses as privmsg");
        let cm = map_privmsg(pm);
        assert_eq!(cm.text, "😀 [Y A Y]");
        let g = &cm.gifs[0];
        assert_eq!((g.start, g.end), (3, 10));
        // Two units = one char: the range must NOT include the emoji.
        assert_eq!(slice_wire_range(&cm.text, g.start, g.end), "[Y A Y]");
        // And a naive char-based slice (the old comments promised char
        // indices) lands on the WRONG text — document why we keep the
        // unit convention end-to-end.
        assert_eq!(cm.text.chars().count(), 9, "😀 counts as ONE char");
        assert_ne!(
            cm.text.chars().skip(3).take(7).collect::<String>(),
            "[Y A Y]"
        );
    }

    /// Ground truth BY HAND: malformed gif entries are skipped, valid ones kept.
    #[test]
    fn parses_gifs_tag_skipping_malformed_entries() {
        let raw = "0-2|a|https://x/1,garbage,5-6|b|https://x/2,9-8|c|https://x/3";
        let gifs = parse_gifs_tag(Some(raw));
        assert_eq!(gifs.len(), 2, "only well-formed, non-inverted ranges kept");
        assert_eq!(gifs[0].id, "a");
        assert_eq!((gifs[0].start, gifs[0].end), (0, 3));
        assert_eq!(gifs[1].id, "b");
        // 9-8 inverts → skipped; 'garbage' lacks all pipes → skipped.
        assert!(!gifs.iter().any(|g| g.id == "c"));
    }

    #[test]
    fn no_gifs_tag_yields_empty() {
        assert!(parse_gifs_tag(None).is_empty());
    }

    #[test]
    fn maps_full_privmsg_with_hand_verified_ground_truth() {
        let cm = map_privmsg(parse_line_to_privmsg(LINE));

        assert_eq!(cm.id, "abc-123");
        assert_eq!(cm.channel_login, "quiverdev");
        assert_eq!(cm.user_id, "29803735");
        assert_eq!(cm.user_login, "quiverdev");
        assert_eq!(cm.display_name, "QuiverDev");
        assert_eq!(cm.color.as_deref(), Some("#FF0000"));
        assert_eq!(
            cm.badges,
            vec![
                Badge {
                    id: "broadcaster".to_string(),
                    version: "1".to_string()
                },
                Badge {
                    id: "subscriber".to_string(),
                    version: "6".to_string()
                },
            ]
        );
        assert_eq!(
            cm.emotes,
            vec![EmoteRef {
                id: "25".to_string(),
                start: 4,
                end: 9
            }]
        );
        assert_eq!(cm.text, "hey Kappa world");
    }

    #[test]
    fn emote_range_slices_the_actual_text() {
        let cm = map_privmsg(parse_line_to_privmsg(LINE));
        let e = &cm.emotes[0];
        assert_eq!(slice_wire_range(&cm.text, e.start, e.end), "Kappa");
    }

    /// Regression for #42: an emoji before the emote shifts wire offsets
    /// away from char indices. "😀 Kappa": 😀 = 2 UTF-16 units, ' ' = 1,
    /// Kappa at units 3..=7 inclusive → wire tag 3-7 + twitch-irc +1 = 8.
    #[test]
    fn emote_range_with_multibyte_prefix_slices_correct_text() {
        let line = "@badge-info=;badges=;color=;display-name=x;emotes=25:3-7;id=abc-123;login=x;room-id=1;tmi-sent-ts=1594545155039;turbo=0;user-id=1 :x!x@x.tmi.twitch.tv PRIVMSG #chn :😀 Kappa";
        let cm = map_privmsg(parse_line_to_privmsg(line));
        assert_eq!(cm.text, "😀 Kappa");
        let e = &cm.emotes[0];
        assert_eq!((e.start, e.end), (3, 8));
        assert_eq!(slice_wire_range(&cm.text, e.start, e.end), "Kappa");
    }

    #[test]
    fn maps_privmsg_without_color_badges_or_emotes() {
        // Realistic "plain user" message: tags present but EMPTY
        // (no color chosen, no badges, no emotes) — this is what Twitch
        // actually sends, as opposed to omitting the tags.
        let line = "@badge-info=;badges=;color=;display-name=Nick;emotes=;id=msg-1;login=nick;room-id=42;tmi-sent-ts=1594545155039;user-id=7 :nick!nick@nick.tmi.twitch.tv PRIVMSG #chan :hello";
        let cm = map_privmsg(parse_line_to_privmsg(line));

        assert_eq!(cm.color, None);
        assert!(cm.badges.is_empty());
        assert!(cm.emotes.is_empty());
        assert_eq!(cm.text, "hello");
        assert_eq!(cm.user_login, "nick");
    }

    #[tokio::test]
    async fn rejects_malformed_channel_login() {
        // join() validates channel names before any network activity.
        let err = IrcChatSource::connect_anonymous("BAD CHANNEL!".to_string());
        assert!(err.is_err(), "malformed channel login must be rejected");
    }

    // ---- USERNOTICE mapping --------------------------------------------------
    // Lines follow Twitch's real wire format (mandatory tags included).

    const USERNOTICE_BASE_TAGS: &str = "badge-info=;badges=;color=;display-name=X;emotes=;id=n-1;login=x;mod=0;room-id=11148817;subscriber=0;system-msg=Something\\shappened.;tmi-sent-ts=1594545155039;turbo=0;user-id=42";

    /// Ground truth BY HAND: sub, 12 cumulative months, tier 1000.
    #[test]
    fn maps_sub() {
        let line = format!(
            "@{base};msg-id=sub;msg-param-cumulative-months=12;msg-param-sub-plan=1000;msg-param-sub-plan-name=ChanSub;msg-param-should-share-streak=0 :x!x@x.tmi.twitch.tv USERNOTICE #chan",
            base = USERNOTICE_BASE_TAGS
        );
        match parse_line_to_event(&line) {
            Event::Sub(s) => {
                assert_eq!(s.user_login, "x");
                assert_eq!(s.display_name, "X");
                assert!(!s.is_resub);
                assert_eq!(s.tier, "1000");
                assert_eq!(s.cumulative_months, 12);
                assert_eq!(s.streak_months, None);
                assert_eq!(s.message, None);
            }
            other => panic!("expected Sub, got {other:?}"),
        }
    }

    /// Ground truth BY HAND: resub with streak + user message.
    #[test]
    fn maps_resub_with_streak_and_message() {
        let line = format!(
            "@{base};msg-id=resub;msg-param-cumulative-months=24;msg-param-streak-months=7;msg-param-should-share-streak=1;msg-param-sub-plan=2000;msg-param-sub-plan-name=ChanSub :x!x@x.tmi.twitch.tv USERNOTICE #chan :love the streams!",
            base = USERNOTICE_BASE_TAGS
        );
        match parse_line_to_event(&line) {
            Event::Sub(s) => {
                assert!(s.is_resub);
                assert_eq!(s.cumulative_months, 24);
                assert_eq!(s.streak_months, Some(7));
                assert_eq!(s.tier, "2000");
                assert_eq!(s.message.as_deref(), Some("love the streams!"));
            }
            other => panic!("expected Sub(resub), got {other:?}"),
        }
    }

    /// Ground truth BY HAND: raid from RaidLeader with 42 viewers.
    #[test]
    fn maps_raid() {
        // Raider identity owns the prefix; base tags say "x" so override
        // the identity tags too — real raids carry consistent identity.
        let line = r"@badge-info=;badges=broadcaster/1;color=;display-name=RaidLeader;emotes=;id=r-1;login=raidleader;mod=0;room-id=11148817;subscriber=0;system-msg=RaidLeader\sis\traiding.;tmi-sent-ts=1594545155039;turbo=0;user-id=77;msg-id=raid;msg-param-display-name=RaidLeader;msg-param-login=raidleader;msg-param-viewerCount=42;msg-param-profileImageURL=https://static-cdn.jtvnw.net/jtv_user_pictures/prof.png :raidleader!raidleader@raidleader.tmi.twitch.tv USERNOTICE #chan";

        match parse_line_to_event(line) {
            Event::Raid(r) => {
                assert_eq!(r.from_login, "raidleader");
                assert_eq!(r.from_display_name, "RaidLeader");
                assert_eq!(r.viewers, 42);
            }
            other => panic!("expected Raid, got {other:?}"),
        }
    }

    /// Ground truth BY HAND: named gifter, recipient Y, 5 gift-months,
    /// recipient at 8 cumulative months, tier 3000.
    #[test]
    fn maps_subgift() {
        let line = format!(
            "@{base};msg-id=subgift;msg-param-gift-months=5;msg-param-months=8;msg-param-cumulative-months=8;msg-param-recipient-id=99;msg-param-recipient-user-name=y;msg-param-recipient-display-name=Y;msg-param-sub-plan=3000;msg-param-sub-plan-name=ChanSub :x!x@x.tmi.twitch.tv USERNOTICE #chan",
            base = USERNOTICE_BASE_TAGS
        );
        match parse_line_to_event(&line) {
            Event::GiftSub(g) => {
                assert_eq!(g.gifter_login.as_deref(), Some("x"));
                assert_eq!(g.recipient_login, "y");
                assert_eq!(g.recipient_display_name, "Y");
                assert_eq!(g.tier, "3000");
                assert_eq!(g.cumulative_months, 8);
                assert_eq!(g.num_gifted_months, 5);
            }
            other => panic!("expected GiftSub, got {other:?}"),
        }
    }

    /// Ground truth BY HAND: mystery gift of 10 subs, lifetime total 55.
    #[test]
    fn maps_mystery_gift() {
        let line = format!(
            "@{base};msg-id=submysterygift;msg-param-mass-gift-count=10;msg-param-sender-count=55;msg-param-sub-plan=1000;msg-param-sub-plan-name=ChanSub :x!x@x.tmi.twitch.tv USERNOTICE #chan",
            base = USERNOTICE_BASE_TAGS
        );
        match parse_line_to_event(&line) {
            Event::MysteryGift(m) => {
                assert_eq!(m.gifter_login.as_deref(), Some("x"));
                assert_eq!(m.mass_gift_count, 10);
                assert_eq!(m.sender_total_gifts, Some(55));
                assert_eq!(m.tier, "1000");
            }
            other => panic!("expected MysteryGift, got {other:?}"),
        }
    }

    /// Ground truth: anonymous mystery gift (anonsubmysterygift msg-id). The
    /// sender is Twitch's dummy account — the variant is the anonymity
    /// signal, NOT the login string. Regression for #39 (these used to
    /// fall through to Event::Other and vanish from the widget).
    #[test]
    fn maps_anonymous_mystery_gift_with_no_gifter_identity() {
        let line = "@badge-info=;badges=;color=;display-name=xQcOW;emotes=;flags=;id=8db97752-3dee-460b-9001-e925d0e2ba5b;login=xqcow;mod=0;msg-id=anonsubmysterygift;msg-param-mass-gift-count=15;msg-param-origin-id=13\\s33\\sed\\sc0\\sef\\sa0\\s7b\\s9b\\s48\\s59\\scb\\scc\\se4\\s39\\s7b\\s90\\sf9\\s54\\s75\\s66;msg-param-sub-plan=2000;room-id=71092938;subscriber=0;system-msg=An\\sanonymous\\suser\\sis\\sgifting\\s10\\sTier\\s1\\sSubs\\sto\\sxQcOW's\\scommunity!;tmi-sent-ts=1585447099603;user-id=71092938;user-type= :tmi.twitch.tv USERNOTICE #xqcow";
        match parse_line_to_event(&line) {
            Event::MysteryGift(m) => {
                assert_eq!(m.gifter_login, None);
                assert_eq!(m.gifter_display_name, None);
                assert_eq!(m.mass_gift_count, 15);
                assert_eq!(m.sender_total_gifts, None);
                assert_eq!(m.tier, "2000");
            }
            other => panic!("expected MysteryGift, got {other:?}"),
        }
    }

    /// Ground truth BY HAND: a moderator CLEARMSG carries the deleted
    /// message id + sender login; we surface it as MessageDeleted.
    #[test]
    fn maps_clearmsg_to_message_deleted() {
        let line = "@login=offender;room-id=123;target-msg-id=abc-123;tmi-sent-ts=1783632907018 :tmi.twitch.tv CLEARMSG #likh_tar :the deleted message";
        let irc = IRCMessage::parse(line).expect("parses");
        match ServerMessage::try_from(irc).expect("parses as ServerMessage") {
            ServerMessage::ClearMsg(cm) => {
                let ev = crate::events::MessageDeleted {
                    message_id: cm.message_id,
                    sender_login: cm.sender_login,
                };
                assert_eq!(ev.message_id, "abc-123");
                assert_eq!(ev.sender_login, "offender");
            }
            other => panic!("expected ClearMsg, got {other:?}"),
        }
    }

    /// Ground truth BY HAND: parameterless CLEARCHAT = full chat cleared.
    #[test]
    fn maps_clearchat_chatcleared() {
        let line = "@room-id=123;tmi-sent-ts=1783632907018 :tmi.twitch.tv CLEARCHAT #likh_tar";
        let irc = IRCMessage::parse(line).expect("parses");
        match ServerMessage::try_from(irc).expect("parses as ServerMessage") {
            ServerMessage::ClearChat(cc) => {
                assert!(matches!(
                    cc.action,
                    twitch_irc::message::ClearChatAction::ChatCleared
                ));
            }
            other => panic!("expected ClearChat, got {other:?}"),
        }
    }

    /// Unmodeled USERNOTICE types (e.g. rituals) must not crash the feed.
    #[test]
    fn maps_unknown_usernotice_to_other() {
        let line = format!(
            "@{base};msg-id=ritual;msg-param-ritual-name=new_chatter :x!x@x.tmi.twitch.tv USERNOTICE #chan",
            base = USERNOTICE_BASE_TAGS
        );
        assert_eq!(parse_line_to_event(&line), Event::Other);
    }
}
