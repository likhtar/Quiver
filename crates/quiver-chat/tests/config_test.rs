//! Integration tests for quiver-chat's config model.
//! Ground truth is hand-written constants; no clever computation.

use quiver_chat::config::ChatConfig;
use quiver_config::{Validate, load_from_path, parse_str, to_string};

#[test]
fn valid_fixture_parses_and_validates_clean() {
    let cfg: ChatConfig =
        load_from_path(std::path::Path::new("tests/fixtures/chat.ron")).expect("must parse");

    // Hand-written ground truth matching tests/fixtures/chat.ron exactly.
    assert_eq!(cfg.server.listen, "127.0.0.1:4783");
    assert_eq!(cfg.server.widget_dist, None);
    assert_eq!(cfg.twitch.channel, "quiverdev");
    assert_eq!(cfg.theme.font_size_px, 18);
    assert_eq!(cfg.theme.max_messages, 30);
    assert_eq!(cfg.theme.message_lifetime_secs, 60);

    assert_eq!(cfg.validate(), Vec::new());
}

#[test]
fn sample_config_parses_and_validates_clean() {
    // The printed sample must be usable as-is (minus channel rename):
    // "your_channel_here" is a valid login shape, so zero issues expected.
    let text = quiver_config::generate_default::<ChatConfig>("quiver-chat --sample-config")
        .expect("generation succeeds");
    let cfg: ChatConfig = parse_str(&text).expect("generated must parse");
    assert_eq!(cfg.validate(), Vec::new());

    // Discoverability contract: key doc comments must appear verbatim.
    for needle in [
        "host:port",
        "Keep ONLY in git-ignored local configs",
        "Base font size",
        "snippets are injected before custom_css",
        "strips them from text",
    ] {
        assert!(text.contains(needle), "missing doc fragment: {needle}");
    }
}

#[test]
fn invalid_values_report_exact_issue_paths() {
    let raw = r#"
(
    server: ( listen: "", ),
    twitch: ( channel: "has space!", ),
    theme: (
        font_size_px: 500,
        max_messages: 0,
        message_lifetime_secs: 0,
    ),
)
"#;
    let cfg: ChatConfig = parse_str(raw).expect("must parse");
    let issues = cfg.validate();
    let mut paths: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
    paths.sort_unstable();
    // Exactly the five planted violations ("has space!" is non-empty, so
    // twitch.channel trips ONLY the charset rule).
    assert_eq!(
        paths,
        vec![
            "server.listen",
            "theme.font_size_px",
            "theme.max_messages",
            "theme.message_lifetime_secs",
            "twitch.channel",
        ]
    );
}

#[test]
fn ron_roundtrip_preserves_optional_field_semantics() {
    let cfg = ChatConfig::default();
    let text = to_string(&cfg).expect("serialize");
    let again: ChatConfig = parse_str(&text).expect("re-parse");
    assert_eq!(cfg, again);
}

// ---- custom_css ----------------------------------------------------------

