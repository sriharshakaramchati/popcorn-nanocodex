//! A Nanocodex browser Hand backed by a rented Popcorn session.
//!
//! [Popcorn](https://github.com/reclaimprotocol/popcorn-oss) rents isolated
//! headful Chromium sessions that run inside a TEE. Each session exposes a
//! full-access CDP WebSocket and a human-viewable LiveView page. This crate
//! rents one session, attaches to its CDP endpoint with `chromiumoxide`, and
//! exposes the browser to a Nanocodex agent as an ordinary [`Tool`]. The
//! session is released on shutdown.
//!
//! From the agent's point of view a Popcorn browser is just another tool: the
//! brain stays wherever it is, and `browser(...)` executes inside the remote
//! enclave. Because the model never receives the CDP URL, the session token
//! never enters the conversation.
//!
//! # Access paths
//!
//! The default path is Popcorn's **hosted MCP server**, which needs no
//! Reclaim-issued credentials. The first run prints an authorization URL to
//! stderr, the operator approves it in a browser, and the OAuth credentials are
//! persisted so later runs start without a login. Sessions are paid for with
//! prepaid credits bought at [`CREDIT_CHECKOUT_URL`].
//!
//! A **credentialed control plane** remains available for dedicated
//! deployments that issue their own client ID and secret.
//!
//! # Rent a browser and hand it to an agent
//!
//! ```no_run
//! use popcorn_hand::{PopcornBrowser, PopcornConfig};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let config = PopcornConfig::from_env()?;
//! let browser = PopcornBrowser::spawn(config).await?;
//!
//! println!("watch or take over at {}", browser.live_view_url());
//!
//! let tool = browser.tool();
//! // Pass `tool` to `nanocodex::Tools::builder().tool(tool)`.
//! browser.shutdown().await?;
//! # Ok(())
//! # }
//! ```
//!
//! Environment read by [`PopcornConfig::from_env`]:
//!
//! - `POPCORN_MCP_URL` (optional): overrides the hosted MCP server URL.
//! - `POPCORN_MCP_CREDENTIALS` (optional): OAuth credential file path.
//! - `POPCORN_PURPOSE` (optional): session purpose shown to the human.
//! - `POPCORN_IDEMPOTENCY_KEY` (optional): reuse to retry without paying twice.
//! - `POPCORN_PROXY_COUNTRY` (optional): ISO 3166-1 alpha-2 proxy exit country.
//! - `POPCORN_REGION` (optional): comma-separated region preference order.
//! - `POPCORN_TTL_SECONDS` (optional): requested lifetime; the hosted MCP
//!   server sells one fixed block and ignores it.
//!
//! Setting any of `POPCORN_CONTROL_PLANE_URL`, `POPCORN_CLIENT_ID`, or
//! `POPCORN_CLIENT_SECRET` selects the credentialed control plane instead, and
//! all three are then required.

mod browser;
mod config;
mod control;
mod creds;
mod error;
mod mcp;
mod session;
mod tool;

pub use browser::PopcornBrowser;
pub use config::PopcornConfig;
pub use error::PopcornError;
pub use session::PopcornSession;
pub use tool::PopcornBrowserTool;

/// Popcorn's hosted Streamable HTTP MCP server.
pub const HOSTED_MCP_URL: &str = "https://popcorn-mcp-gcp.reclaimprotocol.org/mcp";

/// Where a human buys session credits for the hosted MCP server.
pub const CREDIT_CHECKOUT_URL: &str = "https://popcorn.reclaimprotocol.org";

pub(crate) const MCP_SERVER_NAME: &str = "popcorn";
pub(crate) const BALANCE_TOOL: &str = "get_balance";
pub(crate) const CREATE_SESSION_TOOL: &str = "create_browser_session";
pub(crate) const CONNECTION_TOOL: &str = "get_browser_connection";
pub(crate) const LIVE_VIEW_TOOL: &str = "get_live_view";
pub(crate) const END_SESSION_TOOL: &str = "end_browser_session";
pub(crate) const DEFAULT_PURPOSE: &str = "Nanocodex agent browser session";

#[cfg(test)]
mod tests;
