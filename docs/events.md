# Stream Events

Besides chat messages, Quiver renders **transient banners** for rich chat
events. Banners live in the chat flow, auto-dismiss after
`theme.event_banner_secs` seconds (default 8; `0` = keep until scrolled
away), and are **never pruned** by overflow — their timer is their only exit.

| Event | Default banner text | From |
|---|---|---|
| Sub / resub | `★ Name subscribed!` / `★ Name resubbed (12 mo, 7 streak)` | Twitch `sub`/`resub` |
| Gift sub | `🎁 Gifter gifted a sub to Recipient` | Twitch `subgift`/`anonsubgift` |
| Mystery gift | `🎁🎁 Gifter is gifting 10 subs!` | Twitch `submysterygift`/`anonsubmysterygift` |
| Raid | `⚔ Raider raided with 42 viewers!` | Twitch `raid` |
| Redeem | `🏅 Name redeemed «Song Request»: <input>` | PRIVMSG `custom-reward-id` / EventSub redemption |
| Cheer (bits) | `💠 Name cheered 500 bits: <message>` | PRIVMSG `bits` |
| Hype train | `🚂 Hype Train level 3 — 456/500` | EventSub `channel.hype_train.*` |
| Prediction | `📊 Prediction started: Will X win?` | EventSub `channel.prediction.*` |
| Poll | `🗳️ Poll started: <title>` | EventSub `channel.poll.*` |

Like chat messages, events pass through [Filters](filters.md) by kind
(`sub`, `gift_sub`, `mystery_gift`, `raid`, `redeem`, `cheer`,
`hype_train`, `prediction`, `poll`) — a `message_type` allowlist of
`["message"]` hides all banners, for example.

Subs, gift subs, mystery gifts, raids, and cheers arrive over the same
anonymous chat feed as messages — no credentials needed. **Redeems**, hype
trains, predictions, and polls arrive via the **channel OAuth** layer: run
`quiver-chat --auth` once (the broadcaster or a moderator of the channel
approves) — see [OAuth](oauth.md). Redeem banners show the reward name when
OAuth is present; without it they fall back to
`Name redeemed a channel point reward: <input>`.

## Event templates

`redeem` and `cheer` banners are built from a **`{token}` template**
(`theme.event_layout`), so you can rearrange icons and text freely. The
widget substitutes each token in order; literal characters stay as-is.

```ron
theme: (
    event_layout: (
        redeem: "{reward_icon} {user} redeemed {points_icon} {reward}{input_clause}",
        cheer:  "{bits_icon} {user} cheered {bits} bits{message_clause}",
        // Event-name pill for redeem/cheer; true by default.
        event_name: true,
    ),
),
```

Tokens:

| Token | redeem | cheer | Renders as |
|---|---|---|---|
| `{points_icon}` | ✓ | — | channel-points coin from Twitch (custom image, else default orb); omitted when unknown |
| `{reward_icon}` | ✓ | — | the reward's own Twitch icon; omitted when the reward uses Twitch's default icon |
| `{reward}` | ✓ | — | reward name (fallback "a channel point reward") |
| `{bits_icon}` | — | ✓ | bits badge for the amount (else 💠) |
| `{bits}` | — | ✓ | cheer amount |
| `{user}` | ✓ | ✓ | sender display name |
| `{input}` | ✓ | — | raw redemption input (may be empty) |
| `{input_clause}` | ✓ | — | `": <input>"` when input is non-empty, else `""` |
| `{message}` | — | ✓ | raw cheer text (may be empty) |
| `{message_clause}` | — | ✓ | `": <message>"` when non-empty, else `""` |

Notes:

- Text is inserted as **plain text**, never HTML — a crafted display name or
  reward title cannot inject markup.
- Unknown, empty, or unbalanced `{...}` tokens are **hard config errors**
  (the reload is refused and the previous config keeps running). Literal
  braces are not supported.
- Use `{input_clause}` / `{message_clause}` instead of `{input}` /
  `{message}` to avoid a dangling `": "` when the value is empty.
- `{points_icon}` is the channel's points coin (your custom coin, else
  Twitch's default orb) and `{reward_icon}` is the reward's own icon — two
  different images. Both are fetched from Twitch; neither is drawn locally.
  A reward without its own icon simply has no `{reward_icon}`.
- `event_layout.event_name` (`true` by default) toggles the event-name pill
  for `redeem`/`cheer` only. Other kinds keep their pill. The pill text
  comes from `theme.event_names` (`reward:<id>`, else the kind).
- An empty template string falls back to the built-in default for that kind.

Related surfaces:

- [Moderation](moderation.md) — deleted messages, cleared chat
- [Protocol](protocol.md#event-shape) — the wire frame behind banners
