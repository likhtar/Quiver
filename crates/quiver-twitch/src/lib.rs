//! Quiver's Twitch connectivity layer.
//!
//! The ONLY crate in the workspace allowed to speak Twitch. Built as a
//! facade over [`twitch-irc`](https://crates.io/crates/twitch-irc): the
//! library stays an implementation detail — tool crates see only
//! [`events::Event`] and friends, so the backend can be swapped without
//! touching them.

pub mod auth;
pub mod dedupe;
pub mod error;
pub mod events;
pub mod eventsub;
pub mod helix;
pub mod source;

pub use auth::{ChannelToken, DEFAULT_SCOPES, StoredTokens};
pub use dedupe::RedeemDeduper;
pub use error::TwitchError;
pub use events::{
    Badge, ChatMessage, CheerEvent, EmoteRef, Event, GifRef, GiftSubEvent, MessageDeleted,
    MysteryGiftEvent, RaidEvent, RedeemEvent, ReplyParent, SubEvent,
};
pub use helix::{HelixClient, HelixError, Redemption, RewardInfo};
pub use source::IrcChatSource;
