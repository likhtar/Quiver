# Filters

Filter which messages render, per dimension. Every dimension supports both
**allow** and **deny** behavior; dimensions combine with **AND** — a message
shows only when every active dimension passes. **Overrides** (per-user content
rules) are checked BEFORE the dimensions and decide on their own — that's how
a message gets an exception in either direction.

```ron
filters: (
    # Regex against the sender's display name
    display_name: Some(( mode: denylist, items: ["^StreamElements"] )),

    # Regex against the sender's login (always lowercase)
    username: Some(( mode: denylist, items: ["nightbot", "moobot"] )),

    # EXACT Twitch user id (or login handle)
    user_id: None,

    # Regex against message text — e.g. hide chat commands:
    content: Some(( mode: denylist, items: ["^!"] )),

    # Message kind names: message | sub | gift_sub | mystery_gift | raid
    message_type: None,

    # Badge ids the sender carries ("moderator", "vip", ...)
    role: None,

    # Per-user content rules — checked FIRST, see below.
    overrides: [
        (
            action: allow,               # allow = render, deny = drop
            user_id: Some("1538701825"), # EXACT id; any subset of conditions
            content: Some("(?i)^!шіхтар(?: |$)"), # regex vs message text
        ),
    ],
)
```

## Semantics

- A dimension with an **empty** items list is **inactive** (an empty
  allowlist means "match everything", deliberately not "drop everything").
- **allowlist** — at least one item must match, or the message is dropped.
- **denylist** — if any item matches, the message is dropped.
- Dimensions AND: every active dimension must pass.
- `user_id` is exact-match (regex metacharacters are literal — `"7*"` matches
  only the string `7*`).
- Events (subs/raids/…) filter by kind; their text is empty and they carry
  no badges, so a `content` or `role` filter sees an empty target.

## Overrides: first match wins, and it's final

Each `overrides` entry is a rule: a user condition, a content condition and
an action.

- Conditions present in a rule must **ALL** match (AND): `user_id` (exact),
  `username` (regex vs login), `display_name` (regex), `content` (regex vs
  text). An **absent** condition is a wildcard.
- A rule with **no conditions at all** is **inactive** — the parity of an
  empty `items` list. It decides nothing.
- Rules run in config order, BEFORE any dimension, and the **first matching
  rule gives the final decision**: `allow` renders the message (dimensions
  are never consulted — this is the exception), `deny` drops it (an extra
  ban no allowlist can undo). First match, not last match: put the narrower
  rule first when two rules can match the same message.
- If no rule matches, the AND-combined dimensions decide as usual.

### Example 1 — hide a user entirely, except one command

```ron
filters: (
    user_id: Some(( mode: denylist, items: ["1538701825"] )),  # hide likh_bot…
    overrides: [
        ( action: allow,                                       # …except his
          user_id: Some("1538701825"),
          content: Some("(?i)^!шіхтар(?: |$)") ),               # !шіхтар cmd
    ],
)
```

Without overrides this is impossible: `user_id` denies the message before
`content` could excuse it — dimensions only AND.

### Example 2 — deny text for everyone but one user

```ron
filters: (
    content: Some(( mode: denylist, items: ["(?i)купити крипту"] )),
    overrides: [
        ( action: allow,
          username: Some("^likh_tar$"),
          content: Some("(?i)купити крипту") ),
    ],
)
```

The mirror images use `action: deny` with the same conditions: "show
everything from this user except…" (base allowlist + `deny` rule) and
"show this text only to this user" (two rules: `allow` for the user first,
content-only `deny` second — first match wins).

## Errors are loud (on purpose)

Regex patterns that fail to compile, or unknown message-kind names, are
**hard validation errors**: the config is rejected at startup, and a hot
reload with a broken filter is refused — the previous filters stay active.
Moderation must never silently stop working.

The same contract covers `overrides`: empty conditions (`filters.overrides[i].<field>`)
and non-compiling regexes are hard validation errors, and an unknown
`action` (`allow`/`deny` only) fails config parsing outright.

## Examples

Keep the chat to staff and fan-favorites:

```ron
filters: (
    role: Some(( mode: allowlist, items: ["moderator", "vip", "subscriber"] )),
)
```

Hide bots and command spam:

```ron
filters: (
    username: Some(( mode: denylist, items: ["nightbot", "streamelements", "moobot"] )),
    content:  Some(( mode: denylist, items: ["^!"] )),
)
```

Combined with [Badges](badges.md)' `hide_native` and
[Emotes](emotes.md)'s per-provider toggles, most "what should the overlay
show" questions are config-only.