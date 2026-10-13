//! Message filtering: compiled allowlist/denylist dimensions.
//!
//! Pure evaluation logic — no I/O, no state. `CompiledFilters::compile`
//! turns config into matchers; `permits` is the single decision point
//! the engine calls for every message/event.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use quiver_twitch::Badge;
use regex::Regex;

use crate::config::{FiltersConfig, ListFilter, Mode};

/// Compiled filters shared between the server (initial/reload) and the
/// engine pump. `None` = no filtering configured.
pub(crate) type SharedCompiled = Arc<RwLock<Option<CompiledFilters>>>;

/// Message kinds as they appear in filter config AND on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgKind {
    Message,
    Sub,
    GiftSub,
    MysteryGift,
    Raid,
    Redeem,
    Cheer,
    HypeTrain,
    Prediction,
    Poll,
    Follow,
}

impl MsgKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "message" => Some(Self::Message),
            "sub" => Some(Self::Sub),
            "gift_sub" => Some(Self::GiftSub),
            "mystery_gift" => Some(Self::MysteryGift),
            "raid" => Some(Self::Raid),
            "redeem" => Some(Self::Redeem),
            "cheer" => Some(Self::Cheer),
            "hype_train" => Some(Self::HypeTrain),
            "prediction" => Some(Self::Prediction),
            "poll" => Some(Self::Poll),
            "follow" => Some(Self::Follow),
            _ => None,
        }
    }

    /// Canonical wire/config name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Sub => "sub",
            Self::GiftSub => "gift_sub",
            Self::MysteryGift => "mystery_gift",
            Self::Raid => "raid",
            Self::Redeem => "redeem",
            Self::Cheer => "cheer",
            Self::HypeTrain => "hype_train",
            Self::Prediction => "prediction",
            Self::Poll => "poll",
            Self::Follow => "follow",
        }
    }
}

#[derive(Debug)]
struct RegexDim {
    allowlist: bool,
    res: Vec<Regex>,
}

impl RegexDim {
    fn compile(dim: &ListFilter) -> Result<Self, String> {
        let mut res = Vec::new();
        for item in &dim.items {
            // An empty pattern matches every string (position-0 match),
            // silently blocking all chat in denylist mode and neutering
            // every other pattern in allowlist mode. validate() rejects it;
            // this is defense-in-depth for programmatic construction.
            if item.is_empty() {
                return Err(
                    "empty pattern matches every message — refusing (likely a config error)"
                        .to_string(),
                );
            }
            res.push(Regex::new(item).map_err(|e| format!("{item:?}: {e}"))?);
        }
        Ok(Self {
            allowlist: dim.mode == Mode::Allowlist,
            res,
        })
    }

    /// None = inactive (empty items).
    fn passes(&self, value: &str) -> bool {
        if self.res.is_empty() {
            return true;
        }
        let matched = self.res.iter().any(|r| r.is_match(value));
        if self.allowlist { matched } else { !matched }
    }
}

#[derive(Debug)]
struct ExactDim {
    allowlist: bool,
    set: HashSet<String>,
}

impl ExactDim {
    fn new(dim: &ListFilter) -> Self {
        Self {
            allowlist: dim.mode == Mode::Allowlist,
            set: dim.items.iter().cloned().collect(),
        }
    }

    fn passes(&self, value: &str) -> bool {
        if self.set.is_empty() {
            return true;
        }
        let matched = self.set.contains(value);
        if self.allowlist { matched } else { !matched }
    }
}

/// Everything the permit decision needs for one message/event.
/// Badges are the sender's badge ids (empty for events in v1).
pub struct PermitCtx<'a> {
    pub kind: MsgKind,
    pub login: &'a str,
    pub display_name: &'a str,
    pub user_id: &'a str,
    pub badges: &'a [Badge],
    pub content: &'a str,
}

