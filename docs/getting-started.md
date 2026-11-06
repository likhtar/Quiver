# Getting Started

Quiver's first tool, `quiver-chat`, runs a local HTTP+WebSocket server that
serves a chat widget for OBS. This page walks through building, configuring,
and connecting it for the first time.

## Building

Requires a Rust toolchain (stable). From the repository root:

```sh
cargo build --release
```

The binary lands at `target/release/quiver-chat`.

## Running

```sh
# generate a commented starter config
./target/release/quiver-chat --sample-config > chat.ron

# edit it (see Configuration), then:
./target/release/quiver-chat --config chat.ron
```

The server prints what it's doing (use `RUST_LOG=info` for full logging) and
listens on the address in `server.listen` (default `127.0.0.1:4783`).

## Adding it to OBS

1. Add a **Browser Source** to your scene.
2. Point it at the widget URL: `http://127.0.0.1:4783`
3. Set the source **Width/Height** to your desired chat panel size — the
   widget fills the source exactly and clips overflow on its own.
4. Background is transparent; messages render as cards anchored to the bottom
   of the box.

> The widget is served with no caching and cache-busted URLs, so OBS always
> picks up frontend changes after a reload. See
> [Troubleshooting](troubleshooting.md) if you ever see stale content.

## Minimal config

The generated sample is already runnable — change the channel, add Twitch
credentials if you want badges/emotes beyond first-party (keep them in a
separate, git-ignored `credentials.ron` — see
[Badges → Twitch credentials](badges.md#twitch-credentials)), and reload:

```ron
(
    server: ( listen: "127.0.0.1:4783", ),
    twitch: ( channel: "your_channel_here" ),
    theme: (
        font_size_px: 18,
        max_messages: 30,
        message_lifetime_secs: 60,
        overflow_mode: prune,
    ),
)
```

Everything else is optional. Each feature has its own page:

- [Emotes](emotes.md) — emotes & third-party providers
- [Badges](badges.md) — native & custom badges
- [Custom CSS](custom-css.md) — styling
- [Filters](filters.md) — moderation rules
- [Stream Events](events.md) — subs, raids
- [Configuration](configuration.md) — full reference