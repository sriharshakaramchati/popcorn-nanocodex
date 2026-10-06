//! The rented-session Hand: CDP attach plus lifecycle.

use std::sync::Arc;

use futures_util::StreamExt;
use tracing::{debug, warn};
use url::Url;

use crate::config::{PopcornAccess, PopcornConfig};
use crate::control::{PopcornControl, create_session, release_session};
use crate::error::PopcornError;
use crate::mcp::{connect_mcp, create_mcp_session};
use crate::session::PopcornSession;
use crate::tool::PopcornBrowserTool;

/// One rented Popcorn session wrapped as a Nanocodex browser Hand.
///
/// Dropping the value without calling [`PopcornBrowser::shutdown`] leaves the
/// session to Popcorn's TTL controller. Call `shutdown` to release it now.
pub struct PopcornBrowser {
    config: PopcornConfig,
    control: PopcornControl,
    session: PopcornSession,
    browser: Arc<chromiumoxide::Browser>,
    handler: tokio::task::JoinHandle<()>,
}

impl PopcornBrowser {
    /// Rents a Popcorn session and attaches to its CDP endpoint.
    ///
    /// In MCP mode the first call prints an OAuth authorization URL to stderr
    /// and waits for the operator to approve it in a browser.
    ///
    /// # Errors
    ///
    /// Returns an error when Popcorn refuses the session, the account is out of
    /// credit, or the CDP endpoint cannot be reached.
    pub async fn spawn(config: PopcornConfig) -> Result<Self, PopcornError> {
        nanocodex_oai_api::transport::install_default_rustls_crypto_provider();
        let (control, session) = match &config.access {
            PopcornAccess::HostedMcp { .. } => {
                let (handle, client) = connect_mcp(&config).await?;
                let session = create_mcp_session(&client, &config).await?;
                (PopcornControl::Mcp { handle, client }, session)
            }
            PopcornAccess::ControlPlane { .. } => {
                let client = reqwest::Client::builder()
                    .timeout(config.request_timeout)
                    .build()?;
                let session = create_session(&client, &config).await?;
                (PopcornControl::ControlPlane(client), session)
            }
        };
        debug!(
            target: "popcorn_hand",
            session = %session.session_id,
            region = ?session.region,
            mcp = config.uses_mcp(),
            "rented popcorn session"
        );
        // The session is already rented, so release it rather than leaking it to
        // the TTL controller when the attach fails.
        let (browser, mut events) = match chromiumoxide::Browser::connect(
            session.cdp_internal_url.as_str(),
        )
        .await
        {
            Ok(pair) => pair,
            Err(error) => {
                if let Err(release_error) =
                    release_session(&control, &config, &session.session_id).await
                {
                    warn!(
                        target: "popcorn_hand",
                        session = %session.session_id,
                        error = %release_error,
                        "popcorn session release failed after a failed attach; \
                         TTL controller will reclaim it"
                    );
                }
                return Err(PopcornError::Browser(error));
            }
        };
        let handler = tokio::spawn(async move {
            while let Some(event) = events.next().await {
                if let Err(error) = event {
                    warn!(target: "popcorn_hand", %error, "browser handler stopped");
                    break;
                }
            }
        });
        Ok(Self {
            config,
            control,
            session,
            browser: Arc::new(browser),
            handler,
        })
    }

    /// Returns the browser controller driving the remote session.
    #[must_use]
    pub fn browser(&self) -> &Arc<chromiumoxide::Browser> {
        &self.browser
    }

    /// Wraps the remote session as an ordinary Nanocodex browser tool.
    #[must_use]
    pub fn tool(&self) -> PopcornBrowserTool {
        PopcornBrowserTool::new(Arc::clone(&self.browser))
    }

    /// Returns the session record. Treat every URL in it as a secret.
    #[must_use]
    pub const fn session(&self) -> &PopcornSession {
        &self.session
    }

    /// Returns the LiveView page a human can open to watch or take over.
    #[must_use]
    pub const fn live_view_url(&self) -> &Url {
        &self.session.url
    }

    /// Releases the session and stops the CDP connection.
    ///
    /// Releasing the session tears down the remote browser pod, so no separate
    /// browser close is needed; the CDP handler task is aborted after the
    /// release completes.
    ///
    /// # Errors
    ///
    /// Returns an error when the release fails. Popcorn's TTL controller
    /// reclaims the session regardless.
    pub async fn shutdown(self) -> Result<(), PopcornError> {
        let release_result =
            release_session(&self.control, &self.config, &self.session.session_id).await;
        if let Err(error) = &release_result {
            warn!(
                target: "popcorn_hand",
                session = %self.session.session_id,
                %error,
                "popcorn session release failed; TTL controller will reclaim it"
            );
        }
        self.handler.abort();
        release_result
    }
}
