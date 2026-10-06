//! MCP transport for Popcorn session management.
//!
//! OAuth login, credential persistence, and token refresh reuse the published
//! `nanocodex-tools` MCP machinery (`Mcp`, `McpHandle`, `McpOAuthStore`). The
//! published crate exposes those tools to a model only, so the handful of
//! programmatic session calls this crate needs (`create_browser_session` and
//! friends) go through a small owned Streamable HTTP JSON-RPC client below.
//! Session URLs and OAuth tokens never leave this module.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nanocodex_tools::mcp::{Mcp, McpHandle, McpOAuthStore, McpServer};
use reqwest::Client;
use serde_json::{Map, Value};
use tracing::debug;

use crate::config::{PopcornAccess, PopcornConfig};
use crate::creds::{PopcornCredentialFile, default_credentials_path};
use crate::error::PopcornError;
use crate::session::{
    McpSessionDraft, checkout_link, credit_shortfall, payload_is_out_of_credit, truncate,
};
use crate::{
    BALANCE_TOOL, CONNECTION_TOOL, CREATE_SESSION_TOOL, CREDIT_CHECKOUT_URL, LIVE_VIEW_TOOL,
    MCP_SERVER_NAME, PopcornSession,
};

/// The MCP protocol revision this client speaks.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// One MCP tool result, modelled on the shape `create_browser_session` returns.
pub(crate) struct McpToolCall {
    is_error: bool,
    text: String,
    structured: Option<Value>,
}

impl McpToolCall {
    pub(crate) const fn is_error(&self) -> bool {
        self.is_error
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// The tool payload as JSON: `structuredContent` when present, otherwise
    /// the first text content block parsed as JSON.
    pub(crate) fn json(&self) -> Result<Value, String> {
        if let Some(structured) = &self.structured {
            return Ok(structured.clone());
        }
        serde_json::from_str(&self.text)
            .map_err(|error| format!("tool result text is not JSON: {error}"))
    }
}

/// A minimal Streamable HTTP MCP client for programmatic tool calls.
///
/// The client performs the `initialize` handshake itself and tracks the
/// server-issued session id. It exists because the published nanocodex MCP
/// provider only routes calls from a model; see the module docs.
pub(crate) struct OwnedMcpClient {
    http: Client,
    server_url: String,
    access_token: String,
    session_id: Option<String>,
    next_id: AtomicU64,
}

impl OwnedMcpClient {
    /// Connects and completes the MCP initialize handshake.
    pub(crate) async fn connect(
        config: &PopcornConfig,
        access_token: String,
    ) -> Result<Self, PopcornError> {
        let PopcornAccess::HostedMcp { server_url, .. } = &config.access else {
            return Err(PopcornError::Configuration {
                message: "MCP client requires an MCP configuration".to_owned(),
            });
        };
        let http = Client::builder()
            .timeout(config.request_timeout)
            .build()?;
        let client = Self {
            http,
            server_url: server_url.to_string(),
            access_token,
            session_id: None,
            next_id: AtomicU64::new(1),
        };
        let session_id = client
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "popcorn-hand", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await?
            .1;
        // `notifications/initialized` carries no id and expects no body back.
        client
            .notify("notifications/initialized", serde_json::json!({}))
            .await?;
        debug!(target: "popcorn_hand", "popcorn MCP handshake complete");
        Ok(Self {
            session_id,
            ..client
        })
    }

    /// Calls one tool and maps the JSON-RPC envelope into an [`McpToolCall`].
    pub(crate) async fn call_tool(
        &self,
        tool: &str,
        arguments: Map<String, Value>,
    ) -> Result<McpToolCall, PopcornError> {
        let (result, _session) = self
            .request(
                "tools/call",
                serde_json::json!({ "name": tool, "arguments": arguments }),
            )
            .await?;
        if let Some(error) = result.get("error") {
            return Err(PopcornError::Mcp {
                message: format!(
                    "`{tool}` returned a protocol error: {}",
                    truncate(&error.to_string())
                ),
            });
        }
        let result = result.get("result").cloned().unwrap_or(Value::Null);
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let structured = result.get("structuredContent").cloned();
        Ok(McpToolCall {
            is_error,
            text,
            structured,
        })
    }

