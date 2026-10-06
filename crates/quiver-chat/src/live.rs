//! Live config view + reload diffing.
//!
//! `LiveConfig` is the shared, hot-swappable snapshot of the running
//! configuration. `planned_actions` is a PURE function deciding what must
//! happen when the on-disk config changes — fully table-tested, no IO.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::config::{ChatConfig, EmotesConfig};

/// The subset of config that can change at runtime (everything except
/// things that only matter at process start).
#[derive(Debug, Clone, PartialEq)]
pub struct LiveConfig {
    pub listen: String,
    pub widget_dist: Option<PathBuf>,
    pub channel: String,
    pub creds: Option<(String, String)>,
    pub theme: crate::config::ThemeConfig,
    /// Emote source toggles; widget-facing via meta.
    pub emotes: EmotesConfig,
    /// Message filtering rules; engine-facing.
    pub filters: crate::config::FiltersConfig,
    /// Custom badge definitions (per-role/per-user); meta-facing.
    pub badges: Option<crate::config::CustomBadgesConfig>,
}

impl From<&ChatConfig> for LiveConfig {
    fn from(cfg: &ChatConfig) -> Self {
        Self {
            listen: cfg.server.listen.clone(),
            widget_dist: cfg.server.widget_dist.clone(),
            channel: cfg.twitch.channel.clone(),
            creds: cfg
                .twitch
                .client_id
                .clone()
                .zip(cfg.twitch.client_secret.clone()),
            theme: cfg.theme.clone(),
            emotes: cfg.emotes.clone(),
            filters: cfg.filters.clone(),
            badges: cfg.badges.clone(),
        }
    }
}

pub type SharedLive = Arc<RwLock<LiveConfig>>;

/// One runtime effect of a config change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Apply new max_messages cap (may evict + announce).
    SetMax(u32),
    /// Join a different Twitch channel (old one parted, history cleared).
    SwapChannel(String),
    /// Widget frontend directory changed — re-target the file watcher.
    SetWidgetDist(Option<PathBuf>),
    /// Credentials changed — refetch the badge URL map.
    RefreshBadges,
    /// Channel or credentials changed — third-party emote set differs.
    RefreshEmotes,
    /// Custom badge definitions changed — re-resolve cache.
    ResolveBadges,
    /// Custom CSS source changed — re-resolve (inline / file / http).
    RefreshCss,
    /// Filtering rules changed — recompile engine matchers.
    ApplyFilters,
    /// Push fresh meta (theme/badges/custom_css) to connected clients.
    BroadcastMeta,
    /// Bind address changed — restart the HTTP listener.
    Rebind,
}

