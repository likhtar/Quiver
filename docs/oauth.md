# Channel OAuth (`--auth`)

Channel-scoped features (reward names on redeem banners, redemptions of
input-free rewards, hype trains, predictions, polls) use a **user access
token** obtained through Twitch's **Device Code Flow** — no redirect server,
no browser on the machine running Quiver.

## One-time setup

```sh
quiver-chat --config chat.ron --auth
```

`--auth` needs `client_id`/`client_secret`. Quiver reads them from either the
inline `twitch` fields or the separate `twitch.credentials_file` (see
[Badges → Twitch credentials](badges.md#twitch-credentials)).

1. Quiver prints a URL (`https://www.twitch.tv/activate…`) and a code.
2. Open the URL on any device, enter the code, approve the scopes.
3. Tokens are stored at `$XDG_CONFIG_HOME/quiver/oauth.json` (0600) — never
   in the RON config.

The flow requests the full read scope set Quiver consumes
(`channel:read:redemptions`, `channel:read:hype_train`, `channel:read:predictions`,
`channel:read:polls`, `channel:read:subscriptions`, `channel:read:follows`,
`channel:read:goals`, `channel:read:charity`, `channel:read:ads`,
`moderation:read`, `chat:read`, `user:read:chat`,
`channel:manage:redemptions`). Override with `--scopes a,b,c`.

**Ownership matters:** redemption/hype/prediction/poll scopes only work when
the approving user is the **broadcaster or a moderator** of the configured
channel — `--auth` warns when the authed login differs from `twitch.channel`.

Inspect the stored state (for the Quiver UI):

```sh
quiver-chat --config chat.ron --auth-print
# {"client_id":"…","channel_login":"…","scopes":[…],"token_path":"…"}
```

## What it unlocks

| Feature | Without OAuth | With OAuth |
|---|---|---|
| Redeem banner | `Name redeemed a channel point reward: <input>` (text-input redemptions only) | `Name redeemed «Reward Title»: <input>` + input-free redemptions via the redemption poller |
| Hype train / prediction / poll banners | — | via EventSub WebSocket |

Access tokens refresh automatically; if the refresh token is revoked,
re-run `--auth`.
