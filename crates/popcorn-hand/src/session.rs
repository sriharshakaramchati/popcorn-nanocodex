use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::error::PopcornError;

pub(crate) const SESSION_ID_KEYS: &[&str] = &["session_id", "sessionId"];
pub(crate) const CDP_KEYS: &[&str] = &[
    "cdp_url",
    "cdpUrl",
    "cdp_internal_url",
    "cdpInternalUrl",
    "connect_url",
    "connectUrl",
];
pub(crate) const LIVE_VIEW_KEYS: &[&str] = &[
    "live_view_url",
    "liveViewUrl",
    "live_url",
    "liveUrl",
    "live_view",
    "liveView",
];
pub(crate) const REGION_KEYS: &[&str] = &["region"];
pub(crate) const EXPIRES_KEYS: &[&str] = &["expires_at", "expiresAt"];
pub(crate) const CHECKOUT_KEYS: &[&str] = &["checkout_url", "checkoutUrl"];
pub(crate) const NEXT_ACTION_KEYS: &[&str] = &["next_action", "nextAction"];

/// The session record returned by Popcorn.
///
/// Every URL in this record is a bearer secret. Log the session id, never
/// the URLs.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PopcornSession {
    /// Popcorn session identifier.
    pub session_id: String,
    /// Human-facing LiveView page for watching or taking over the browser.
    pub url: Url,
    /// Restricted client-facing CDP endpoint.
    pub cdp_url: Url,
    /// Trusted full-access CDP endpoint used by the agent.
    ///
    /// MCP servers return exactly one agent-facing CDP URL, so in MCP mode this
    /// holds the same endpoint as [`PopcornSession::cdp_url`].
    pub cdp_internal_url: Url,
    /// Allocated browser pod identity.
    #[serde(default)]
    pub browser_pod_id: Option<String>,
    /// Session deadline when Popcorn set one.
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Selected region.
    #[serde(default)]
    pub region: Option<String>,
    /// Selected cluster.
    #[serde(default)]
    pub cluster_name: Option<String>,
}

/// Redacts every session URL, each of which is a bearer secret.
impl std::fmt::Debug for PopcornSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PopcornSession")
            .field("session_id", &self.session_id)
            .field("url", &"<redacted>")
            .field("cdp_url", &"<redacted>")
            .field("cdp_internal_url", &"<redacted>")
            .field("browser_pod_id", &self.browser_pod_id)
            .field("expires_at", &self.expires_at)
            .field("region", &self.region)
            .field("cluster_name", &self.cluster_name)
            .finish()
    }
}

/// A session assembled from one or more MCP tool results.
///
/// `create_browser_session` normally returns every field at once; the optional
/// fields are filled from `get_browser_connection` and `get_live_view` when a
/// deployment splits them across tools.
pub(crate) struct McpSessionDraft {
    pub(crate) session_id: String,
    cdp_url: Option<String>,
    live_view_url: Option<String>,
    region: Option<String>,
    expires_at: Option<String>,
}

impl McpSessionDraft {
    /// Reads a `create_browser_session` payload.
    pub(crate) fn from_create(payload: &Value) -> Result<Self, PopcornError> {
        let session_id =
            lookup_str(payload, SESSION_ID_KEYS).ok_or(PopcornError::MissingField {
                field: "session_id",
            })?;
        Ok(Self {
            session_id,
            cdp_url: lookup_str(payload, CDP_KEYS),
            live_view_url: lookup_str(payload, LIVE_VIEW_KEYS),
            region: lookup_str(payload, REGION_KEYS),
            expires_at: lookup_str(payload, EXPIRES_KEYS),
        })
    }

    /// Returns whether the CDP endpoint still needs a follow-up call.
    pub(crate) const fn missing_cdp(&self) -> bool {
        self.cdp_url.is_none()
    }

    /// Returns whether the LiveView page still needs a follow-up call.
    pub(crate) const fn missing_live_view(&self) -> bool {
        self.live_view_url.is_none()
    }

    /// Merges a `get_browser_connection` payload into the missing fields.
    pub(crate) fn merge_connection(&mut self, payload: &Value) {
        self.cdp_url = self
            .cdp_url
            .take()
            .or_else(|| lookup_str(payload, CDP_KEYS));
        self.live_view_url = self
            .live_view_url
            .take()
            .or_else(|| lookup_str(payload, LIVE_VIEW_KEYS));
        self.region = self
            .region
            .take()
            .or_else(|| lookup_str(payload, REGION_KEYS));
        self.expires_at = self
            .expires_at
            .take()
            .or_else(|| lookup_str(payload, EXPIRES_KEYS));
    }

