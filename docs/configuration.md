# Configuration

Quiver tools are configured by **RON files** — verbose, explicit, and
deliberately so. A config file *is* the tool's interface: hand-author it,
have a UI generate it, or swap it at runtime.

## Generating a starting point

```sh
./target/release/quiver-chat --sample-config > chat.ron
```

This prints a fully commented config built from the tool's own defaults.
Every `//` comment and every value comes from the code — it never drifts
from what the program actually understands.

## Validating

On startup (and on every [hot reload](#hot-reload)) the config is checked in
two layers:

- **Parse** — must be valid RON.
- **Semantic validation** — ranges, referential integrity, regex validity,
  kind names. Errors list the exact path and message, e.g.:

  ```
  config chat.ron is invalid:
    - filters.message_type.items: unknown message kind "foo" — expected one of:
      message, sub, gift_sub, mystery_gift, raid
  ```

  Invalid configs are **rejected outright**: the tool refuses to start, or a
  hot reload keeps the previous config. Broken CSS is the only thing treated
  as advisory (warned, not blocked).

## The JSON Schema

```sh
./target/release/quiver-chat --print-schema
```

prints the config's JSON Schema. It's the machine-contract for tooling: the
future Quiver UI reads it to build forms. Human-facing descriptions and
defaults are embedded.

## Structure

A config has five top-level sections:

| Section | Purpose | See |
|---|---|---|
| `server` | listen address, widget frontend directory | [Getting Started](getting-started.md) |
| `twitch` | channel login, API credentials | [Badges](badges.md#twitch-credentials) |
| `theme` | typography, overflow, custom & per-role CSS, event templates | [Custom CSS](custom-css.md), [Events](events.md#event-templates) |
| `emotes` | per-provider emote toggles | [Emotes](emotes.md) |
| `filters` | allow/deny lists per dimension | [Filters](filters.md) |
| `badges` | custom badge definitions & assignments | [Badges](badges.md) |

## Options & enums

- `Option<T>` fields are written `None` or `Some(value)` — RON requires the
  explicit wrapper (e.g. `widget_dist: Some("crates/quiver-chat/widget")`).
- Enum values are lowercase identifiers: `overflow_mode: prune`, filter
  `mode: denylist`.
- Free-form maps use braces + quoted keys: `role_css: { "moderator": "…" }`.
- Long strings (CSS) are easiest as raw strings — a string in `chat.ron` is
  just inline text; no special encoding needed for newlines if you write a
  RON raw string.

## Hot reload

While running, the config file is watched. Save any change and it reloads
**live** — no restart:

- theme/CSS/emote toggles update instantly (pushed over WebSocket as a
  `config` frame); existing messages re-render live so badge heights,
  custom-badge images and role CSS all update in-place — no F5 needed
- the channel can even **swap live** if you change `twitch.channel`
- `server.listen` changes rebind the server (the OBS source URL must be
  updated separately, but no restart needed)
- `SIGHUP` (`kill -HUP <pid>`) forces a reload manually

Rejected edits (see [validation](#validating)) leave the running config
untouched and log why.