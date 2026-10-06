//! Message filtering: override rules plus compiled allowlist/denylist
//! dimensions.
//!
//! Pure evaluation logic — no I/O, no state. `CompiledFilters::compile`
//! turns config into matchers; `permits` is the single decision point
//! the engine calls for every message/event: override rules decide
//! first (first match wins), the AND-combined dimensions only for
//! messages no rule claimed.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use quiver_twitch::Badge;
use regex::Regex;

use crate::config::{FilterOverride, FiltersConfig, ListFilter, Mode, OverrideAction};

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

/// One compiled override rule. Absent conditions are wildcards; a rule
/// with no conditions at all never matches (parity with an inactive
/// dimension: empty items = the rule does nothing).
#[derive(Debug)]
struct CompiledOverride {
    allow: bool,
    user_id: Option<String>,
    username: Option<Regex>,
    display_name: Option<Regex>,
    content: Option<Regex>,
}

impl CompiledOverride {
    fn compile(rule: &FilterOverride) -> Result<Self, String> {
        // Same defense-in-depth as RegexDim::compile: validate() already
        // rejects these, but programmatic construction must not slip
        // through — an empty pattern matches every message.
        let compile_pat = |field: &str, pat: &Option<String>| -> Result<Option<Regex>, String> {
            match pat {
                None => Ok(None),
                Some(p) if p.is_empty() => Err(format!(
                    "filters.overrides: empty pattern in {field:?} matches every message \u{2014} \
                     refusing (likely a config error)"
                )),
                Some(p) => Ok(Some(Regex::new(p).map_err(|e| format!("{p:?}: {e}"))?)),
            }
        };
        if rule.user_id.as_deref() == Some("") {
            return Err(
                "filters.overrides: empty user id matches nothing \u{2014} refusing \
                 (likely a config error)"
                    .to_string(),
            );
        }
        Ok(Self {
            allow: rule.action == OverrideAction::Allow,
            user_id: rule.user_id.clone(),
            username: compile_pat("username", &rule.username)?,
            display_name: compile_pat("display_name", &rule.display_name)?,
            content: compile_pat("content", &rule.content)?,
        })
    }

