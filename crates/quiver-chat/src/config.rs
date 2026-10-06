//! quiver-chat's RON configuration model.
//!
//! Verbose and explicit on purpose: this file IS the tool's interface.
//! The future Quiver UI generates configs of this shape from the JSON
//! Schema exported via `quiver-chat --print-schema`.

use std::collections::HashMap;
use std::path::PathBuf;

use indexmap::IndexMap;
use quiver_config::{Validate, ValidationIssue, require};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// quiver-chat configuration. Verbose on purpose: this file IS the
/// tool's interface. The future Quiver UI generates files of this shape
/// from the JSON Schema exported via `quiver-chat --print-schema`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct ChatConfig {
    /// HTTP server and widget frontend location.
    pub server: ServerConfig,
    /// Twitch chat source and optional API credentials.
    pub twitch: TwitchConfig,
    /// Visual appearance of rendered messages.
    pub theme: ThemeConfig,
    /// Emote source toggles. All on by default; disabling a provider makes
    /// its emojis vanish from rendered messages entirely.
    #[serde(default)]
    pub emotes: EmotesConfig,
    /// Message filtering (allowlist/denylist per dimension). Absent = no
    /// filtering at all.
    #[serde(default)]
    pub filters: FiltersConfig,
    /// Custom per-role / per-user badge images with explicit priority and
    /// server-side caching. Absent = no custom badges.
    #[serde(default)]
    pub badges: Option<CustomBadgesConfig>,
}

fn default_true() -> bool {
    true
}

/// Per-provider emote switches, all enabled by default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EmotesConfig {
    /// First-party Twitch emotes (rendered from per-message ranges).
    #[serde(default = "default_true")]
    pub twitch: bool,
    /// Native unicode emoji characters. `false` strips them from text.
    #[serde(default = "default_true")]
    pub unicode: bool,
    /// 7TV global + channel sets.
    #[serde(default = "default_true")]
    pub seventv: bool,
    /// BetterTTV global + channel/shared sets.
    #[serde(default = "default_true")]
    pub bttv: bool,
    /// FrankerFaceZ global + room sets.
    #[serde(default = "default_true")]
    pub ffz: bool,
}

impl Default for EmotesConfig {
    fn default() -> Self {
        Self {
            twitch: true,
            unicode: true,
            seventv: true,
            bttv: true,
            ffz: true,
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:4783".to_string(),
            // Relative to where quiver-chat runs — works out of the box
            // from the monorepo root.
            widget_dist: Some(PathBuf::from("crates/quiver-chat/widget")),
        }
    }
}