/// Compiled, ready-to-evaluate filters. `None` fields are inactive.
#[derive(Debug, Default)]
pub struct CompiledFilters {
    display_name: Option<RegexDim>,
    username: Option<RegexDim>,
    user_id: Option<ExactDim>,
    content: Option<RegexDim>,
    message_type: Option<ExactDim>,
    role: Option<ExactDim>,
}

impl CompiledFilters {
    /// Compile config into matchers. Errors carry the offending pattern.
    pub fn compile(cfg: &FiltersConfig) -> Result<Self, String> {
        let compile_regex_dim = |dim: &Option<ListFilter>| -> Result<Option<RegexDim>, String> {
            dim.as_ref().map(RegexDim::compile).transpose()
        };
        let exact =
            |dim: &Option<ListFilter>| -> Option<ExactDim> { dim.as_ref().map(ExactDim::new) };

        Ok(Self {
            display_name: compile_regex_dim(&cfg.display_name)?,
            username: compile_regex_dim(&cfg.username)?,
            user_id: exact(&cfg.user_id),
            content: compile_regex_dim(&cfg.content)?,
            message_type: exact(&cfg.message_type),
            role: exact(&cfg.role),
        })
    }

    /// The single decision point. True = render.
    ///
    /// Dimensions combine with AND; inactive dimensions always pass.
    /// An empty items list makes its dimension inactive by construction.
    pub fn permits(&self, ctx: &PermitCtx) -> bool {
        if let Some(dim) = &self.display_name
            && !dim.passes(ctx.display_name)
        {
            return false;
        }
        if let Some(dim) = &self.username
            && !dim.passes(ctx.login)
        {
            return false;
        }
        if let Some(dim) = &self.user_id
            && !dim.passes(ctx.user_id)
        {
            return false;
        }
        if let Some(dim) = &self.content
            && !dim.passes(ctx.content)
        {
            return false;
        }
        if let Some(dim) = &self.message_type
            && !dim.passes(ctx.kind.name())
        {
            return false;
        }
        if let Some(dim) = &self.role {
            // Pass when ANY carried badge matches the dimension's rules.
            let badge_ids: Vec<&str> = ctx.badges.iter().map(|b| b.id.as_str()).collect();
            let passes = if dim.set.is_empty() {
                true
            } else if dim.allowlist {
                badge_ids.iter().any(|id| dim.set.contains(*id))
            } else {
                !badge_ids.iter().any(|id| dim.set.contains(*id))
            };
            if !passes {
                return false;
            }
        }
        true
    }

