use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nanocodex_tools::mcp::McpOAuthStore;
use url::Url;

use crate::error::PopcornError;
use crate::{DEFAULT_PURPOSE, HOSTED_MCP_URL};

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How a session is rented.
#[derive(Clone)]
pub(crate) enum PopcornAccess {
    /// Popcorn's hosted MCP server, authorized with OAuth and paid with credits.
    HostedMcp {
        server_url: Url,
        oauth_store: Option<Arc<dyn McpOAuthStore>>,
        credentials_path: Option<PathBuf>,
    },
    /// A control plane that issued this client its own credentials.
    ControlPlane {
        control_plane: Url,
        client_id: String,
        client_secret: String,
    },
}

/// Everything needed to rent one Popcorn session.
#[derive(Clone)]
pub struct PopcornConfig {
    pub(crate) access: PopcornAccess,
    pub(crate) regions: Vec<String>,
    pub(crate) ttl_seconds: Option<u64>,
    pub(crate) session_id: Option<String>,
    pub(crate) request_timeout: Duration,
    pub(crate) purpose: Option<String>,
    pub(crate) idempotency_key: Option<String>,
    pub(crate) proxy_country: Option<String>,
}

/// Redacts the client secret and never renders a credential path's contents.
impl std::fmt::Debug for PopcornConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut rendered = f.debug_struct("PopcornConfig");
        match &self.access {
            PopcornAccess::HostedMcp { server_url, .. } => {
                rendered
                    .field("mode", &"hosted_mcp")
                    .field("server_url", server_url);
            }
            PopcornAccess::ControlPlane {
                control_plane,
                client_id,
                ..
            } => {
                rendered
                    .field("mode", &"control_plane")
                    .field("control_plane", control_plane)
                    .field("client_id", client_id)
                    .field("client_secret", &"<redacted>");
            }
        }
        rendered
            .field("regions", &self.regions)
            .field("ttl_seconds", &self.ttl_seconds)
            .field("session_id", &self.session_id)
            .field("request_timeout", &self.request_timeout)
            .field("purpose", &self.purpose)
            .field("idempotency_key", &self.idempotency_key)
            .field("proxy_country", &self.proxy_country)
            .finish()
    }
}

impl PopcornConfig {
    /// Creates a configuration for Popcorn's hosted MCP server.
    ///
    /// The first spawn prints an OAuth authorization URL to stderr; sessions
    /// are paid for with credits bought at [`CREDIT_CHECKOUT_URL`](crate::CREDIT_CHECKOUT_URL).
    #[must_use]
    pub fn hosted_mcp() -> Self {
        Self::mcp(Url::parse(HOSTED_MCP_URL).unwrap_or_else(|error| {
            unreachable!("the hosted Popcorn MCP URL is a valid URL: {error}")
        }))
    }

    /// Creates a configuration for a self-hosted Popcorn MCP server.
    #[must_use]
    pub fn mcp(server_url: Url) -> Self {
        Self::with_access(PopcornAccess::HostedMcp {
            server_url,
            oauth_store: None,
            credentials_path: None,
        })
    }