    /// Merges a `get_live_view` payload, which may name the link `url`.
    pub(crate) fn merge_live_view(&mut self, payload: &Value) {
        self.live_view_url = self.live_view_url.take().or_else(|| {
            lookup_str(payload, LIVE_VIEW_KEYS).or_else(|| lookup_str(payload, &["url"]))
        });
    }

    /// Builds the session record, requiring a CDP endpoint and a LiveView page.
    pub(crate) fn finish(self) -> Result<PopcornSession, PopcornError> {
        let cdp_url = parse_session_url(
            self.cdp_url
                .ok_or(PopcornError::MissingField { field: "cdp_url" })?,
            "cdp_url",
        )?;
        let url = parse_session_url(
            self.live_view_url
                .ok_or(PopcornError::MissingField {
                    field: "live_view_url",
                })?,
            "live_view_url",
        )?;
        Ok(PopcornSession {
            session_id: self.session_id,
            url,
            cdp_url: cdp_url.clone(),
            cdp_internal_url: cdp_url,
            browser_pod_id: None,
            expires_at: self.expires_at,
            region: self.region,
            cluster_name: None,
        })
    }
}

/// Parses a session URL without ever rendering it in the error.
fn parse_session_url(raw: String, field: &'static str) -> Result<Url, PopcornError> {
    Url::parse(&raw).map_err(|error| PopcornError::Mcp {
        message: format!("`{field}` is not a valid URL: {error}"),
    })
}

/// Finds the first string at any of `keys`, searching nested objects.
///
/// MCP deployments wrap results in envelopes such as `{"session": {...}}`, so
/// the search descends through objects and arrays rather than assuming a shape.
pub(crate) fn lookup_str(payload: &Value, keys: &[&str]) -> Option<String> {
    const MAX_DEPTH: usize = 4;
    fn walk(value: &Value, keys: &[&str], depth: usize) -> Option<String> {
        if depth == 0 {
            return None;
        }
        match value {
            Value::Object(fields) => {
                for key in keys {
                    if let Some(found) = fields
                        .get(*key)
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|found| !found.is_empty())
                    {
                        return Some(found.to_owned());
                    }
                }
                fields
                    .values()
                    .find_map(|nested| walk(nested, keys, depth - 1))
            }
            Value::Array(items) => items
                .iter()
                .find_map(|nested| walk(nested, keys, depth - 1)),
            _ => None,
        }
    }
    walk(payload, keys, MAX_DEPTH)
}

/// Describes the shortfall when a metered account has no session credit left.
///
/// This is diagnostic only. The authoritative out-of-credit signal is the
/// `create_browser_session` response, which carries the checkout link a human
/// needs; see the MCP call wrapper.
pub(crate) fn credit_shortfall(payload: &Value) -> Option<String> {
    let metered = payload
        .get("metered")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let credits = payload.get("credits").and_then(Value::as_f64)?;
    if !metered || credits > 0.0 {
        return None;
    }
    Some(format!(
        "{credits} credits remaining on a metered deployment"
    ))
}

/// Finds the human approval link in an out-of-credit refusal.
///
/// The hosted server answers with a `next_action` of type `external_approval`
/// holding the link; other deployments may name it `checkout_url`. The link is
/// read only from those positions, never from a generic `url` field, so a
/// session's LiveView URL can never be mistaken for a payment link.
pub(crate) fn checkout_link(payload: &Value) -> Option<String> {
    for key in NEXT_ACTION_KEYS {
        if let Some(action) = payload.get(*key)
            && let Some(url) = lookup_str(action, &["url"])
        {
            return Some(url);
        }
    }
    lookup_str(payload, CHECKOUT_KEYS)
}

/// Detects an out-of-credit refusal that carried no checkout link.
pub(crate) fn payload_is_out_of_credit(payload: &Value) -> bool {
    payload
        .get("error")
        .or_else(|| payload.get("code"))
        .and_then(Value::as_str)
        .is_some_and(|error| {
            let error = error.to_ascii_lowercase();
            error.contains("credit") || error.contains("payment")
        })
}

pub(crate) fn truncate(body: &str) -> String {
    const LIMIT: usize = 512;
    if body.len() <= LIMIT {
        body.to_owned()
    } else {
        let mut end = LIMIT;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &body[..end])
    }
}