    /// Convenience gate for events produced OUTSIDE the engine pump (the
    /// redemption poller, EventSub): builds a PermitCtx from scalar fields.
    pub fn permits_event(
        compiled: &SharedCompiled,
        kind: MsgKind,
        login: &str,
        display_name: &str,
        user_id: &str,
        content: &str,
    ) -> bool {
        let guard = match compiled.read() {
            Ok(g) => g,
            Err(_) => return true, // fail-open, same as the pump's permitted()
        };
        let Some(f) = guard.as_ref() else {
            return true;
        };
        f.permits(&PermitCtx {
            kind,
            login,
            display_name,
            user_id,
            badges: &[],
            content,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ListFilter;

    fn list(mode: Mode, items: &[&str]) -> ListFilter {
        ListFilter {
            mode,
            items: items.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn cfg() -> FiltersConfig {
        FiltersConfig::default()
    }

    fn ctx<'a>(
        kind: MsgKind,
        login: &'a str,
        display: &'a str,
        id: &'a str,
        badges: &'a [Badge],
        content: &'a str,
    ) -> PermitCtx<'a> {
        PermitCtx {
            kind,
            login,
            display_name: display,
            user_id: id,
            badges,
            content,
        }
    }

    // ---- per-dimension basics ---------------------------------------------

    #[test]
    fn no_filters_permit_everything() {
        let f = CompiledFilters::compile(&cfg()).unwrap();
        assert!(f.permits(&ctx(MsgKind::Message, "a", "A", "1", &[], "hi")));
    }

    #[test]
    fn denylist_drops_matching_display_name() {
        let mut c = cfg();
        c.display_name = Some(list(Mode::Denylist, &["^Stream.*"]));
        let f = CompiledFilters::compile(&c).unwrap();
        assert!(!f.permits(&ctx(
            MsgKind::Message,
            "streamelements",
            "StreamElements",
            "1",
            &[],
            ""
        )));
        assert!(f.permits(&ctx(
            MsgKind::Message,
            "melodieee__",
            "Melodieee__",
            "2",
            &[],
            ""
        )));
    }

    #[test]
    fn allowlist_requires_a_match() {
        let mut c = cfg();
        c.username = Some(list(Mode::Allowlist, &["^vip_"]));
        let f = CompiledFilters::compile(&c).unwrap();
        assert!(f.permits(&ctx(MsgKind::Message, "vip_friend", "F", "1", &[], "")));
        assert!(!f.permits(&ctx(MsgKind::Message, "random", "R", "2", &[], "")));
    }

    #[test]
    fn empty_items_make_dimension_inactive() {
        // Empty ALLOWLIST would mean "drop everything" — refused: inactive.
        let mut c = cfg();
        c.username = Some(list(Mode::Allowlist, &[]));
        let f = CompiledFilters::compile(&c).unwrap();
        assert!(f.permits(&ctx(MsgKind::Message, "anyone", "Anyone", "1", &[], "")));
    }

    #[test]
    fn user_id_is_exact_not_regex() {
        let mut c = cfg();
        // Regex metachars must be LITERAL here: "7*" matches only the id
        // literally spelled "7*", never "71092938".
        c.user_id = Some(list(Mode::Denylist, &["7*", "123"]));
        let f = CompiledFilters::compile(&c).unwrap();
        assert!(f.permits(&ctx(MsgKind::Message, "u", "U", "71092938", &[], "")));
        assert!(!f.permits(&ctx(MsgKind::Message, "u", "U", "123", &[], "")));
        assert!(!f.permits(&ctx(MsgKind::Message, "u", "U", "7*", &[], "")));
    }

    #[test]
    fn content_denylist_case_insensitive_via_inline_flag() {
        let mut c = cfg();
        c.content = Some(list(Mode::Denylist, &["(?i)free money"]));
        let f = CompiledFilters::compile(&c).unwrap();
        assert!(!f.permits(&ctx(MsgKind::Message, "u", "U", "1", &[], "FREE MONEY now")));
        assert!(f.permits(&ctx(MsgKind::Message, "u", "U", "1", &[], "hello")));
    }

    // ---- kinds ---------------------------------------------------------------
    // Command filtering is a content-regex concern (e.g. `^!`), not a kind.

    #[test]
    fn content_regex_covers_command_style_filtering() {
        // The documented replacement for the removed command_prefixes:
        // deny content matching ^! — drops "!so xqc", keeps normal text.
        let mut c = cfg();
        c.content = Some(list(Mode::Denylist, &["^!"]));
        let f = CompiledFilters::compile(&c).unwrap();
        assert!(!f.permits(&ctx(MsgKind::Message, "u", "U", "1", &[], "!so xqc")));
        assert!(f.permits(&ctx(MsgKind::Message, "u", "U", "1", &[], "hello")));
    }

    #[test]
    fn message_type_filter_drops_subs_only() {
        let mut c = cfg();
        c.message_type = Some(list(Mode::Denylist, &["sub"]));
        let f = CompiledFilters::compile(&c).unwrap();
        assert!(!f.permits(&ctx(MsgKind::Sub, "u", "U", "1", &[], "")));
        assert!(f.permits(&ctx(MsgKind::Message, "u", "U", "1", &[], "normal")));
    }

    // ---- roles ----------------------------------------------------------------

    #[test]
    fn role_allowlist_keeps_only_listed_badges() {
        let mut c = cfg();
        c.role = Some(list(Mode::Allowlist, &["moderator", "vip"]));
        let f = CompiledFilters::compile(&c).unwrap();

        let mod_badges = [Badge {
            id: "moderator".into(),
            version: "1".into(),
        }];
        let plain: Vec<Badge> = Vec::new();
        assert!(f.permits(&ctx(MsgKind::Message, "m", "M", "1", &mod_badges, "")));
        assert!(!f.permits(&ctx(MsgKind::Message, "p", "P", "2", &plain, "")));
    }

    #[test]
    fn role_denylist_removes_listed_badges_only() {
        let mut c = cfg();
        c.role = Some(list(Mode::Denylist, &["subscriber"]));
        let f = CompiledFilters::compile(&c).unwrap();

        let sub = [Badge {
            id: "subscriber".into(),
            version: "54".into(),
        }];
        let mixed = [
            Badge {
                id: "subscriber".into(),
                version: "54".into(),
            },
            Badge {
                id: "moderator".into(),
                version: "1".into(),
            },
        ];
        assert!(!f.permits(&ctx(MsgKind::Message, "s", "S", "1", &sub, "")));
        // Carrying subscriber among others → still denied (denylist = any hit).
        assert!(!f.permits(&ctx(MsgKind::Message, "m", "M", "2", &mixed, "")));
    }

    // ---- composition ------------------------------------------------------------

    /// Ground truth BY HAND: username denylist hits AND role allowlist
    /// fails → dropped. Fixing either one lets it through (AND semantics).
    #[test]
    fn dimensions_combine_with_and() {
        let mut c = cfg();
        c.username = Some(list(Mode::Denylist, &["^bot"]));
        c.role = Some(list(Mode::Allowlist, &["moderator"]));
        let f = CompiledFilters::compile(&c).unwrap();

        let mod_badges = [Badge {
            id: "moderator".into(),
            version: "1".into(),
        }];
        // bot with moderator badge: username dimension drops it.
        assert!(!f.permits(&ctx(
            MsgKind::Message,
            "botguy",
            "BotGuy",
            "1",
            &mod_badges,
            ""
        )));
        // human without badge: role dimension drops it.
        assert!(!f.permits(&ctx(MsgKind::Message, "human", "Human", "2", &[], "")));
        // human moderator: both pass.
        assert!(f.permits(&ctx(
            MsgKind::Message,
            "realmod",
            "RealMod",
            "3",
            &mod_badges,
            ""
        )));
    }

    #[test]
    fn invalid_regex_is_a_compile_error() {
        let mut c = cfg();
        c.content = Some(list(Mode::Denylist, &["[unclosed"]));
        assert!(CompiledFilters::compile(&c).is_err());
    }

    /// Regression for #11: the empty pattern matches EVERY string
    /// (position-0 match) and would silently block all chat in denylist
    /// mode. compile() must refuse it even when constructed programmatically
    /// (bypassing config validation).
    #[test]
    fn empty_regex_item_refuses_to_compile() {
        let mut c = cfg();
        c.content = Some(list(Mode::Denylist, &["nightbot", ""]));
        let err = CompiledFilters::compile(&c).expect_err("empty pattern must be refused");
        assert!(err.contains("empty pattern"), "unexpected error: {err}");
    }

    #[test]
    fn cheer_kind_parses_and_filters() {
        assert_eq!(MsgKind::parse("cheer"), Some(MsgKind::Cheer));
        assert_eq!(MsgKind::Cheer.name(), "cheer");
        assert_eq!(MsgKind::parse("bits"), None);

        // message_type allowlist of just "cheer" drops a normal message but
        // keeps a cheer.
        let mut c = cfg();
        c.message_type = Some(list(Mode::Allowlist, &["cheer"]));
        let f = CompiledFilters::compile(&c).unwrap();
        assert!(f.permits(&ctx(MsgKind::Cheer, "u", "U", "1", &[], "hi")));
        assert!(!f.permits(&ctx(MsgKind::Message, "u", "U", "1", &[], "hi")));
    }
}
