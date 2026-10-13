# WebSocket Protocol

The widget is a dumb renderer: the engine broadcasts **JSON frames** over a
WebSocket (`/ws`), and the page draws whatever arrives. Understanding the
frames helps with debugging and with [Custom CSS](custom-css.md) hooks.

Frames are newline-delimited JSON objects with a `type` field:

| `type` | Payload | Meaning |
|---|---|---|
| `snapshot` | `{ messages[], meta }` | full state on connect / lag resync |
| `message`  | `{ message }` | a new rendered message |
| `expire`   | `{ ids[] }` | messages removed by age or count cap |
| `delete`   | `{ id }` | a message deleted by a moderator |
| `clear`    | `{}` | entire chat cleared (or channel switched) |
| `config`   | `{ meta }` | hot reload: theme/badges/emotes/CSS changed |
| `event`    | `{ event }` | a transient banner (sub/raid/…) |
| `reload`   | `{}` | frontend files changed — previous page should `location.reload()` |

## `meta` block (on `snapshot` and `config`)

```json
{
  "meta": {
    "theme":       { "font_size_px": 18, "max_messages": 30, "overflow_mode": "prune" },
    "badges":      { "moderator/1": "https://…/badge.png" },
    "custom_css":  ".msg { … }",
    "role_css":    { "moderator": ".msg.role-moderator { … }" },
    "emote_flags": { "twitch": true, "unicode": true, "seventv": true, "bttv": true, "ffz": true },
    "event_names":  { "reward:<id>": "Song Request", "sub": "Sub" },
    "event_layout": { "redeem": "{reward_icon} {user} redeemed {points_icon} {reward}{input_clause}",
                      "cheer":  "{bits_icon} {user} cheered {bits} bits{message_clause}",
                      "event_name": true },
    "custom_badges": {
      "definitions": { "special": { "id": "special", "url": "/badge-cache/…", "priority": 10, "height": 24, "label": "Special" } },
      "per_role":    { "moderator": { "badges": ["special"], "hide_native": false } },
      "per_user":    { "71092938":  { "badges": ["special"], "hide_native": true  } }
    }
  }
}
```

## Message shape

```json
{
  "type": "message",
  "message": {
    "id": "uuid", "user_login": "x", "user_id": "num", "display_name": "X",
    "color": "#FF0000", "is_action": false,
    "reply_to": { "message_id": "…", "user_login": "…", "display_name": "…", "text": "…" },
    "text": "hey Kappa world",
    "emotes": [ { "id": "25", "start": 4, "end": 9 } ],
    "gifs": [ { "id": "g1", "start": 0, "end": 34, "url": "https://media.giphy.com/…" } ],
    "badges": [ { "id": "moderator", "version": "1" } ]
  }
}
```

Position fields (`emotes.start/end`, `gifs.start/end`) are **character
indices, end-exclusive** (slicing `text[start..end]` is safe). Omitted
optional fields are skipped entirely (`reply_to`, `gifs`, … absent when
empty).

## Event shape

Internally-tagged:

```json
{ "type": "event", "event": { "kind": "raid", "from_login": "r", "from_display_name": "R", "viewers": 42 } }
```

Kinds: `sub`, `gift_sub`, `mystery_gift`, `raid`, `redeem`, `cheer`
(see [Events](events.md)). `redeem` carries `reward_title`, `reward_image`,
`points_image`, `reward_color`, `reward_id`, `user_input`; `cheer` carries
`bits` and `message`. Banner structure is configurable via
`theme.event_layout` (see [Events](events.md#event-templates)) — the widget
renders the template tokens, keeping user text as plain text.

## HTTP endpoints

| Path | Purpose |
|---|---|
| `/` | the widget page (index.html) |
| `/ws` | the WebSocket feed |
| `/health` | liveness probe |
| `/emotes.json` | merged third-party emote map `{ "providers": { … } }` |
| `/badge-cache/{hash}` | cached HTTP badge bytes (immutable, content-addressed) |
| `/badge-file/{hash}` | file:// badge passthrough (read fresh) |

Static responses send `Cache-Control: no-cache` plus legacy headers, and
`index.html` references versioned asset URLs, so OBS caches can't serve
stale frontends. See [Troubleshooting](troubleshooting.md).