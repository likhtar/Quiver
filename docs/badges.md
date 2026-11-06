# Badges

Two badge systems coexist: **native** Twitch badges (broadcaster, moderator,
subscriber, … from the CDN) and **custom** badges (your own images, attached
per-role or per-user).

## Native badges

Twitch's own badges render automatically from each message's badge tags, as
long as you configured API credentials:

### Twitch credentials

```ron
twitch: (
    channel: "your_channel_here",
    client_id: "your_client_id",
    client_secret: "your_client_secret",
)
```

`client_id`/`client_secret` power the Helix API lookups that resolve badge
image URLs (and channel emotes — see [Emotes](emotes.md)). Keep secrets in
files matching `*.local.ron` — git-ignored by default. Without credentials,
Twitch badges and channel-specific emotes are not resolved.

For a config that is **shared or shipped as a template**, keep the secrets
out of it entirely and point `twitch` at a separate file:

```ron
twitch: (
    channel: "your_channel_here",
    credentials_file: Some("credentials.ron"),
)
```

```ron
# credentials.ron — same directory as chat.ron (also git-ignored)
( client_id: "your_client_id", client_secret: "your_client_secret" )
```

A relative `credentials_file` resolves against the directory of the config
that names it. Values there **override** any inline `client_id`/`client_secret`
(so you can ship placeholders and let each user drop in their own file).
Empty/whitespace values count as unset. A missing file only warns (Quiver runs
without API credentials); a malformed one is a hard error. The file is watched
for [hot reload](configuration.md#hot-reload) like the config itself.

## Custom badges

Define images, then attach them:

```ron
badges: Some((
    definitions: {
        "special": ( uri: "https://cdn.example.com/special.png", priority: 10, height: 1, label: Some("Special") ),
        "local":   ( uri: "file:///home/user/badges/logo.png",  priority: 20, height: 1 ),
    },
    per_role: {
        "moderator": ( badges: ["special"], hide_native: false ),
    },
    per_user: {
        "uquark":    ( badges: ["special", "local"], hide_native: false ),
    },
))
```

- **`definitions`** — `id → { uri, priority, height, label }`. Lower
  `priority` renders first (ties by definition order). `uri` is
  `http(s)://` or `file://` — see [caching](#caching) below. `height` is
  in **em units, default 1** (= native badge size, scales with the font).

Badge definitions are **hot-reloaded live**: edit a height or priority,
save the config, and the change appears in every row — no F5 required.
See [Message Rendering](chat.md#config-re-rendering).
- **`per_role`** — attach badge ids to users carrying a Twitch *badge set id*
  (`moderator`, `vip`, `subscriber`, … same vocabulary as `role_css`).
- **`per_user`** — attach to a specific Twitch user, keyed by numeric ID
  (stable across renames) **or** login handle — both matched.
- **`hide_native: true`** suppresses that identity's Twitch CDN badges too.

When several assignments match one sender (a moderator who's also in
`per_user`), their badge lists **union**. `hide_native` semantics:

- **per_role** — hides ONLY that role's own badge, e.g.
  `"moderator": ( badges: […], hide_native: true )` removes just the
  moderator badge; subscriber/broadcaster/etc. still render.
- **per_user** — hides ALL native badges for that user.

Custom badges always render; native ones only when not hidden.

Per-identity assignment detail: [Chat rendering](chat.md#layout) shows where
badges sit in the row; class hooks are `.badges`, `.badge`, `.custom-badge`.

## Caching

Remote badge images are fetched **engine-side**, then served to the widget
from local routes — the browser never hits external origins (firewall-safe).

- `http(s)://` URIs: fetched once at startup/reload, **cached by content
  hash** at the cache directory, and revalidated with conditional GETs
  (ETag / Last-Modified) every `refresh_interval_secs` (default 24h).
  Revalidation failures serve the previously cached image.
- `file://` URIs: **not cached** — read fresh from the canonical path on
  every request (`/badge-file/{hash}` route). Follows symlinks.
- Cache directory: `$XDG_CACHE_HOME/Quiver/badges/`, or `~/.cache/Quiver/badges/`
  — override with `badges.cache_dir` if you want it elsewhere.

```
badges: Some((
    definitions: { … },
    per_role: { … },
    per_user: { … },
    cache_dir: Some("/custom/badge-cache"),
    refresh_interval_secs: 86400,
))
```

### Failure handling

Definitions are **independent**: one unresolvable badge (a pre-wired file
that doesn't exist yet, a transient 404) is skipped with a warning — the
rest resolve and appear, and the broken one shows up automatically on the
next reload once it resolves.