impl Default for TwitchConfig {
    fn default() -> Self {
        Self {
            channel: "your_channel_here".to_string(),
            client_id: None,
            client_secret: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ServerConfig {
    /// `host:port` the widget HTTP+WS server binds to.
    pub listen: String,
    /// Optional absolute path to the built widget frontend directory.
    /// Unset → placeholder page is served at `/`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub widget_dist: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TwitchConfig {
    /// Twitch channel login to read chat from (read-only anonymous join).
    pub channel: String,
    /// Twitch API client id — enables badge image URLs (Helix lookup).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Twitch API client secret. Keep ONLY in git-ignored local configs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
}

/// Custom CSS source: inline text OR a URI to load.
/// Untagged union: a bare string is inline CSS (historical form);
/// a struct with `uri` loads from file:// or http(s):// using the same
/// resolution as badges (file passthrough, http fetched) — engine-side
/// at load/reload, so the widget always receives resolved text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum CustomCssSource {
    /// Inline CSS text: `custom_css: Some("…")`.
    Inline(String),
    /// Load from a URI: `custom_css: Some(( uri: "file:///…" ))`.
    File { uri: String },
}

impl CustomCssSource {
    pub fn uri(&self) -> Option<&str> {
        match self {
            Self::Inline(_) => None,
            Self::File { uri } => Some(uri),
        }
    }

    /// Resolve the source to CSS text. Inline returns as-is; URIs call
    /// the badge-style resolver and propagate its errors.
    pub(crate) async fn resolve(
        &self,
        http: &reqwest::Client,
        cache_dir: &std::path::Path,
    ) -> Result<String, String> {
        match self {
            Self::Inline(text) => Ok(text.clone()),
            Self::File { uri } => {
                let bytes = crate::serve::fetch_css_bytes(uri, http, cache_dir).await?;
                String::from_utf8(bytes).map_err(|e| format!("{uri}: not valid UTF-8: {e}"))
            }
        }
    }
}

/// Container overflow behavior. Default: prune (tight chat, no scrollbar).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OverflowMode {
    /// Remove oldest `.msg` nodes until the container fits. Event banners
    /// and the newest message are never pruned.
    #[default]
    Prune,
    /// Keep messages until the count cap; scroll pinned to the bottom.
    Scroll,
}

/// Visual appearance of rendered messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ThemeConfig {
    /// Base font size for messages, in CSS pixels.
    pub font_size_px: u32,
    /// Maximum number of messages kept in the visible history.
    pub max_messages: u32,
    /// Seconds a message stays visible before fading out.
    pub message_lifetime_secs: u64,
    /// Your own CSS, applied after the built-in widget styles. Inline
    /// string, or load from file:// / http(s):// via `( uri: … )` —
    /// URI sources resolve like badges (file passthrough, http fetched).
    /// Linted with lightningcss at load/reload (advisory warn).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_css: Option<CustomCssSource>,
    /// Per-role CSS: Twitch badge set id -> CSS snippet applied to messages
    /// whose sender carries that badge. Message rows get `role-<id>` classes,
    /// snippets are injected before custom_css. Common ids: broadcaster,
    /// moderator, vip, subscriber, founder.
    ///
    /// `IndexMap`, not `HashMap`: every snippet sets the same custom property
    /// with the same specificity and `!important`, so the LAST injected one
    /// wins. The widget injects them in map order, and `serde_json` is built
    /// with `preserve_order` — a `HashMap` here would serialise in random
    /// order and hand multi-role senders an arbitrary colour, re-rolled on
    /// every reload. See #108.
    ///
    /// Order is therefore meaningful: a sender carrying several roles takes
    /// the colour of the LAST entry that matches them. Put `broadcaster`
    /// after `subscriber`/`founder` to keep the broadcaster colour.
    // JSON Schema cannot express key order, so the schema keeps the
    // order-insensitive shape; the order itself only matters on the wire.
    #[schemars(with = "Option<std::collections::BTreeMap<String, String>>")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role_css: Option<IndexMap<String, String>>,
    /// What happens when messages overflow the container height:
    /// `prune` removes oldest messages until everything fits (OBS
    /// default), `scroll` pins a scrollable chat to the bottom.
    #[serde(default)]
    pub overflow_mode: OverflowMode,
    /// Seconds an event banner (redeem, hype train, prediction, poll, …)
    /// stays in the chat flow before fading out. `0` keeps banners until
    /// they are scrolled away by later messages (no auto-dismiss).
    #[serde(default = "default_event_banner_secs")]
    pub event_banner_secs: u64,
}

fn default_event_banner_secs() -> u64 {
    8
}

/// Lint user CSS through a real parser (lightningcss).
/// Advisory only: Ok means "browser will almost certainly accept it".
pub fn lint_custom_css(css: &str) -> Result<(), String> {
    let options = lightningcss::stylesheet::ParserOptions::default();
    lightningcss::stylesheet::StyleSheet::parse(css, options)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Log a warning when user CSS fails the lint. Never fatal.
pub(crate) fn report_css_lint(css: Option<&str>, role_css: Option<&IndexMap<String, String>>) {
    if let Some(css) = css
        && let Err(e) = lint_custom_css(css)
    {
        tracing::warn!(
            error = %e,
            "custom_css looks broken — browsers will ignore invalid rules"
        );
    }
    if let Some(map) = role_css {
        for (role, snippet) in map {
            if let Err(e) = lint_custom_css(snippet) {
                tracing::warn!(
                    role = %role,
                    error = %e,
                    "role_css snippet looks broken — browsers will ignore invalid rules"
                );
            }
        }
    }
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            font_size_px: 18,
            max_messages: 30,
            message_lifetime_secs: 60,
            custom_css: None,
            role_css: None,
            overflow_mode: OverflowMode::Prune,
            event_banner_secs: default_event_banner_secs(),
        }
    }
}

