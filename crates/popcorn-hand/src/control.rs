//! Session lifecycle across the two Popcorn access paths.

use nanocodex_tools::mcp::McpHandle;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::PopcornConfig;
use crate::error::PopcornError;
use crate::mcp::{OwnedMcpClient, call_mcp_tool};
use crate::session::{PopcornSession, truncate};
use crate::END_SESSION_TOOL;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateSessionRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ttl_seconds: Option<u64>,
    #[serde(skip_serializing_if = "slice_is_empty")]
    pub(crate) regions: &'a [String],
}

const fn slice_is_empty(regions: &&[String]) -> bool {
    regions.is_empty()
}

#[derive(Deserialize)]
pub(crate) struct CreateSessionResponse {
    #[serde(default)]
    pub(crate) success: Option<bool>,
    #[serde(default)]
    pub(crate) error: Option<String>,
    #[serde(flatten)]
    pub(crate) session: Option<PopcornSession>,
}

impl CreateSessionResponse {
    pub(crate) fn into_session(self) -> Result<PopcornSession, String> {
        if self.success == Some(false) {
            return Err(self
                .error
                .unwrap_or_else(|| "session creation failed".to_owned()));
        }
        self.session
            .ok_or_else(|| "session response is missing cdpInternalUrl".to_owned())
    }
}

/// How a rented session is reached for its remaining lifecycle calls.
pub(crate) enum PopcornControl {
    /// An MCP server, already connected and authorized.
    Mcp {
        /// Kept alive so the OAuth provider stays connected and can refresh.
        #[allow(dead_code)]
        handle: McpHandle,
        client: OwnedMcpClient,
    },
    /// A control plane, reached over HTTP with client credentials.
    ControlPlane(Client),
}

/// Releases a rented session through whichever path rented it.
pub(crate) async fn release_session(
    control: &PopcornControl,
    config: &PopcornConfig,
    session_id: &str,
) -> Result<(), PopcornError> {
    match control {
        PopcornControl::Mcp { client, .. } => {
            let mut arguments = Map::new();
            arguments.insert(
                "session_id".to_owned(),
                Value::String(session_id.to_owned()),
            );
            call_mcp_tool(client, END_SESSION_TOOL, arguments).await?;
            Ok(())
        }
        PopcornControl::ControlPlane(client) => delete_session(client, config, session_id).await,
    }
}

pub(crate) async fn create_session(
    client: &Client,
    config: &PopcornConfig,
) -> Result<PopcornSession, PopcornError> {
    let request = CreateSessionRequest {
        session_id: config.session_id.as_deref(),
        ttl_seconds: config.ttl_seconds,
        regions: &config.regions,
    };
    let response = client
        .post(config.sessions_url()?)
        .header(reqwest::header::AUTHORIZATION, config.bearer()?)
        .json(&request)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(PopcornError::ControlPlane {
            status,
            body: truncate(&body),
        });
    }
    let parsed: CreateSessionResponse =
        serde_json::from_str(&body).map_err(|error| PopcornError::ControlPlane {
            status,
            body: format!("unparseable session response: {error}"),
        })?;
    parsed
        .into_session()
        .map_err(|message| PopcornError::ControlPlane {
            status,
            body: message,
        })
}

pub(crate) async fn delete_session(
    client: &Client,
    config: &PopcornConfig,
    session_id: &str,
) -> Result<(), PopcornError> {
    let response = client
        .delete(config.session_url(session_id)?)
        .header(reqwest::header::AUTHORIZATION, config.bearer()?)
        .send()
        .await?;
    let status = response.status();
    if status.is_success() || status == StatusCode::NOT_FOUND {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    Err(PopcornError::ControlPlane {
        status,
        body: truncate(&body),
    })
}