    /// Sends one JSON-RPC request, returning the parsed body and any
    /// server-issued session id.
    async fn request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(Value, Option<String>), PopcornError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let request = self
            .http
            .post(&self.server_url)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.access_token),
            );
        let request = match &self.session_id {
            Some(session) => request.header("mcp-session-id", session),
            None => request,
        };
        let response = request.json(&body).send().await?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(PopcornError::Mcp {
                message: "the Popcorn MCP server rejected the OAuth token; \
                     delete the credential file and sign in again"
                    .to_owned(),
            });
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(PopcornError::ControlPlane {
                status,
                body: truncate(&body),
            });
        }
        let session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.text().await?;
        parse_response_body(&body).map(|value| (value, session_id))
    }

    /// Sends one JSON-RPC notification, which has no response body to parse.
    async fn notify(&self, method: &str, params: Value) -> Result<(), PopcornError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut request = self
            .http
            .post(&self.server_url)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.access_token),
            )
            .json(&body);
        if let Some(session) = &self.session_id {
            request = request.header("mcp-session-id", session);
        }
        let response = request.send().await?;
        let status = response.status();
        // Servers answer notifications with 200/202 and no meaningful body.
        if status.is_success() || status == reqwest::StatusCode::ACCEPTED {
            Ok(())
        } else {
            let body = response.text().await.unwrap_or_default();
            Err(PopcornError::ControlPlane {
                status,
                body: truncate(&body),
            })
        }
    }
}

/// Parses a Streamable HTTP response body, which is either one JSON document
/// or an SSE stream whose `data:` lines carry the JSON-RPC message.
fn parse_response_body(body: &str) -> Result<Value, PopcornError> {
    let trimmed = body.trim();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed).map_err(|error| PopcornError::Mcp {
            message: format!("unparseable MCP response: {error}"),
        });
    }
    for line in trimmed.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim();
            if data.starts_with('{') {
                return serde_json::from_str(data).map_err(|error| PopcornError::Mcp {
                    message: format!("unparseable MCP SSE payload: {error}"),
                });
            }
        }
    }
    Err(PopcornError::Mcp {
        message: format!("MCP response carried no JSON-RPC message: {}", truncate(trimmed)),
    })
}

/// Connects to the MCP server, running a browser OAuth login when needed.
///
/// Returns the control handle (kept alive for reconnects and refresh) and an
/// owned programmatic client authorized with the persisted credentials.
pub(crate) async fn connect_mcp(
    config: &PopcornConfig,
) -> Result<(McpHandle, OwnedMcpClient), PopcornError> {
    let PopcornAccess::HostedMcp {
        server_url,
        oauth_store,
        credentials_path,
    } = &config.access
    else {
        return Err(PopcornError::Configuration {
            message: "MCP connection requires an MCP configuration".to_owned(),
        });
    };
    let store = match oauth_store {
        Some(store) => Arc::clone(store),
        None => Arc::new(PopcornCredentialFile::new(default_credentials_path(
            credentials_path.clone(),
        )?)) as Arc<dyn McpOAuthStore>,
    };
    let provider = Mcp::builder()
        .oauth_store(Arc::clone(&store))
        .server(
            MCP_SERVER_NAME,
            McpServer::http(server_url.as_str())
                .description("Popcorn remote browser sessions")
                .startup_timeout(config.request_timeout)
                .tool_timeout(config.request_timeout),
        )
        .build()
        .map_err(|error| PopcornError::Mcp {
            message: error.to_string(),
        })?;
    let handle = provider.handle();
    // A first run has no stored credentials, so the unauthenticated connect
    // fails and the login below both authorizes and reloads the server.
    if let Err(connect_error) = handle.reload(MCP_SERVER_NAME).await {
        let login = handle
            .login(MCP_SERVER_NAME)
            .await
            .map_err(|error| PopcornError::Mcp {
                message: format!("{connect_error}; OAuth login could not start: {error}"),
            })?;
        eprintln!(
            "popcorn: authorize this client by opening\n  {}",
            login.authorization_url()
        );
        eprintln!("popcorn: waiting for the browser callback...");
        login.wait().await.map_err(|error| PopcornError::Mcp {
            message: format!("OAuth login failed: {error}"),
        })?;
        eprintln!("popcorn: authorized");
    }
    let credentials = store
        .load(MCP_SERVER_NAME, server_url.as_str())
        .await
        .map_err(|error| PopcornError::Mcp {
            message: format!("failed to load the stored OAuth credentials: {error}"),
        })?
        .ok_or_else(|| PopcornError::Mcp {
            message: "the OAuth login completed but no credentials were stored".to_owned(),
        })?;
    let client = OwnedMcpClient::connect(config, credentials.access_token().to_owned()).await?;
    Ok((handle, client))
}