/// Filter list direction. Allowlist = at least one item must match;
/// denylist = no item may match. An EMPTY item list makes the whole
/// dimension inactive (an empty allowlist meaning "drop everything"
/// is a footgun we refuse to arm).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Allowlist,
    Denylist,
}

/// One filter dimension: a direction plus its string items.
/// Item interpretation depends on the dimension (regex / exact id /
/// kind name / badge id) — see `FiltersConfig` field docs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ListFilter {
    pub mode: Mode,
    pub items: Vec<String>,
}

/// What a matching override rule does: `allow` renders the message (the
/// exception — beats the denylist dimensions), `deny` drops it (an extra
/// ban — beats the allowlist dimensions). An unknown action name fails
/// config parsing, exactly like an unknown `message_type` kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OverrideAction {
    Allow,
    Deny,
}

/// One per-user content rule (the `filters.overrides` list entry).
///
/// Every PRESENT condition must match (AND); an absent condition is a
/// wildcard. A rule with NO conditions at all is inactive and never
/// decides — the parity of an empty `items` list in the base dimensions.
/// Rules run BEFORE the base dimensions, in config order: the FIRST
/// matching rule gives the final decision and the dimensions below are
/// never consulted for that message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FilterOverride {
    /// What the matching message gets: `allow` (render) or `deny` (drop).
    pub action: OverrideAction,
    /// EXACT match against the sender's Twitch user id (numeric string);
    /// regex metacharacters are literal, as in `filters.user_id`.
    #[serde(default)]
    pub user_id: Option<String>,
    /// Regex matched against the sender's login name (always lowercase).
    #[serde(default)]
    pub username: Option<String>,
    /// Regex matched against the sender's display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Regex matched against the message text (events have empty text).
    #[serde(default)]
    pub content: Option<String>,
}

/// Message filtering rules. `overrides` are checked first (first match
/// wins, final decision); the remaining dimensions combine with AND: a
/// message is rendered only when EVERY active dimension passes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FiltersConfig {
    /// Regex matched against the sender's display name.
    pub display_name: Option<ListFilter>,
    /// Regex matched against the sender's login name (always lowercase).
    pub username: Option<ListFilter>,
    /// EXACT match against the sender's Twitch user id (numeric string).
    pub user_id: Option<ListFilter>,
    /// Regex matched against the message text. Events have empty text.
    /// Use patterns like `^!` to filter chat commands.
    pub content: Option<ListFilter>,
    /// Message kind names: message | sub | gift_sub | mystery_gift |
    /// raid. Unknown names are config errors.
    pub message_type: Option<ListFilter>,
    /// Badge ids carried by the sender — same vocabulary as role_css
    /// (moderator, vip, subscriber, ...). Events carry no badges in v1.
    pub role: Option<ListFilter>,
    /// Per-user content rules checked BEFORE the dimensions above: the
    /// FIRST rule whose conditions all match decides `allow`/`deny` for
    /// that message and the dimensions never run for it. This is how
    /// AND-combined dimensions get exceptions ("hide everything from
    /// this user except ...") and extra bans ("drop this text except
    /// for ..."). Empty list = no rules; a rule without any condition is
    /// inactive. Empty/invalid patterns are config errors, like the
    /// dimensions.
    #[serde(default)]
    pub overrides: Vec<FilterOverride>,
}

fn default_refresh_interval() -> u64 {
    86400 // 24h
}

fn default_badge_height() -> u32 {
    1 // em — native badge size, scales with font
}

/// One custom badge image: a remote URL with display metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CustomBadgeDefinition {
    /// Remote URL of the badge image (http:// or https://).
    pub uri: String,
    /// Lower value renders first in the badge row; ties broken by
    /// insertion order (JSON map key order).
    pub priority: u32,
    /// Display height in EM units (scales with the widget font size) —
    /// `1` renders at exactly native badge size. Absent = `1`.
    #[serde(default = "default_badge_height")]
    pub height: u32,
    /// Optional tooltip text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Per-identity badge assignment: custom badges to attach, plus whether to
