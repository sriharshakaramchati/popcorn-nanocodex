use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use fs2::FileExt;
use nanocodex_tools::mcp::{McpOAuthCredentials, McpOAuthRefreshGuard, McpOAuthStore};
use serde::{Deserialize, Serialize};

use crate::error::PopcornError;

/// Returns the credential file path, defaulting under the user's home.
pub(crate) fn default_credentials_path(
    configured: Option<PathBuf>,
) -> Result<PathBuf, PopcornError> {
    if let Some(path) = configured {
        return Ok(path);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
        .ok_or_else(|| PopcornError::Configuration {
            message: "`HOME` is not set; set `POPCORN_MCP_CREDENTIALS` to a writable path"
                .to_owned(),
        })?;
    Ok(home.join(".nanocodex").join("popcorn-mcp-oauth.json"))
}

/// A file-backed [`McpOAuthStore`] so a second run needs no login.
///
/// The file holds one entry per server URL and is created with owner-only
/// permissions. An exclusive lock serialises readers, writers, and token
/// refreshes across processes sharing the same file.
pub(crate) struct PopcornCredentialFile {
    path: PathBuf,
}

#[derive(Default, Serialize, Deserialize)]
struct StoredCredentialFile {
    #[serde(default)]
    servers: std::collections::BTreeMap<String, StoredCredential>,
}

#[derive(Serialize, Deserialize)]
struct StoredCredential {
    client_id: String,
    access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    issuer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scopes: Vec<String>,
}

/// An exclusive lock on the credential file, held until dropped.
struct CredentialFileLock {
    _file: std::fs::File,
}

impl PopcornCredentialFile {
    pub(crate) const fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn lock_path(&self) -> PathBuf {
        let mut path = self.path.clone();
        let name = path
            .file_name()
            .map(|name| format!("{}.lock", name.to_string_lossy()))
            .unwrap_or_else(|| "popcorn-mcp-oauth.lock".to_owned());
        path.set_file_name(name);
        path
    }

    fn acquire_lock(&self) -> Result<CredentialFileLock, String> {
        let path = self.lock_path();
        create_parent(&path)?;
        let file = open_private(&path)?;
        let started = std::time::Instant::now();
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(CredentialFileLock { _file: file }),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if started.elapsed() >= Duration::from_secs(60) {
                        return Err(format!(
                            "timed out waiting for the Popcorn credential lock {}",
                            path.display()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => {
                    return Err(format!(
                        "failed to lock the Popcorn credential file {}: {error}",
                        path.display()
                    ));
                }
            }
        }
    }

    fn read_file(&self) -> Result<StoredCredentialFile, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(contents) if contents.trim().is_empty() => Ok(StoredCredentialFile::default()),
            Ok(contents) => serde_json::from_str(&contents).map_err(|error| {
                format!(
                    "failed to parse the Popcorn credential file {}: {error}",
                    self.path.display()
                )
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(StoredCredentialFile::default())
            }
            Err(error) => Err(format!(
                "failed to read the Popcorn credential file {}: {error}",
                self.path.display()
            )),
        }
    }

    fn write_file(&self, file: &StoredCredentialFile) -> Result<(), String> {
        create_parent(&self.path)?;
        let encoded = serde_json::to_vec_pretty(file)
            .map_err(|error| format!("failed to encode Popcorn credentials: {error}"))?;
        // Create the file with owner-only permissions before writing a token to it.
        drop(open_private(&self.path)?);
        std::fs::write(&self.path, encoded).map_err(|error| {
            format!(
                "failed to write the Popcorn credential file {}: {error}",
                self.path.display()
            )
        })
    }

    fn load_blocking(&self, server_url: &str) -> Result<Option<McpOAuthCredentials>, String> {
        let _lock = self.acquire_lock()?;
        let Some(entry) = self.read_file()?.servers.remove(server_url) else {
            return Ok(None);
        };
        if entry.client_id.trim().is_empty() || entry.access_token.trim().is_empty() {
            return Err("stored Popcorn OAuth credentials are incomplete".to_owned());
        }
        let mut credentials =
            McpOAuthCredentials::new(entry.client_id, entry.access_token).scopes(entry.scopes);
        if let Some(refresh_token) = entry.refresh_token.filter(|token| !token.trim().is_empty()) {
            credentials = credentials.refresh_token(refresh_token);
        }
        if let Some(issuer) = entry.issuer.filter(|issuer| !issuer.trim().is_empty()) {
            credentials = credentials.issuer(issuer);
        }
        if let Some(expires_at) = entry.expires_at {
            credentials = credentials.expires_at_millis(expires_at);
        }
        Ok(Some(credentials))
    }

    fn save_blocking(
        &self,
        server_url: &str,
        credentials: &McpOAuthCredentials,
    ) -> Result<(), String> {
        let _lock = self.acquire_lock()?;
        let mut file = self.read_file()?;
        file.servers.insert(
            server_url.to_owned(),
            StoredCredential {
                client_id: credentials.client_id().to_owned(),
                access_token: credentials.access_token().to_owned(),
                refresh_token: credentials.refresh_token_value().map(ToOwned::to_owned),
                issuer: credentials.authorization_issuer().map(ToOwned::to_owned),
                expires_at: credentials.expires_at(),
                scopes: credentials.granted_scopes().to_vec(),
            },
        );
        self.write_file(&file)
    }
}

fn create_parent(path: &Path) -> Result<(), String> {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))
}

/// Opens or creates a file readable and writable only by its owner.
fn open_private(path: &Path) -> Result<std::fs::File, String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|error| format!("failed to open {}: {error}", path.display()))
}

#[async_trait]
impl McpOAuthStore for PopcornCredentialFile {
    async fn load(
        &self,
        _server_name: &str,
        server_url: &str,
    ) -> Result<Option<McpOAuthCredentials>, String> {
        let store = Self::new(self.path.clone());
        let server_url = server_url.to_owned();
        tokio::task::spawn_blocking(move || store.load_blocking(&server_url))
            .await
            .map_err(|error| format!("Popcorn credential reader stopped: {error}"))?
    }

    async fn save(
        &self,
        _server_name: &str,
        server_url: &str,
        credentials: &McpOAuthCredentials,
    ) -> Result<(), String> {
        let store = Self::new(self.path.clone());
        let server_url = server_url.to_owned();
        let credentials = credentials.clone();
        tokio::task::spawn_blocking(move || store.save_blocking(&server_url, &credentials))
            .await
            .map_err(|error| format!("Popcorn credential writer stopped: {error}"))?
    }

    async fn acquire_refresh_lock(
        &self,
        _server_name: &str,
        _server_url: &str,
    ) -> Result<Box<dyn McpOAuthRefreshGuard>, String> {
        let store = Self::new(self.path.clone());
        tokio::task::spawn_blocking(move || {
            store
                .acquire_lock()
                .map(|lock| Box::new(lock) as Box<dyn McpOAuthRefreshGuard>)
        })
        .await
        .map_err(|error| format!("Popcorn credential lock task stopped: {error}"))?
    }
}