    /// Creates a configuration for a credentialed Popcorn control plane.
    ///
    /// This is the option for dedicated deployments that issue their own client
    /// ID and secret; ordinary users want [`PopcornConfig::hosted_mcp`].
    pub fn new(
        control_plane: Url,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> Self {
        Self::with_access(PopcornAccess::ControlPlane {
            control_plane,
            client_id: client_id.into(),
            client_secret: client_secret.into(),
        })
    }

    const fn with_access(access: PopcornAccess) -> Self {
        Self {
            access,
            regions: Vec::new(),
            ttl_seconds: None,
            session_id: None,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            purpose: None,
            idempotency_key: None,
            proxy_country: None,
        }
    }

    /// Reads the configuration from `POPCORN_*` environment variables.
    ///
    /// The hosted MCP server is selected unless any of
    /// `POPCORN_CONTROL_PLANE_URL`, `POPCORN_CLIENT_ID`, or
    /// `POPCORN_CLIENT_SECRET` is set, in which case all three are required.
    ///
    /// # Errors
    ///
    /// Returns [`PopcornError::Configuration`](crate::PopcornError) when a required variable is
    /// missing or a URL or number does not parse.
    pub fn from_env() -> Result<Self, PopcornError> {
        Self::from_lookup(&|name| std::env::var(name).ok())
    }

    pub(crate) fn from_lookup(
        lookup: &impl Fn(&str) -> Option<String>,
    ) -> Result<Self, PopcornError> {
        let value = |name: &str| {
            lookup(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let required = |name: &str| {
            value(name).ok_or_else(|| PopcornError::Configuration {
                message: format!("`{name}` is required"),
            })
        };
        let parsed_url = |name: &str, raw: &str| {
            Url::parse(raw).map_err(|error| PopcornError::Configuration {
                message: format!("`{name}` is not a valid URL: {error}"),
            })
        };

        const CREDENTIALED: [&str; 3] = [
            "POPCORN_CONTROL_PLANE_URL",
            "POPCORN_CLIENT_ID",
            "POPCORN_CLIENT_SECRET",
        ];
        let mut config = if CREDENTIALED.iter().any(|name| value(name).is_some()) {
            let control_plane = required(CREDENTIALED[0])?;
            Self::new(
                parsed_url(CREDENTIALED[0], &control_plane)?,
                required(CREDENTIALED[1])?,
                required(CREDENTIALED[2])?,
            )
        } else {
            let mut config = match value("POPCORN_MCP_URL") {
                Some(url) => Self::mcp(parsed_url("POPCORN_MCP_URL", &url)?),
                None => Self::hosted_mcp(),
            };
            if let Some(path) = value("POPCORN_MCP_CREDENTIALS") {
                config = config.credentials_path(path);
            }
            config
        };

        if let Some(regions) = value("POPCORN_REGION") {
            config = config.regions(
                regions
                    .split(',')
                    .map(str::trim)
                    .filter(|region| !region.is_empty())
                    .map(str::to_owned),
            );
        }
        if let Some(ttl) = value("POPCORN_TTL_SECONDS") {
            let ttl = ttl
                .parse::<u64>()
                .map_err(|error| PopcornError::Configuration {
                    message: format!("`POPCORN_TTL_SECONDS` must be a positive integer: {error}"),
                })?;
            config = config.ttl_seconds(ttl);
        }
        if let Some(purpose) = value("POPCORN_PURPOSE") {
            config = config.purpose(purpose);
        }
        if let Some(key) = value("POPCORN_IDEMPOTENCY_KEY") {
            config = config.idempotency_key(key);
        }
        if let Some(country) = value("POPCORN_PROXY_COUNTRY") {
            config = config.proxy_country(country);
        }
        Ok(config)
    }

    /// Returns whether this configuration rents through an MCP server.
    #[must_use]
    pub const fn uses_mcp(&self) -> bool {
        matches!(self.access, PopcornAccess::HostedMcp { .. })
    }

    /// Sets the region preference order, closest to the human first.
    #[must_use]
    pub fn regions(mut self, regions: impl IntoIterator<Item = String>) -> Self {
        self.regions = regions.into_iter().collect();
        self
    }

    /// Requests a session lifetime in seconds.
    ///
    /// MCP servers sell one fixed block and ignore this.
    #[must_use]
    pub const fn ttl_seconds(mut self, ttl_seconds: u64) -> Self {
        self.ttl_seconds = Some(ttl_seconds);
        self
    }

    /// Uses a caller-chosen session identifier instead of a generated one.
    ///
    /// In MCP mode the session identifier is allocated by the server, so this
    /// value is used as the creation idempotency key instead.
    #[must_use]
    pub fn session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Overrides the control-plane or MCP request timeout.
    #[must_use]
    pub const fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Describes what the session is for; MCP servers show this to the human.
    #[must_use]
    pub fn purpose(mut self, purpose: impl Into<String>) -> Self {
        self.purpose = Some(purpose.into());
        self
    }

    /// Pins the MCP creation idempotency key.
    ///
    /// Reuse the same key when retrying an uncertain spawn: the server returns
    /// the same session instead of renting and charging for a second one.
    #[must_use]
    pub fn idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }

    /// Requests a deployment-managed proxy exit country, as ISO 3166-1 alpha-2.
    #[must_use]
    pub fn proxy_country(mut self, country: impl Into<String>) -> Self {
        self.proxy_country = Some(country.into());
        self
    }

    /// Persists MCP OAuth credentials at `path` instead of the default file.
    #[must_use]
    pub fn credentials_path(mut self, path: impl Into<PathBuf>) -> Self {
        if let PopcornAccess::HostedMcp {
            credentials_path, ..
        } = &mut self.access
        {
            *credentials_path = Some(path.into());
        }
        self
    }

    /// Persists MCP OAuth credentials through a caller-owned store.
    ///
    /// Use this to share one credential store with the rest of an application;
    /// otherwise a file store is used.
    #[must_use]
    pub fn oauth_store(mut self, store: Arc<dyn McpOAuthStore>) -> Self {
        if let PopcornAccess::HostedMcp { oauth_store, .. } = &mut self.access {
            *oauth_store = Some(store);
        }
        self
    }

    pub(crate) fn bearer(&self) -> Result<String, PopcornError> {
        match &self.access {
            PopcornAccess::ControlPlane {
                client_id,
                client_secret,
                ..
            } => Ok(format!("Bearer {client_id}:{client_secret}")),
            PopcornAccess::HostedMcp { .. } => Err(PopcornError::Configuration {
                message: "MCP mode has no control-plane bearer token".to_owned(),
            }),
        }
    }

    pub(crate) fn control_plane(&self) -> Result<&Url, PopcornError> {
        match &self.access {
            PopcornAccess::ControlPlane { control_plane, .. } => Ok(control_plane),
            PopcornAccess::HostedMcp { .. } => Err(PopcornError::Configuration {
                message: "MCP mode has no control-plane URL".to_owned(),
            }),
        }
    }

    pub(crate) fn sessions_url(&self) -> Result<Url, PopcornError> {
        self.control_plane()?
            .join("v1/sessions")
            .map_err(|error| PopcornError::Configuration {
                message: format!("cannot build sessions URL: {error}"),
            })
    }

    pub(crate) fn session_url(&self, session_id: &str) -> Result<Url, PopcornError> {
        self.control_plane()?
            .join(&format!("v1/session/{session_id}"))
            .map_err(|error| PopcornError::Configuration {
                message: format!("cannot build session URL: {error}"),
            })
    }

    pub(crate) fn creation_purpose(&self) -> &str {
        self.purpose.as_deref().unwrap_or(DEFAULT_PURPOSE)
    }

    /// Returns the creation idempotency key, generating one when unset.
    pub(crate) fn creation_idempotency_key(&self) -> String {
        self.idempotency_key
            .clone()
            .or_else(|| self.session_id.clone())
            .unwrap_or_else(generated_idempotency_key)
    }
}

/// Builds a key unique to one spawn attempt in this process.
pub(crate) fn generated_idempotency_key() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("nanocodex-{:x}-{nanos:x}-{sequence:x}", std::process::id())
}
