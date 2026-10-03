//! The web companion's wire types (`docs/companion.md`): pairing a phone,
//! who the caller is, and Web Push subscriptions. Everything else the
//! companion page does is the ordinary API in [`crate::api`] and
//! [`crate::triggers`], under the caller's own scopes.

use serde::{Deserialize, Serialize};

use crate::{Client, Error};

/// `POST /app/pair`: redeem a one-time pairing code (from the fragment of
/// the link `by serve token new --link` prints) for a token.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairRequest {
    pub code: String,
    /// A label for the device, such as its browser; shown by `token list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

/// The answer to a redeemed pairing code. `token` is shown once; the
/// server keeps only its hash.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paired {
    pub token: String,
    pub me: Me,
}

/// `GET /v1/app/me`: the principal the caller's token verifies as.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Me {
    /// The subject name: a configured credential's, or a paired token's.
    pub name: String,
    pub tenant: String,
    pub scopes: Vec<String>,
    /// Its own repository allowlist, when it has one narrower than its
    /// tenant's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repos: Option<Vec<String>>,
    /// `configured` (from the server's configuration) or `paired` (from a
    /// pairing link, revocable with `by serve token revoke`).
    pub kind: String,
    /// When a paired token stops working, in milliseconds since the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
}

/// `GET /v1/app/push`: whether this server sends Web Push notifications,
/// the key a browser subscribes with, and the caller's subscriptions.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushInfo {
    pub enabled: bool,
    /// The VAPID public key (an uncompressed P-256 point, base64url without
    /// padding): the browser's `applicationServerKey`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    /// Push service hosts subscriptions may name.
    pub services: Vec<String>,
    /// The caller's subscriptions' endpoints.
    pub subscriptions: Vec<String>,
    /// What a notification can be about.
    pub kinds: Vec<String>,
}

/// A browser's `PushSubscription.toJSON()`'s `keys`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushKeys {
    /// The browser's P-256 public key, base64url.
    pub p256dh: String,
    /// The 16-byte authentication secret, base64url.
    pub auth: String,
}

/// `POST /v1/app/push/subscriptions`: notify this browser.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushSubscribe {
    pub endpoint: String,
    pub keys: PushKeys,
    /// Accepted and ignored: `PushSubscription.toJSON()` includes it.
    #[serde(
        default,
        rename = "expirationTime",
        skip_serializing_if = "Option::is_none"
    )]
    pub expiration_time: Option<u64>,
    /// Which kinds (see [`PushInfo::kinds`]); empty means all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
}

/// `DELETE /v1/app/push/subscriptions`: stop notifying this endpoint.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushUnsubscribe {
    pub endpoint: String,
}

/// The answer to a subscription change or a test push.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushResult {
    /// Subscriptions of the caller now.
    pub subscriptions: usize,
    /// Notifications a push service accepted (for a test push).
    #[serde(default)]
    pub delivered: usize,
    /// Why some were not, one line each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<String>,
}

/// `POST /v1/app/push/test`: no fields.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushTest {}

impl Client {
    /// Who this client's token verifies as (needs the server's `app`).
    pub fn me(&self) -> Result<Me, Error> {
        self.get("/v1/app/me")
    }
}