#[test]
fn raw_string_css_roundtrips_with_quotes_and_newlines() {
    // RON raw string: no escaping of quotes or newlines.
    // Outer Rust literal uses ## because the RON itself contains "#.
    let raw = r##"
(
    server: ( listen: "127.0.0.1:1", ),
    twitch: ( channel: "chan", ),
    theme: (
        font_size_px: 18,
        max_messages: 30,
        message_lifetime_secs: 60,
        custom_css: Some(r#".msg { content: "x"; opacity: 0.5; }"#),
    ),
)
"##;
    let cfg: ChatConfig = parse_str(raw).expect("raw string CSS must parse");
    let css = cfg.theme.custom_css.expect("css present");
    // Untagged union: a bare string deserializes into the Inline variant.
    assert_eq!(
        css,
        quiver_chat::config::CustomCssSource::Inline(
            r#".msg { content: "x"; opacity: 0.5; }"#.to_string()
        )
    );
}

/// Ground truth BY HAND: definitions + per_role without per_user must
/// parse (empty maps are the default — not required fields).
#[test]
fn badges_parse_without_per_user() {
    let raw = r##"
(
    server: ( listen: "127.0.0.1:1", ),
    twitch: ( channel: "chan", ),
    theme: (
        font_size_px: 18,
        max_messages: 30,
        message_lifetime_secs: 60,
    ),
    badges: Some((
        definitions: {
            // height omitted → defaults to 1 (em, native badge size).
            "vip-star": ( uri: "https://cdn.example.com/vip.png", priority: 10 ),
        },
        per_role: {
            "vip": ( badges: ["vip-star"], hide_native: true ),
        },
    )),
)
"##;
    let cfg: ChatConfig = parse_str(raw).expect("config without per_user must parse");
    let badges = cfg.badges.as_ref().expect("badges present");
    assert!(badges.per_user.is_empty(), "per_user defaults to empty");
    assert_eq!(badges.per_role.len(), 1);
    assert_eq!(badges.definitions.len(), 1);
    assert_eq!(
        badges.definitions["vip-star"].height, 1,
        "height must default to 1 em when omitted"
    );
    // And validation passes (references resolve within definitions).
    assert_eq!(cfg.validate(), Vec::new());
}

/// Ground truth BY HAND: the struct form selects the File(uri) variant.
#[test]
fn uri_form_deserializes_as_file_source() {
    let raw = r##"
(
    server: ( listen: "127.0.0.1:1", ),
    twitch: ( channel: "chan", ),
    theme: (
        font_size_px: 18,
        max_messages: 30,
        message_lifetime_secs: 60,
        custom_css: Some(( uri: "file:///tmp/theme.css" )),
    ),
)
"##;
    let cfg: ChatConfig = parse_str(raw).expect("uri form must parse");
    assert_eq!(
        cfg.theme.custom_css,
        Some(quiver_chat::config::CustomCssSource::File {
            uri: "file:///tmp/theme.css".to_string()
        })
    );
    assert_eq!(
        cfg.theme.custom_css.as_ref().and_then(|c| c.uri()),
        Some("file:///tmp/theme.css")
    );
}

#[test]
fn lint_accepts_valid_and_unknown_property_css() {
    assert_eq!(
        quiver_chat::config::lint_custom_css(".msg { opacity: 0.9; }"),
        Ok(())
    );
    // Unknown properties are ignored by browsers — parser must agree.
    assert_eq!(
        quiver_chat::config::lint_custom_css(".a { blah-blah: 12px; }"),
        Ok(())
    );
}

#[test]
fn lint_rejects_broken_css_with_position_info() {
    let err = quiver_chat::config::lint_custom_css("} { color: red; ")
        .expect_err("stray brace must be rejected");
    assert!(!err.is_empty());
}

#[test]
fn lint_tolerates_unclosed_final_block_like_browsers_do() {
    // Browsers apply a truncated final rule; the parser matches that
    // behavior, so the lint must not flag it.
    assert_eq!(
        quiver_chat::config::lint_custom_css(".msg { opacity: 0.9; "),
        Ok(())
    );
}

// ---- filters -------------------------------------------------------------

/// Regression for #11: an empty regex item must be rejected at validation,
/// since `Regex::new("")` matches every string — in denylist mode it would
/// silently silence ALL chat with no error anywhere.
#[test]
fn empty_filter_regex_item_is_rejected() {
    let raw = r##"
(
    server: ( listen: "127.0.0.1:1", ),
    twitch: ( channel: "chan", ),
    theme: (
        font_size_px: 18,
        max_messages: 30,
        message_lifetime_secs: 60,
    ),
    filters: (
        content: Some((
            mode: denylist,
            items: ["nightbot", ""],
        )),
    ),
)
"##;
    let cfg: ChatConfig = parse_str(raw).expect("must parse");
    let issues = cfg.validate();
    assert!(
        issues.iter().any(|i| {
            i.path == "filters.content.items" && i.message.contains("empty pattern")
        }),
        "empty regex item must be rejected, got: {issues:?}"
    );
}

/// Ground truth by hand: overrides parse field-by-field; a config
/// WITHOUT the block stays valid and defaults to zero rules (the field
/// is additive — old configs keep working unchanged).
#[test]
fn overrides_parse_and_absent_means_empty() {
    let raw = r##"
(
    server: ( listen: "127.0.0.1:1", ),
    twitch: ( channel: "chan", ),
    theme: (
        font_size_px: 18,
        max_messages: 30,
        message_lifetime_secs: 60,
    ),
    filters: (
        user_id: Some(( mode: denylist, items: ["1538701825"] )),
        overrides: [
            (
                action: allow,
                user_id: Some("1538701825"),
                content: Some("(?i)^!шіхтар(?: |$)"),
            ),
            (
                action: deny,
                username: Some("^spam_bot$"),
                display_name: Some("^Spam"),
            ),
        ],
    ),
)
"##;
    let cfg: ChatConfig = parse_str(raw).expect("overrides must parse");
    assert_eq!(cfg.validate(), Vec::new());

    let rules = &cfg.filters.overrides;
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0].action, quiver_chat::config::OverrideAction::Allow);
    assert_eq!(rules[0].user_id.as_deref(), Some("1538701825"));
    assert_eq!(rules[0].content.as_deref(), Some("(?i)^!шіхтар(?: |$)"));
    assert_eq!(rules[0].username, None);
    assert_eq!(rules[0].display_name, None);
    assert_eq!(rules[1].action, quiver_chat::config::OverrideAction::Deny);
    assert_eq!(rules[1].user_id, None);
    assert_eq!(rules[1].content, None);
    assert_eq!(rules[1].username.as_deref(), Some("^spam_bot$"));
    assert_eq!(rules[1].display_name.as_deref(), Some("^Spam"));

    // Backward compat: no overrides key anywhere → empty rule list.
    let old_raw = r##"