/// hide Twitch's NATIVE badges. Semantics differ by table:
/// - in `per_role`: hides ONLY that role's own badge
///   (e.g. "moderator" hide_native → moderators lose the moderator badge,
///   keep subscriber/broadcaster/etc).
/// - in `per_user`: hides ALL native badges for that user.
///
/// Multiple matching assignments union their badges.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BadgeAssignment {
    /// Custom badge ids (from `definitions`) to attach.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub badges: Vec<String>,
    /// Conditional on the containing table (per_role = its own badge only,
    /// per_user = all native badges).
    #[serde(default)]
    pub hide_native: bool,
}

/// Custom badge system: per-role and/or per-user badge attachments.
/// Badge images are fetched server-side and cached by content hash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CustomBadgesConfig {
    /// Badge image definitions: id → {uri, priority, height, label?}.
    /// HTTP(S) URIs only; fetched once at startup/reload and cached
    /// by content hash at `cache_dir`.
    #[serde(default)]
    pub definitions: HashMap<String, CustomBadgeDefinition>,
    /// Twitch badge set id → assignment (custom badges + hide-native flag).
    #[serde(default)]
    pub per_role: HashMap<String, BadgeAssignment>,
    /// User id or login → assignment (custom badges + hide-native flag).
    #[serde(default)]
    pub per_user: HashMap<String, BadgeAssignment>,
    /// Cache directory override. Default: `$XDG_CACHE_HOME/Quiver/badges/`
    /// or `~/.cache/Quiver/badges/` if unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,
    /// Seconds between HTTP badge re-validations via conditional GET.
    /// Default 86400 (24 hours).
    #[serde(default = "default_refresh_interval")]
    pub refresh_interval_secs: u64,
}

impl Default for CustomBadgesConfig {
    fn default() -> Self {
        Self {
            definitions: HashMap::new(),
            per_role: HashMap::new(),
            per_user: HashMap::new(),
            cache_dir: None,
            refresh_interval_secs: 86400,
        }
    }
}

