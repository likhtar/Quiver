//! Minimal Helix API client: app access token (client credentials) plus
//! the few calls Quiver needs. Grows only when tools need more.
//!
//! Secrets never leave this module; nothing here logs credentials.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;

const TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";
const HELIX_URL: &str = "https://api.twitch.tv/helix";
/// The public web-client id required by gql.twitch.tv (verified: Helix
/// client-ids are rejected there with "Client-ID header is invalid").
const GQL_WEB_CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const GQL_URL: &str = "https://gql.twitch.tv/gql";
/// Refresh margin: treat tokens expiring within this window as expired.
const TOKEN_EXPIRY_MARGIN: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum HelixError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("helix api error (status {status}): {body}")]
    Api { status: u16, body: String },

    #[error("helix resource not found: {0}")]
    NotFound(String),

    /// A channel-scoped (user token) call was made without usable OAuth.
    #[error("channel oauth required: {0}")]
    ChannelAuthRequired(String),
}

#[derive(Debug, Clone)]
struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

/// Helix client authenticated via app access token (client-credentials),
/// optionally paired with a channel-scoped user token (see [`crate::auth`]).
pub struct HelixClient {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    token: RwLock<Option<CachedToken>>,
    channel: RwLock<Option<crate::auth::ChannelToken>>,
}