    /// All PRESENT conditions must match; no condition = inactive rule.
    fn matches(&self, ctx: &PermitCtx) -> bool {
        let mut any = false;
        if let Some(id) = &self.user_id {
            // Exact match — regex metacharacters stay literal, same as
            // the user_id base dimension.
            any = true;
            if ctx.user_id != id {
                return false;
            }
        }
        if let Some(re) = &self.username {
            any = true;
            if !re.is_match(ctx.login) {
                return false;
            }
        }
        if let Some(re) = &self.display_name {
            any = true;
            if !re.is_match(ctx.display_name) {
                return false;
            }
        }
        if let Some(re) = &self.content {
            any = true;
            if !re.is_match(ctx.content) {
                return false;
            }
        }
        any
    }
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
    /// Per-user rules, config order — evaluated BEFORE the dimensions.
    overrides: Vec<CompiledOverride>,
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
            overrides: cfg
                .overrides
                .iter()
                .map(CompiledOverride::compile)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }

    /// The single decision point. True = render.
    ///
    /// Order: override rules FIRST (config order, FIRST match wins and
    /// its allow/deny is the final decision), then the dimensions. The
    /// dimensions combine with AND; inactive dimensions always pass.
    /// An empty items list makes its dimension inactive by construction.
    pub fn permits(&self, ctx: &PermitCtx) -> bool {
        for rule in &self.overrides {
            if rule.matches(ctx) {
                return rule.allow;
            }
        }
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
    /// Sees the same override rules and dimensions as the pump.
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
    use crate::config::{FilterOverride, ListFilter, OverrideAction};

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

    // ---- overrides (per-user content rules) --------------------------------

    /// Compact rule builder; uncovered conditions stay None (wildcard).
    fn rule(
        action: OverrideAction,
        user_id: Option<&str>,
        content: Option<&str>,
    ) -> FilterOverride {
        FilterOverride {
            action,
            user_id: user_id.map(str::to_string),
            username: None,
            display_name: None,
            content: content.map(str::to_string),
        }
    }

    /// Scenario 1 (ground truth by hand): hide EVERYTHING from likh_bot
    /// via a user_id denylist, but the override allow rule hands
    /// `!шіхтар` (with arguments, case-insensitive) back to him.
    #[test]
    fn override_allow_beats_user_id_denylist() {
        let mut c = cfg();
        c.user_id = Some(list(Mode::Denylist, &["1538701825"]));
        c.overrides.push(rule(
            OverrideAction::Allow,
            Some("1538701825"),
            Some("(?i)^!шіхтар(?: |$)"),
        ));
        let f = CompiledFilters::compile(&c).unwrap();

        let bot = ("likh_bot", "LikH_bot", "1538701825");
        // The exception: bot's command renders despite the denylist.
        assert!(f.permits(&ctx(
            MsgKind::Message,
            bot.0,
            bot.1,
            bot.2,
            &[],
            "!шіхтар dnb set"
        )));
        // Everything else from the bot stays hidden.
        assert!(!f.permits(&ctx(MsgKind::Message, bot.0, bot.1, bot.2, &[], "hello")));
        assert!(!f.permits(&ctx(MsgKind::Message, bot.0, bot.1, bot.2, &[], "!діджей")));
        // Prefix trap: !шіхтарішка is a different command, not the exception.
        assert!(!f.permits(&ctx(
            MsgKind::Message,
            bot.0,
            bot.1,
            bot.2,
            &[],
            "!шіхтарішка"
        )));
        // A different user is unaffected (rule misses, denylist passes).
        assert!(f.permits(&ctx(
            MsgKind::Message,
            "likh_tar",
            "Likh_tar",
            "999",
            &[],
            "!шіхтар"
        )));
    }

    /// Scenario 2 (ground truth by hand): deny "купити крипту" for
    /// everyone EXCEPT one moderator — allow-override carves the hole
    /// out of the content denylist.
    #[test]
    fn override_allow_carves_exception_out_of_content_denylist() {
        let mut c = cfg();
        c.content = Some(list(Mode::Denylist, &["(?i)купити крипту"]));
        let mut mod_exception = rule(OverrideAction::Allow, None, Some("(?i)купити крипту"));
        mod_exception.username = Some("^likh_tar$".to_string());
        c.overrides.push(mod_exception);
        let f = CompiledFilters::compile(&c).unwrap();

        // The moderator passes the override (user + content both match).
        assert!(f.permits(&ctx(
            MsgKind::Message,
            "likh_tar",
            "Likh_tar",
            "1",
            &[],
            "КУПИТИ КРИПТУ зараз!"
        )));
        // Everyone else hits the denylist.
        assert!(!f.permits(&ctx(
            MsgKind::Message,
            "shill",
            "Shill",
            "2",
            &[],
            "купити крипту"
        )));
        // The exception is content-scoped too: moderator's normal text
        // falls through to the dimensions and passes them.
        assert!(f.permits(&ctx(
            MsgKind::Message,
            "likh_tar",
            "Likh_tar",
            "1",
            &[],
            "привіт"
        )));
        assert!(f.permits(&ctx(MsgKind::Message, "shill", "Shill", "2", &[], "привіт")));
    }

    /// Scenario 3 (ground truth by hand): show the secret text ONLY from
    /// user 1 — allow rule first, content-only deny rule second. Proves
    /// FIRST-match ordering: with last-match semantics the swapped list
    /// would decide the same way.
    #[test]
    fn first_matching_override_wins_not_last() {
        let show_only_for_one = rule(OverrideAction::Allow, Some("1"), Some("секрет"));
        let hide_for_everyone = rule(OverrideAction::Deny, None, Some("секрет"));

        let mut c = cfg();
        c.overrides = vec![show_only_for_one.clone(), hide_for_everyone.clone()];
        let f = CompiledFilters::compile(&c).unwrap();
        // User 1 hits the FIRST rule (allow) — the deny below never runs.
        assert!(f.permits(&ctx(MsgKind::Message, "one", "One", "1", &[], "секрет")));
        // Anyone else skips rule 1, hits rule 2 (deny).
        assert!(!f.permits(&ctx(MsgKind::Message, "two", "Two", "2", &[], "секрет")));
        // Unrelated text matches no rule → base dimensions (empty) pass.
        assert!(f.permits(&ctx(MsgKind::Message, "two", "Two", "2", &[], "hello")));

        // Swap the order: now the deny rule claims user 1's message too.
        c.overrides = vec![hide_for_everyone, show_only_for_one];
        let f = CompiledFilters::compile(&c).unwrap();
        assert!(!f.permits(&ctx(MsgKind::Message, "one", "One", "1", &[], "секрет")));
    }

    /// Scenario 4 (ground truth by hand): show everything from user 777
    /// (user_id allowlist) EXCEPT the spam content — an override deny
    /// must beat an allowlist dimension too.
    #[test]
    fn override_deny_beats_allowlist_dimensions() {
        let mut c = cfg();
        c.user_id = Some(list(Mode::Allowlist, &["777", "888"]));
        c.overrides
            .push(rule(OverrideAction::Deny, Some("777"), Some("(?i)spam")));
        let f = CompiledFilters::compile(&c).unwrap();

        // 777 keeps everything but the overridden content.
        assert!(f.permits(&ctx(MsgKind::Message, "a", "A", "777", &[], "hello")));
        assert!(!f.permits(&ctx(MsgKind::Message, "a", "A", "777", &[], "SPAM!")));
        // The deny rule is user-scoped: 888's spam passes the allowlist
        // untouched (the rule's user condition misses).
        assert!(f.permits(&ctx(MsgKind::Message, "b", "B", "888", &[], "spam")));
        // Users outside the allowlist still fail it.
        assert!(!f.permits(&ctx(MsgKind::Message, "c", "C", "999", &[], "hello")));
    }

    /// Parity with the dimensions: a rule with no conditions is INACTIVE
    /// (the analogue of empty items) — a condition-less deny must not
    /// become "drop all chat".
    #[test]
    fn rule_without_conditions_is_inactive() {
        let mut c = cfg();
        c.user_id = Some(list(Mode::Denylist, &["1"]));
        c.overrides.push(rule(OverrideAction::Deny, None, None));
        let f = CompiledFilters::compile(&c).unwrap();

        // The empty deny rule decides nothing...
        assert!(f.permits(&ctx(MsgKind::Message, "u", "U", "2", &[], "anything")));
        // ...and the base denylist still applies.
        assert!(!f.permits(&ctx(MsgKind::Message, "u", "U", "1", &[], "anything")));
    }

    /// user_id in a rule is EXACT — regex metacharacters are literal,
    /// matching the base dimension's contract.
    #[test]
    fn override_user_id_is_exact_not_regex() {
        let mut c = cfg();
        c.overrides
            .push(rule(OverrideAction::Deny, Some("7*"), None));
        let f = CompiledFilters::compile(&c).unwrap();

        assert!(f.permits(&ctx(MsgKind::Message, "u", "U", "71092938", &[], "")));
        assert!(!f.permits(&ctx(MsgKind::Message, "u", "U", "7*", &[], "")));
    }

    /// login and display-name conditions compile as regexes and AND with
    /// each other inside one rule when both are present.
    #[test]
    fn override_matches_username_and_display_name_regex() {
        let mut by_login = rule(OverrideAction::Deny, None, None);
        by_login.username = Some("^nightbot$".to_string());
        let mut by_display = rule(OverrideAction::Deny, None, None);
        by_display.display_name = Some("^Stream.*".to_string());

        let mut c = cfg();
        c.overrides = vec![by_login, by_display];
        let f = CompiledFilters::compile(&c).unwrap();

        assert!(!f.permits(&ctx(
            MsgKind::Message,
            "nightbot",
            "NightBot",
            "1",
            &[],
            "hi"
        )));
        assert!(!f.permits(&ctx(
            MsgKind::Message,
            "streamelements",
            "StreamElements",
            "2",
            &[],
            "hi"
        )));
        assert!(f.permits(&ctx(
            MsgKind::Message,
            "melodieee__",
            "Melodieee__",
            "3",
            &[],
            "hi"
        )));
    }

    #[test]
    fn invalid_override_regex_is_a_compile_error() {
        let mut c = cfg();
        c.overrides
            .push(rule(OverrideAction::Deny, None, Some("[unclosed")));
        assert!(CompiledFilters::compile(&c).is_err());
    }

    /// Defense-in-depth for overrides (#11 parity): empty patterns are
    /// refused even when validation is bypassed by programmatic
    /// construction — content, username/display_name (match everything)
    /// and user_id (can never match → dead rule).
    #[test]
    fn empty_override_conditions_refuse_to_compile() {
        let mut c = cfg();
        c.overrides.push(rule(OverrideAction::Deny, None, Some("")));
        let err = CompiledFilters::compile(&c).expect_err("empty pattern must be refused");
        assert!(err.contains("empty pattern"), "unexpected error: {err}");

        c.overrides = vec![rule(OverrideAction::Allow, Some(""), None)];
        let err = CompiledFilters::compile(&c).expect_err("empty user id must be refused");
        assert!(err.contains("empty user id"), "unexpected error: {err}");

        let mut bad_login = rule(OverrideAction::Deny, None, None);
        bad_login.username = Some(String::new());
        c.overrides = vec![bad_login];
        let err = CompiledFilters::compile(&c).expect_err("empty pattern must be refused");
        assert!(err.contains("empty pattern"), "unexpected error: {err}");
    }

    /// The redemption/EventSub gate must see the same overrides as the
    /// pump — it calls permits() on the shared compiled filters.
    #[test]
    fn permits_event_sees_overrides() {
        let mut c = cfg();
        c.user_id = Some(list(Mode::Denylist, &["1538701825"]));
        c.overrides.push(rule(
            OverrideAction::Allow,
            Some("1538701825"),
            Some("(?i)^!шіхтар(?: |$)"),
        ));
        let shared: SharedCompiled = std::sync::Arc::new(std::sync::RwLock::new(Some(
            CompiledFilters::compile(&c).unwrap(),
        )));

        assert!(CompiledFilters::permits_event(
            &shared,
            MsgKind::Redeem,
            "likh_bot",
            "LikH_bot",
            "1538701825",
            "!шіхтар настрій"
        ));
        assert!(!CompiledFilters::permits_event(
            &shared,
            MsgKind::Redeem,
            "likh_bot",
            "LikH_bot",
            "1538701825",
            "будь-який текст"
        ));
        // No compiled filters = everything passes (unchanged contract).
        let empty: SharedCompiled = std::sync::Arc::new(std::sync::RwLock::new(None));
        assert!(CompiledFilters::permits_event(
            &empty,
            MsgKind::Follow,
            "u",
            "U",
            "9",
            ""
        ));
    }
}