impl Validate for ChatConfig {
    fn validate(&self) -> Vec<ValidationIssue> {
        let mut out = Vec::new();

        require(
            !self.server.listen.is_empty(),
            "server.listen",
            "must not be empty",
            &mut out,
        );
        require(
            !self.twitch.channel.is_empty(),
            "twitch.channel",
            "must not be empty",
            &mut out,
        );
        require(
            self.twitch
                .channel
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "twitch.channel",
            "must contain only ASCII letters, digits and underscores (a Twitch login)",
            &mut out,
        );
        // Credentials are only meaningful as a pair.
        require(
            self.twitch.client_id.is_some() == self.twitch.client_secret.is_some(),
            if self.twitch.client_id.is_none() {
                "twitch.client_id"
            } else {
                "twitch.client_secret"
            },
            "client_id and client_secret must be set together",
            &mut out,
        );
        require(
            (8..=200).contains(&self.theme.font_size_px),
            "theme.font_size_px",
            "must be within 8..=200",
            &mut out,
        );
        require(
            (1..=1000).contains(&self.theme.max_messages),
            "theme.max_messages",
            "must be within 1..=1000",
            &mut out,
        );
                require(
            self.theme.message_lifetime_secs >= 1,
            "theme.message_lifetime_secs",
            "must be at least 1",
            &mut out,
        );
        require(
            self.theme.event_banner_secs <= 3600,
            "theme.event_banner_secs",
            "must be at most 3600 (0 = banners stay until scrolled away)",
            &mut out,
        );

        // Filters: regex patterns must compile; kind names must be real.
        // A broken filter is functional, not cosmetic — these are hard
        // validation errors, unlike advisory CSS lint.
        let f = &self.filters;
        for (name, dim) in [
            ("display_name", &f.display_name),
            ("username", &f.username),
            ("content", &f.content),
        ] {
            if let Some(dim) = dim {
                for item in &dim.items {
                    // Empty pattern matches EVERY string (position-0 match) —
                    // in denylist mode it silently blocks all chat, in
                    // allowlist mode it makes every other pattern dead.
                    // Almost certainly a config error: refuse it loudly.
                    if item.is_empty() {
                        out.push(ValidationIssue {
                            path: format!("filters.{name}.items"),
                            message:
                                "empty pattern matches every message — refusing (likely a config error)"
                                    .to_string(),
                        });
                        continue;
                    }
                    if let Err(e) = regex::Regex::new(item) {
                        out.push(ValidationIssue {
                            path: format!("filters.{name}.items"),
                            message: format!("invalid regex {item:?}: {e}"),
                        });
                    }
                }
            }
        }
        if let Some(dim) = &f.message_type {
            for item in &dim.items {
                if crate::filters::MsgKind::parse(item).is_none() {
                    out.push(ValidationIssue {
                        path: "filters.message_type.items".to_string(),
                        message: format!(
                            "unknown message kind {item:?} \u{2014} expected one of: \
                             message, sub, gift_sub, mystery_gift, raid, redeem,
                             hype_train, prediction, poll, follow"
                        ),
                    });
                }
            }
        }
        // Override rules: same contract as the dimensions — empty or
        // non-compiling patterns are hard errors (they would silently
        // re-decide moderation outcomes otherwise). `user_id` is an exact
        // match: an empty value can never match a real id, so the rule
        // would be dead code. An unknown `action` never reaches this
        // point — the enum rejects it at parse time.
        for (i, rule) in f.overrides.iter().enumerate() {
            let base = format!("filters.overrides[{i}]");
            for (field, value) in [
                ("user_id", &rule.user_id),
                ("username", &rule.username),
                ("display_name", &rule.display_name),
                ("content", &rule.content),
            ] {
                let Some(value) = value else { continue };
                if value.is_empty() {
                    out.push(ValidationIssue {
                        path: format!("{base}.{field}"),
                        message: if field == "user_id" {
                            "empty user id matches nothing \u{2014} refusing (likely a config error)"
                                .to_string()
                        } else {
                            "empty pattern matches every message \u{2014} refusing (likely a config error)"
                                .to_string()
                        },
                    });
                    continue;
                }
                if field != "user_id"
                    && let Err(e) = regex::Regex::new(value)
                {
                    out.push(ValidationIssue {
                        path: format!("{base}.{field}"),
                        message: format!("invalid regex {value:?}: {e}"),
                    });
                }
            }
        }

        // Custom badges: URI format, priority, height, references.
        if let Some(badges) = &self.badges {
            require(
                badges.refresh_interval_secs > 0,
                "badges.refresh_interval_secs",
                "must be at least 1",
                &mut out,
            );
            for (id, defn) in &badges.definitions {
                if !defn.uri.starts_with("http://")
                    && !defn.uri.starts_with("https://")
                    && !defn.uri.starts_with("file://")
                {
                    out.push(ValidationIssue {
                        path: format!("badges.definitions.{id}.uri"),
                        message: "must start with http://, https:// or file://".to_string(),
                    });
                }
                if defn.priority == 0 {
                    out.push(ValidationIssue {
                        path: format!("badges.definitions.{id}.priority"),
                        message: "must be greater than 0".to_string(),
                    });
                }
                if defn.height == 0 {
                    out.push(ValidationIssue {
                        path: format!("badges.definitions.{id}.height"),
                        message: "must be greater than 0".to_string(),
                    });
                }
            }
            for (role, assignment) in &badges.per_role {
                for id in &assignment.badges {
                    if !badges.definitions.contains_key(id.as_str()) {
                        out.push(ValidationIssue {
                            path: format!("badges.per_role.{role}.badges"),
                            message: format!("references undefined badge {id:?}"),
                        });
                    }
                }
            }
            for (uid, assignment) in &badges.per_user {
                for id in &assignment.badges {
                    if !badges.definitions.contains_key(id.as_str()) {
                        out.push(ValidationIssue {
                            path: format!("badges.per_user.{uid}.badges"),
                            message: format!("references undefined badge {id:?}"),
                        });
                    }
                }
            }
        }

        out
    }
}