/// Pure diff between two live views → ordered action list.
///
/// Canonical order (tested): SetMax → SetWidgetDist → SwapChannel →
/// RefreshEmotes → RefreshBadges → BroadcastMeta → Rebind. Rebind runs
/// last so everything else settles before the listener moves.
pub fn planned_actions(old: &LiveConfig, new: &LiveConfig) -> Vec<Action> {
    let mut out = Vec::new();

    if old.theme.max_messages != new.theme.max_messages {
        out.push(Action::SetMax(new.theme.max_messages));
    }
    if old.widget_dist != new.widget_dist {
        out.push(Action::SetWidgetDist(new.widget_dist.clone()));
    }
    let mut broadcast_meta = old.theme != new.theme || old.emotes != new.emotes;
    if old.channel != new.channel {
        out.push(Action::SwapChannel(new.channel.clone()));
        // Per-channel data sets: the 7TV/BTTV/FFZ emote map AND the badge
        // URL map (helix resolves global badges + channel overlay). A swap
        // must refresh both against the NEW channel, and meta carries the
        // badge map — so force a BroadcastMeta so the widget learns the new
        // channel's badges and re-renders existing rows.
        out.push(Action::RefreshEmotes);
        out.push(Action::RefreshBadges);
        broadcast_meta = true;
    }
    if old.creds != new.creds {
        // Channel change above already scheduled both refreshes.
        if !out.contains(&Action::RefreshBadges) {
            out.push(Action::RefreshBadges);
        }
        if !out.contains(&Action::RefreshEmotes) {
            out.push(Action::RefreshEmotes);
        }
        // Badge URLs live in meta, so a credential swap implies re-push.
        broadcast_meta = true;
    }
    if old.filters != new.filters {
        out.push(Action::ApplyFilters);
    }
    // An ACTIVE custom_css (inline or URI) re-resolves on every reload —
    // cheap for inline (clone) and file reads, and it makes source-file
    // edits flow through on the next config save ("easy swapping").
    if old.theme.custom_css.is_some() || new.theme.custom_css.is_some() {
        out.push(Action::RefreshCss);
    }
    if old.badges != new.badges {
        out.push(Action::ResolveBadges);
        broadcast_meta = true;
    }
    if broadcast_meta {
        out.push(Action::BroadcastMeta);
    }
    if old.listen != new.listen {
        out.push(Action::Rebind);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EmotesConfig, ThemeConfig};

    /// Hand-built live views; ground truth per case is written inline.
    fn live(
        listen: &str,
        dist: Option<&str>,
        channel: &str,
        client_id: Option<&str>,
        font: u32,
        max: u32,
        lifetime: u64,
    ) -> LiveConfig {
        LiveConfig {
            listen: listen.to_string(),
            widget_dist: dist.map(PathBuf::from),
            channel: channel.to_string(),
            creds: client_id.map(|id| (id.to_string(), "s".to_string())),
            theme: ThemeConfig {
                font_size_px: font,
                max_messages: max,
                message_lifetime_secs: lifetime,
                custom_css: None,
                role_css: None,
                overflow_mode: crate::config::OverflowMode::Prune,
                event_banner_secs: 8,
            },
            emotes: EmotesConfig::default(),
            filters: crate::config::FiltersConfig::default(),
            badges: None,
        }
    }

    #[test]
    fn identical_configs_produce_no_actions() {
        let c = live("a:1", None, "chan", Some("id"), 18, 30, 60);
        assert_eq!(planned_actions(&c, &c), Vec::new());
    }

    #[test]
    fn font_change_broadcasts_only() {
        let a = live("a:1", None, "chan", Some("id"), 18, 30, 60);
        let b = live("a:1", None, "chan", Some("id"), 24, 30, 60);
        assert_eq!(planned_actions(&a, &b), vec![Action::BroadcastMeta]);
    }

    #[test]
    fn max_change_sets_cap_and_broadcasts() {
        let a = live("a:1", None, "chan", Some("id"), 18, 30, 60);
        let b = live("a:1", None, "chan", Some("id"), 18, 50, 60);
        assert_eq!(
            planned_actions(&a, &b),
            vec![Action::SetMax(50), Action::BroadcastMeta]
        );
    }

    #[test]
    fn channel_change_refreshes_channel_scoped_data_and_broadcasts() {
        let a = live("a:1", None, "one", Some("id"), 18, 30, 60);
        let b = live("a:1", None, "two", Some("id"), 18, 30, 60);
        assert_eq!(
            planned_actions(&a, &b),
            vec![
                Action::SwapChannel("two".to_string()),
                Action::RefreshEmotes,
                Action::RefreshBadges,
                Action::BroadcastMeta,
            ]
        );
    }

    #[test]
    fn credential_change_refreshes_badges_and_broadcasts() {
        let a = live("a:1", None, "chan", Some("id1"), 18, 30, 60);
        let b = live("a:1", None, "chan", Some("id2"), 18, 30, 60);
        assert_eq!(
            planned_actions(&a, &b),
            vec![
                Action::RefreshBadges,
                Action::RefreshEmotes,
                Action::BroadcastMeta,
            ]
        );
    }

    #[test]
    fn listen_change_rebinds_only() {
        let a = live("a:1", None, "chan", Some("id"), 18, 30, 60);
        let b = live("b:2", None, "chan", Some("id"), 18, 30, 60);
        assert_eq!(planned_actions(&a, &b), vec![Action::Rebind]);
    }

    #[test]
    fn widget_dist_change_retargets_watcher() {
        let a = live("a:1", None, "chan", Some("id"), 18, 30, 60);
        let b = live("a:1", Some("/tmp/x"), "chan", Some("id"), 18, 30, 60);
        assert_eq!(
            planned_actions(&a, &b),
            vec![Action::SetWidgetDist(Some(PathBuf::from("/tmp/x")))]
        );
    }

    #[test]
    fn widget_dist_removal_retargets_to_none() {
        let a = live("a:1", Some("/tmp/x"), "chan", Some("id"), 18, 30, 60);
        let b = live("a:1", None, "chan", Some("id"), 18, 30, 60);
        assert_eq!(planned_actions(&a, &b), vec![Action::SetWidgetDist(None)]);
    }

    #[test]
    fn lifetime_change_broadcasts_only() {
        let a = live("a:1", None, "chan", Some("id"), 18, 30, 60);
        let b = live("a:1", None, "chan", Some("id"), 18, 30, 10);
        assert_eq!(planned_actions(&a, &b), vec![Action::BroadcastMeta]);
    }

    // Ground truth by hand: full change hits every action in canonical order.
    #[test]
    fn kitchen_sink_follows_canonical_order() {
        let a = live("a:1", None, "one", None, 18, 30, 60);
        let b = live("b:2", Some("/x"), "two", Some("id"), 22, 40, 15);
        assert_eq!(
            planned_actions(&a, &b),
            vec![
                Action::SetMax(40),
                Action::SetWidgetDist(Some(PathBuf::from("/x"))),
                Action::SwapChannel("two".to_string()),
                Action::RefreshEmotes,
                Action::RefreshBadges,
                Action::BroadcastMeta,
                Action::Rebind,
            ]
        );
    }

    // custom_css edit alone = meta broadcast only.
    #[test]
    fn custom_css_change_broadcasts_only() {
        let mut a = live("a:1", None, "c", Some("id"), 18, 30, 60);
        let mut b = a.clone();
        assert_eq!(planned_actions(&a, &b), Vec::new());
        b.theme.custom_css = Some(crate::config::CustomCssSource::Inline(".msg{}".to_string()));
        a.theme.custom_css = None;
        // custom_css change → RefreshCss action (not just BroadcastMeta).
        assert_eq!(
            planned_actions(&a, &b),
            vec![Action::RefreshCss, Action::BroadcastMeta]
        );
    }

    /// Overrides are part of FiltersConfig (PartialEq), so editing ONLY
    /// the rules must recompile the engine matchers — no other action.
    #[test]
    fn override_change_reapplies_filters() {
        use crate::config::{FilterOverride, OverrideAction};

        let a = live("a:1", None, "c", Some("id"), 18, 30, 60);
        let mut b = a.clone();
        assert_eq!(planned_actions(&a, &b), Vec::new());
        b.filters.overrides = vec![FilterOverride {
            action: OverrideAction::Allow,
            user_id: Some("1538701825".to_string()),
            username: None,
            display_name: None,
            content: Some("(?i)^!шіхтар(?: |$)".to_string()),
        }];
        assert_eq!(planned_actions(&a, &b), vec![Action::ApplyFilters]);
    }
}
