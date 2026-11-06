/// A chat badge attached to a user (e.g. `broadcaster/1`).
#[derive(Debug, Clone, PartialEq)]
pub struct Badge {
    pub id: String,
    pub version: String,
}

/// An emote reference inside a message.
///
/// `start`/`end` are RAW UTF-16 code-unit offsets into the message text —
/// the exact numbers from the Twitch wire (no char normalization),
/// inclusive start, EXCLUSIVE end (twitch-irc adds +1 to the wire's
/// inclusive end).
///
/// GOTCHA: these are NOT Rust char indices — `&text[start..end]` panics or
/// slices the wrong text whenever a multibyte character precedes the emote.
/// They ARE valid offsets for JS `String.slice` (also UTF-16 units), which
/// is what the widget uses. Keep widget-side slicing in UTF-16 units.
#[derive(Debug, Clone, PartialEq)]
pub struct EmoteRef {
    pub id: String,
    pub start: usize,
    pub end: usize,
}

/// A GIPHY GIF sent via Twitch's Tier2/3 GIF Keyboard.
///
/// Positioned like an emote: RAW UTF-16 code-unit offsets from the wire,
/// inclusive start / exclusive end (+1 from the wire's inclusive end).
/// Same gotcha as [`EmoteRef`] — never char-slice these in Rust.
/// The URL is the signed GIPHY media URL from the wire tag — render as-is.
#[derive(Debug, Clone, PartialEq)]
pub struct GifRef {
    pub id: String,
    pub start: usize,
    pub end: usize,
    pub url: String,
}

/// The parent message of a Twitch reply thread.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplyParent {
    pub message_id: String,
    pub user_login: String,
    pub display_name: String,
    pub text: String,
}

/// One parsed chat message.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    pub id: String,
    pub channel_login: String,
    /// Twitch numeric user id (stable across renames).
    pub user_id: String,
    pub user_login: String,
    pub display_name: String,
    pub color: Option<String>,
    pub badges: Vec<Badge>,
    pub emotes: Vec<EmoteRef>,
    /// GIF positions (Twitch GIF Keyboard). Usually one; renders inline.
    pub gifs: Vec<GifRef>,
    pub text: String,
    /// Present when this message is a Twitch reply thread member.
    pub reply_parent: Option<ReplyParent>,
}

/// A new (or renewed) subscription. From USERNOTICE `sub`/`resub`.
#[derive(Debug, Clone, PartialEq)]
pub struct SubEvent {
    pub user_login: String,
    pub display_name: String,
    pub is_resub: bool,
    /// `Prime`, `1000`, `2000` or `3000`.
    pub tier: String,
    pub cumulative_months: u64,
    /// Consecutive-month streak; None when the user opted to hide it.
    pub streak_months: Option<u64>,
    /// Resub message text, when the user attached one.
    pub message: Option<String>,
}

/// One gifted sub delivered to a specific recipient.
/// From USERNOTICE `subgift`/`anonsubgift`.
#[derive(Debug, Clone, PartialEq)]
pub struct GiftSubEvent {
    /// None = anonymous gifter.
    pub gifter_login: Option<String>,
    pub gifter_display_name: Option<String>,
    pub recipient_login: String,
    pub recipient_display_name: String,
    pub tier: String,
    pub cumulative_months: u64,
    /// Multi-month gifts deliver this many months at once.
    pub num_gifted_months: u64,
}

/// The announcement preceding a wave of gift subs.
/// From USERNOTICE `submysterygift`/`anonsubmysterygift`.
#[derive(Debug, Clone, PartialEq)]
pub struct MysteryGiftEvent {
    /// None = anonymous gifter.
    pub gifter_login: Option<String>,
    pub gifter_display_name: Option<String>,
    pub mass_gift_count: u64,
    /// Gifter's lifetime total in the channel; absent on some promos.
    pub sender_total_gifts: Option<u64>,
    pub tier: String,
}

/// A channel point reward redemption with user input. From IRC PRIVMSG
/// carrying the `custom-reward-id` tag. The reward NAME is not on the wire —
/// resolve it via Helix (`custom_reward_titles`) when channel OAuth exists.
#[derive(Debug, Clone, PartialEq)]
pub struct RedeemEvent {
    pub channel_login: String,
    pub user_id: String,
    pub user_login: String,
    pub display_name: String,
    /// UUID of the redeemed custom reward.
    pub reward_id: String,
    /// The text the redeemer typed (may be empty for input-free rewards —
    /// those never reach chat, so this is usually non-empty).
    pub user_input: String,
}

/// A cheer (bits) message. From IRC PRIVMSG carrying the `bits` tag.
#[derive(Debug, Clone, PartialEq)]
pub struct CheerEvent {
    pub channel_login: String,
    pub user_id: String,
    pub user_login: String,
    pub display_name: String,
    /// Bits cheered.
    pub bits: u64,
    /// The cheer message text (may be empty).
    pub text: String,
}

/// An incoming raid. From USERNOTICE `raid`.
#[derive(Debug, Clone, PartialEq)]
pub struct RaidEvent {
    pub from_login: String,
    pub from_display_name: String,
    pub viewers: u64,
}

/// A chat message removed by a moderator/streamer. From IRC `CLEARMSG`.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageDeleted {
    pub message_id: String,
    pub sender_login: String,
}

/// The ENTIRE chat was cleared by a moderator. From IRC `CLEARCHAT`
/// with no user target. `UserBanned`/`UserTimedOut` actions are not
/// surfaced (their deletions arrive as CLEARMSG frames separately).
#[derive(Debug, Clone, PartialEq)]
pub struct ChatCleared;

/// Events surfaced by a chat source.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    ChatMessage(ChatMessage),
    Sub(SubEvent),
    GiftSub(GiftSubEvent),
    MysteryGift(MysteryGiftEvent),
    Raid(RaidEvent),
    /// A channel point reward redemption (PRIVMSG with custom-reward-id).
    Redeem(RedeemEvent),
    /// A cheer (bits) message (PRIVMSG with bits tag).
    Cheer(CheerEvent),
    /// A message was deleted by a moderator/streamer (IRC CLEARMSG).
    MessageDeleted(MessageDeleted),
    /// The whole chat was cleared (IRC CLEARCHAT, ChatCleared action).
    ChatCleared,
    /// Server PING. Answered internally by the library; informational only.
    Ping(String),
    /// Anything not yet modeled (JOIN/PART/NOTICE/USERNOTICE/...).
    Other,
}