(
    server: ( listen: "127.0.0.1:1", ),
    twitch: ( channel: "chan", ),
    theme: (
        font_size_px: 18,
        max_messages: 30,
        message_lifetime_secs: 60,
    ),
    filters: (
        content: Some(( mode: denylist, items: ["^!"] )),
    ),
)
"##;
    let old: ChatConfig = parse_str(old_raw).expect("pre-overrides config must parse");
    assert!(old.filters.overrides.is_empty(), "absent = no rules");
    assert_eq!(old.validate(), Vec::new());
}

/// Empty / non-compiling override conditions are HARD validation errors
/// with per-field paths — same contract as the base dimensions (#11).
#[test]
fn invalid_override_conditions_report_exact_issue_paths() {
    let raw = r##"
(
    server: ( listen: "127.0.0.1:1", ),
    twitch: ( channel: "chan", ),
    theme: (
        font_size_px: 18,
        max_messages: 30,
        message_lifetime_secs: 60,
    ),
    filters: (
        overrides: [
            ( action: allow, content: Some("") ),
            ( action: deny, username: Some("[unclosed") ),
            ( action: deny, user_id: Some("") ),
        ],
    ),
)
"##;
    let cfg: ChatConfig = parse_str(raw).expect("must parse");
    let issues = cfg.validate();
    let mut paths: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        vec![
            "filters.overrides[0].content",
            "filters.overrides[1].username",
            "filters.overrides[2].user_id",
        ],
        "got: {issues:?}"
    );
    assert!(
        issues.iter().all(|i| i.message.contains("empty pattern")
            || i.message.contains("empty user id")
            || i.message.contains("invalid regex")),
        "messages must name the reason, got: {issues:?}"
    );
}

/// An unknown action name never reaches validate() — the enum rejects it
/// at parse time (the message_type-style hard error for `action`).
#[test]
fn unknown_override_action_is_a_parse_error() {
    let raw = r##"
(
    server: ( listen: "127.0.0.1:1", ),
    twitch: ( channel: "chan", ),
    theme: (
        font_size_px: 18,
        max_messages: 30,
        message_lifetime_secs: 60,
    ),
    filters: (
        overrides: [ ( action: maybe, content: Some("x") ) ],
    ),
)
"##;
    let err = parse_str::<ChatConfig>(raw).expect_err("unknown action must be refused");
    assert!(
        err.to_string().contains("maybe"),
        "error should name the bad action: {err}"
    );
}

/// The commented sample must surface the new list (discoverability
/// contract: `--sample-config` is the tool's interface).
#[test]
fn sample_config_exposes_overrides_list() {
    let text = quiver_config::generate_default::<ChatConfig>("quiver-chat --sample-config")
        .expect("generation succeeds");
    assert!(
        text.contains("overrides: []"),
        "sample config must show the overrides list, got no `overrides: []`"
    );
}

// ---- role_css -------------------------------------------------------------

// Ground truth by hand: three roles, exact strings preserved.
#[test]
fn role_css_map_parses_with_exact_snippets() {
    let raw = r##"
(
    server: ( listen: "127.0.0.1:1", ),
    twitch: ( channel: "chan", ),
    theme: (
        font_size_px: 18,
        max_messages: 30,
        message_lifetime_secs: 60,
        role_css: Some({
            "moderator": ".msg { background: rgba(0,0,0,.35); }",
            "broadcaster": ".msg { border-left: 3px solid red; }",
            "vip": ".user { text-shadow: 0 0 4px pink; }",
        }),
    ),
)
"##;
    let cfg: ChatConfig = parse_str(raw).expect("role_css must parse");
    let roles = cfg.theme.role_css.expect("map present");
    assert_eq!(roles.len(), 3);
    assert_eq!(
        roles.get("moderator").map(String::as_str),
        Some(".msg { background: rgba(0,0,0,.35); }")
    );
    assert_eq!(
        roles.get("broadcaster").map(String::as_str),
        Some(".msg { border-left: 3px solid red; }")
    );
    assert!(!roles.contains_key("subscriber"));
}