impl HelixClient {
    pub fn new(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> reqwest::Result<Self> {
        // 10s deadline per request (see #36): a hung Helix must degrade, not stall.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            token: RwLock::new(None),
            channel: RwLock::new(None),
        })
    }

    /// Attach channel-scoped OAuth from the on-disk store, when present.
    /// Callers check [`Self::has_channel_auth`] to learn whether it landed.
    pub fn with_channel_auth(self) -> Self {
        let token = crate::auth::ChannelToken::load(
            self.http.clone(),
            &self.client_id,
            &self.client_secret,
        );
        if let Some(token) = token {
            *self.channel.write().unwrap() = Some(token);
        }
        self
    }

    /// Install tokens obtained right now (e.g. straight after `--auth`).
    pub fn set_channel_tokens(&self, store: crate::auth::StoredTokens) {
        let token = crate::auth::ChannelToken::from_store(
            self.http.clone(),
            &self.client_id,
            &self.client_secret,
            store,
        );
        *self.channel.write().unwrap() = Some(token);
    }

    pub fn has_channel_auth(&self) -> bool {
        self.channel
            .read()
            .unwrap()
            .as_ref()
            .map(|t| t.is_present())
            .unwrap_or(false)
    }

    pub fn channel_login(&self) -> Option<String> {
        self.channel
            .read()
            .unwrap()
            .as_ref()
            .and_then(|t| t.channel_login())
    }

    /// Scopes granted to the channel token (empty without OAuth).
    pub fn channel_scopes(&self) -> Vec<String> {
        self.channel
            .read()
            .unwrap()
            .as_ref()
            .map(|t| t.scopes())
            .unwrap_or_default()
    }

    /// POST an EventSub subscription (websocket transport, user token).
    pub async fn create_eventsub_subscription(
        &self,
        body: &serde_json::Value,
    ) -> Result<(), HelixError> {
        let channel_token = {
            let guard = self.channel.read().unwrap();
            match guard.as_ref() {
                Some(t) => t.clone(),
                None => {
                    return Err(HelixError::ChannelAuthRequired(
                        "run `quiver-chat --auth` to enable EventSub".to_string(),
                    ))
                }
            }
        };
        let token = channel_token.access_token().await?;
        let resp = self
            .http
            .post(format!("{HELIX_URL}/eventsub/subscriptions"))
            .header("Client-Id", &self.client_id)
            .bearer_auth(token)
            .json(body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(HelixError::Api {
                status: status.as_u16(),
                body,
            });
        }
        Ok(())
    }

    /// Valid app access token; fetches a fresh one when missing/expiring.
    async fn app_token(&self) -> Result<String, HelixError> {
        // Fast path: cached token still valid past the refresh margin.
        if let Some(t) = self.token.read().unwrap().as_ref()
            && t.expires_at > Instant::now() + TOKEN_EXPIRY_MARGIN
        {
            return Ok(t.access_token.clone());
        }

        #[derive(serde::Deserialize)]
        struct TokenResponse {
            access_token: String,
            expires_in: u64,
        }

        let resp: TokenResponse = self
            .http
            .post(TOKEN_URL)
            .query(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("grant_type", "client_credentials"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let cached = CachedToken {
            access_token: resp.access_token,
            expires_at: Instant::now() + Duration::from_secs(resp.expires_in),
        };
        *self.token.write().unwrap() = Some(cached.clone());
        Ok(cached.access_token)
    }

    async fn invalidate_token(&self) {
        *self.token.write().unwrap() = None;
    }

    /// GET a Helix path with app auth. Retries once on 401 (stale token).
    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, HelixError> {
        for attempt in 0..2 {
            let token = self.app_token().await?;
            let resp = self
                .http
                .get(format!("{HELIX_URL}{path}"))
                .query(query)
                .header("Client-Id", &self.client_id)
                .bearer_auth(token)
                .send()
                .await?;

            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                self.invalidate_token().await;
                continue;
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(HelixError::Api {
                    status: status.as_u16(),
                    body,
                });
            }
            return Ok(resp.json::<T>().await?);
        }
        unreachable!("retry loop returns or errors on both passes")
    }

    /// GET a Helix path with CHANNEL (user) auth. Refreshes the token on
    /// demand; the store guard is dropped before any await.
    async fn get_json_user<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, HelixError> {
        let channel_token = {
            let guard = self.channel.read().unwrap();
            match guard.as_ref() {
                Some(t) => t.clone(),
                None => {
                    return Err(HelixError::ChannelAuthRequired(
                        "run `quiver-chat --auth` to enable channel-scoped calls".to_string(),
                    ))
                }
            }
        };
        let token = channel_token.access_token().await?;
        let resp = self
            .http
            .get(format!("{HELIX_URL}{path}"))
            .query(query)
            .header("Client-Id", &self.client_id)
            .bearer_auth(token)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(HelixError::Api {
                status: status.as_u16(),
                body,
            });
        }
        Ok(resp.json::<T>().await?)
    }

    /// Identity for the currently-authed user (used right after `--auth`).
    /// Requires a user token; returns (id, login).
    pub async fn me(&self) -> Result<(String, String), HelixError> {
        #[derive(serde::Deserialize)]
        struct MeResponse {
            data: Vec<Me>,
        }
        #[derive(serde::Deserialize)]
        struct Me {
            id: String,
            login: String,
        }
        let parsed: MeResponse = self.get_json_user("/users", &[]).await?;
        parsed
            .data
            .into_iter()
            .next()
            .map(|u| (u.id, u.login))
            .ok_or_else(|| HelixError::NotFound("current user".to_string()))
    }

    /// Numeric broadcaster ID for a channel login.
    pub async fn user_id(&self, login: &str) -> Result<String, HelixError> {
        #[derive(serde::Deserialize)]
        struct UsersResponse {
            data: Vec<User>,
        }
        #[derive(serde::Deserialize)]
        struct User {
            id: String,
        }

        let parsed: UsersResponse = self.get_json("/users", &[("login", login)]).await?;
        parsed
            .data
            .into_iter()
            .next()
            .map(|u| u.id)
            .ok_or_else(|| HelixError::NotFound(format!("channel login {login:?}")))
    }

    /// Badge image URLs keyed by `"set_id/version"` (2x scale). Global map
    /// first; channel-specific badges override on key collision.
    pub async fn badge_map(
        &self,
        broadcaster_id: Option<&str>,
    ) -> Result<HashMap<String, String>, HelixError> {
        let global: BadgesResponse = self.get_json("/chat/badges/global", &[]).await?;
        let mut map = flatten_badge_sets(global.data);

        if let Some(bid) = broadcaster_id {
            let channel: BadgesResponse = self
                .get_json("/chat/badges", &[("broadcaster_id", bid)])
                .await?;
            map.extend(flatten_badge_sets(channel.data));
        }
        Ok(map)
    }

    /// reward_id -> reward display info (title + icon URL) for the channel.
    /// Requires channel OAuth (scope `channel:read:redemptions`).
    pub async fn custom_reward_titles(
        &self,
        broadcaster_id: &str,
    ) -> Result<HashMap<String, RewardInfo>, HelixError> {
        #[derive(serde::Deserialize)]
        struct RewardsResponse {
            data: Vec<Reward>,
        }
        #[derive(serde::Deserialize)]
        struct Reward {
            id: String,
            title: String,
            #[serde(default)]
            image: Option<RewardImage>,
            #[serde(default)]
            background_color: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct RewardImage {
            #[serde(rename = "url_2x")]
            url_2x: Option<String>,
        }

        fn normalize_hex(h: Option<String>) -> Option<String> {
            let mut h = h?;
            if h.is_empty() { return None; }
            if !h.starts_with('#') {
                h.insert(0, '#');
            }
            // Helix returns 6-hex usually; normalize
            Some(h)
        }

        let parsed: RewardsResponse = self
            .get_json_user(
                "/channel_points/custom_rewards",
                &[("broadcaster_id", broadcaster_id)],
            )
            .await?;
        Ok(parsed
            .data
            .into_iter()
            .map(|r| {
                (
                    r.id,
                    RewardInfo {
                        title: r.title,
                        // 2x preferred; Twitch returns null image for
                        // rewards using the default icon.
                        image_url: r.image.and_then(|i| i.url_2x),
                        background_color: normalize_hex(r.background_color),
                    },
                )
            })
            .collect())
    }

    /// Open (UNFULFILLED) channel point redemptions. Requires channel OAuth
    /// (scope `channel:read:redemptions`).
    pub async fn open_redemptions(
        &self,
        broadcaster_id: &str,
    ) -> Result<Vec<Redemption>, HelixError> {
        #[derive(serde::Deserialize)]
        struct RedemptionsResponse {
            data: Vec<RawRedemption>,
        }

        let parsed: RedemptionsResponse = self
            .get_json_user(
                "/channel_points/custom_rewards/redemptions",
                &[("broadcaster_id", broadcaster_id), ("status", "UNFULFILLED")],
            )
            .await?;
        Ok(parsed.data.into_iter().map(Into::into).collect())
    }

    /// The channel's channel-points coin icon URL — the custom coin when the
    /// streamer uploaded one, else Twitch's global default orb. Anonymous
    /// GQL (no OAuth; web Client-Id required — Helix client-ids are
    /// rejected there). `login_or_id` accepts the channel login or numeric
    /// broadcaster id.
    ///
    /// The coin is per-channel custom art NOT exposed by any Helix endpoint;
    /// GQL's `user.channel.communityPointsSettings` is the only source.
    pub async fn coin_icon_url(&self, login_or_id: &str) -> Option<String> {
        let field = if login_or_id.chars().all(|c| c.is_ascii_digit()) {
            format!(r#"id:"{login_or_id}""#)
        } else {
            format!(r#"login:"{login_or_id}""#)
        };
        let query = format!(
            r#"{{user({field}){{channel{{communityPointsSettings{{image{{url2x}} defaultImage{{url2x}}}}}}}}}}"#
        );
        let body = serde_json::json!({ "query": query });

        let resp = self
            .http
            .post(GQL_URL)
            .header("Client-Id", GQL_WEB_CLIENT_ID)
            .json(&body)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            tracing::warn!(status = %resp.status(), "gql coin lookup failed");
            return None;
        }
        let v: serde_json::Value = resp.json().await.ok()?;
        parse_coin_icon(&v)
    }
}

/// Coalesce the coin icon from a GQL `communityPointsSettings` response:
/// custom coin first (`image.url2x`), global default orb second. Pure for
/// testability.
fn parse_coin_icon(v: &serde_json::Value) -> Option<String> {
    let cps = v.pointer("/data/user/channel/communityPointsSettings")?;
    cps.pointer("/image/url2x")
        .and_then(|x| x.as_str())
        .or_else(|| {
            cps.pointer("/defaultImage/url2x")
                .and_then(|x| x.as_str())
        })
        .map(str::to_owned)
}

/// Display info for a channel point reward: its title and icon URL
/// (None when the reward uses Twitch's default icon).
#[derive(Debug, Clone, PartialEq)]
pub struct RewardInfo {
    pub title: String,
    pub image_url: Option<String>,
    pub background_color: Option<String>,
}

/// A channel point redemption (from the GET redemptions endpoint).
#[derive(Debug, Clone, PartialEq)]
pub struct Redemption {
    pub id: String,
    pub user_id: String,
    pub user_login: String,
    pub user_display_name: String,
    pub reward_id: String,
    pub user_input: String,
    pub created_at: String,
}

impl Redemption {
    /// Unix epoch seconds for `created_at`.
    pub fn created_at_secs(&self) -> u64 {
        chrono::DateTime::parse_from_rfc3339(&self.created_at)
            .map(|t| t.timestamp().max(0) as u64)
            .unwrap_or(0)
    }
}

impl From<RawRedemption> for Redemption {
    fn from(r: RawRedemption) -> Self {
        Self {
            id: r.id,
            user_id: r.user_id,
            user_login: r.user_login,
            user_display_name: r.user_name,
            reward_id: r.reward_id,
            user_input: r.user_input,
            created_at: r.created_at,
        }
    }
}

#[derive(serde::Deserialize)]
struct RawRedemption {
    id: String,
    user_id: String,
    user_login: String,
    user_name: String,
    #[serde(rename = "channel_points_custom_reward_id")]
    reward_id: String,
    #[serde(default)]
    user_input: String,
    created_at: String,
}

#[derive(serde::Deserialize)]
struct BadgesResponse {
    data: Vec<BadgeSet>,
}

#[derive(serde::Deserialize)]
struct BadgeSet {
    set_id: String,
    versions: Vec<BadgeVersion>,
}

#[derive(serde::Deserialize)]
struct BadgeVersion {
    id: String,
    image_url_2x: String,
}

fn flatten_badge_sets(sets: Vec<BadgeSet>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for set in sets {
        for v in set.versions {
            map.insert(format!("{}/{}", set.set_id, v.id), v.image_url_2x);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live-verified GQL fixture: channel with a CUSTOM coin (likh_tar).
    const COIN_CUSTOM_JSON: &str = r#"{"data":{"user":{"channel":{"communityPointsSettings":{"image":{"url2x":"https://static-cdn.jtvnw.net/channel-points-icons/400589222/29c4fd4a-adc1-42fd-8a34-49185f1a6a32/icon-2.png"},"defaultImage":{"url2x":"https://static-cdn.jtvnw.net/channel-points-icons/orb-2.png"}}}}}}"#;

    /// Live-verified GQL fixture: channel with the DEFAULT coin (jerma985).
    const COIN_DEFAULT_JSON: &str = r#"{"data":{"user":{"channel":{"communityPointsSettings":{"image":null,"defaultImage":{"url2x":"https://static-cdn.jtvnw.net/channel-points-icons/orb-2.png"}}}}}}"#;

    #[test]
    fn coin_icon_custom_coin_wins() {
        let v: serde_json::Value = serde_json::from_str(COIN_CUSTOM_JSON).unwrap();
        assert_eq!(
            parse_coin_icon(&v).as_deref(),
            Some("https://static-cdn.jtvnw.net/channel-points-icons/400589222/29c4fd4a-adc1-42fd-8a34-49185f1a6a32/icon-2.png")
        );
    }

    #[test]
    fn coin_icon_falls_back_to_default_orb() {
        let v: serde_json::Value = serde_json::from_str(COIN_DEFAULT_JSON).unwrap();
        assert_eq!(
            parse_coin_icon(&v).as_deref(),
            Some("https://static-cdn.jtvnw.net/channel-points-icons/orb-2.png")
        );
    }

    #[test]
    fn coin_icon_none_on_garbage() {
        let v: serde_json::Value = serde_json::json!({"errors": [{"message": "nope"}]});
        assert_eq!(parse_coin_icon(&v), None);
        assert_eq!(parse_coin_icon(&serde_json::Value::Null), None);
    }

    const GLOBAL_BADGES_JSON: &str = r#"{
        "data": [
            { "set_id": "moderator", "versions": [ { "id": "1", "image_url_2x": "https://cdn/mod_2x.png" } ] },
            { "set_id": "subscriber", "versions": [
                { "id": "0",  "image_url_2x": "https://cdn/sub_0_2x.png" },
                { "id": "12", "image_url_2x": "https://cdn/sub_12_2x.png" }
            ] }
        ]
    }"#;

    // Ground truth by hand: moderator/1, subscriber/0, subscriber/12.
    #[test]
    fn flattens_badge_sets_with_hand_written_truth() {
        let resp: BadgesResponse =
            serde_json::from_str(GLOBAL_BADGES_JSON).expect("fixture parses");
        let map = flatten_badge_sets(resp.data);

        assert_eq!(map.len(), 3);
        assert_eq!(map["moderator/1"], "https://cdn/mod_2x.png");
        assert_eq!(map["subscriber/0"], "https://cdn/sub_0_2x.png");
        assert_eq!(map["subscriber/12"], "https://cdn/sub_12_2x.png");
    }

    // Ground truth by hand: channel entry overrides global on same key only.
    #[test]
    fn channel_badges_override_global_on_collision() {
        let mut global = HashMap::new();
        global.insert("moderator/1".to_string(), "global".to_string());
        global.insert("vip/1".to_string(), "global_vip".to_string());

        let mut channel = HashMap::new();
        channel.insert("moderator/1".to_string(), "channel".to_string());

        global.extend(channel);

        assert_eq!(global.len(), 2);
        assert_eq!(global["moderator/1"], "channel");
        assert_eq!(global["vip/1"], "global_vip");
    }
}