/// Rents one session through the MCP server.
pub(crate) async fn create_mcp_session(
    client: &OwnedMcpClient,
    config: &PopcornConfig,
) -> Result<PopcornSession, PopcornError> {
    // The balance is free to read and worth logging, but it must not short
    // circuit creation: only `create_browser_session` returns the human
    // checkout link for buying credits, so an out-of-credit run has to reach it.
    if let Ok(balance) = call_mcp_tool(client, BALANCE_TOOL, Map::new()).await
        && let Ok(payload) = balance.json()
        && let Some(shortfall) = credit_shortfall(&payload)
    {
        debug!(
            target: "popcorn_hand",
            shortfall,
            "popcorn reports no session credit; asking the server for a checkout link"
        );
    }

    let mut arguments = Map::new();
    arguments.insert(
        "purpose".to_owned(),
        Value::String(config.creation_purpose().to_owned()),
    );
    arguments.insert(
        "idempotency_key".to_owned(),
        Value::String(config.creation_idempotency_key()),
    );
    if !config.regions.is_empty() {
        arguments.insert(
            "regions".to_owned(),
            Value::Array(
                config
                    .regions
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect::<Vec<_>>(),
            ),
        );
    }
    if let Some(country) = &config.proxy_country {
        arguments.insert("proxy_country".to_owned(), Value::String(country.clone()));
    }

    let created = call_mcp_tool(client, CREATE_SESSION_TOOL, arguments).await?;
    let mut draft = McpSessionDraft::from_create(&created.json().map_err(map_mcp_error)?)?;
    if draft.missing_cdp() {
        let mut arguments = Map::new();
        arguments.insert(
            "session_id".to_owned(),
            Value::String(draft.session_id.clone()),
        );
        let connection = call_mcp_tool(client, CONNECTION_TOOL, arguments).await?;
        draft.merge_connection(&connection.json().map_err(map_mcp_error)?);
    }
    if draft.missing_live_view() {
        let mut arguments = Map::new();
        arguments.insert(
            "session_id".to_owned(),
            Value::String(draft.session_id.clone()),
        );
        let live_view = call_mcp_tool(client, LIVE_VIEW_TOOL, arguments).await?;
        draft.merge_live_view(&live_view.json().map_err(map_mcp_error)?);
    }
    draft.finish()
}

/// Calls one Popcorn tool, turning a tool-level error into a typed error.
pub(crate) async fn call_mcp_tool(
    client: &OwnedMcpClient,
    tool: &str,
    arguments: Map<String, Value>,
) -> Result<McpToolCall, PopcornError> {
    let call = client
        .call_tool(tool, arguments)
        .await
        .map_err(|error| PopcornError::Mcp {
            message: error.to_string(),
        })?;
    if !call.is_error() {
        return Ok(call);
    }
    let detail = call.text().trim().to_owned();
    let payload = call.json().ok();
    let checkout = payload.as_ref().and_then(checkout_link);
    // An out-of-credit creation answers with a checkout link for the human;
    // hand it over unchanged and stop rather than retrying.
    if let Some(checkout) = checkout {
        return Err(PopcornError::InsufficientCredit {
            message: format!(
                "open {checkout} to buy credits, then retry with the same idempotency key"
            ),
        });
    }
    if payload.as_ref().is_some_and(payload_is_out_of_credit) {
        // The server sent no `checkout_url` field, so pass its own wording
        // through: deployments put the payment link in the message text.
        return Err(PopcornError::InsufficientCredit {
            message: if detail.is_empty() {
                format!("buy credits at {CREDIT_CHECKOUT_URL}")
            } else {
                truncate(&detail)
            },
        });
    }
    Err(PopcornError::Mcp {
        message: format!("`{tool}` failed: {}", truncate(&detail)),
    })
}

const fn map_mcp_error(message: String) -> PopcornError {
    PopcornError::Mcp { message }
}
