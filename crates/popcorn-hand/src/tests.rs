use std::path::{Path, PathBuf};

use serde_json::json;

use crate::config::{PopcornAccess, PopcornConfig, generated_idempotency_key};
use crate::control::{CreateSessionRequest, CreateSessionResponse};
use crate::error::PopcornError;
use crate::creds::{PopcornCredentialFile, default_credentials_path};
use crate::session::{
    McpSessionDraft, PopcornSession, checkout_link, credit_shortfall, lookup_str,
    payload_is_out_of_credit,
};
use crate::{HOSTED_MCP_URL, MCP_SERVER_NAME};
use nanocodex_tools::mcp::{McpOAuthCredentials, McpOAuthStore};
use url::Url;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let pairs = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect::<std::collections::BTreeMap<_, _>>();
        move |name: &str| pairs.get(name).cloned()
    }

    #[test]
    fn create_request_omits_empty_fields() {
        let request = CreateSessionRequest {
            session_id: None,
            ttl_seconds: None,
            regions: &[],
        };
        assert_eq!(serde_json::to_string(&request).unwrap(), "{}");
    }

    #[test]
    fn create_request_serialises_camel_case() {
        let regions = vec!["asia-south1".to_owned()];
        let request = CreateSessionRequest {
            session_id: Some("demo"),
            ttl_seconds: Some(600),
            regions: &regions,
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"sessionId":"demo","ttlSeconds":600,"regions":["asia-south1"]}"#
        );
    }

    #[test]
    fn session_response_parses_reference_shape() {
        let body = r#"{
            "success": true,
            "sessionId": "demo-session",
            "url": "https://browser.example.com/liveview/demo-session/t/liveview.html",
            "cdpUrl": "wss://browser.example.com/cdp/demo-session/t/",
            "cdpInternalUrl": "wss://browser.example.com/cdp-internal/demo-session/t/",
            "apiUrl": "https://browser.example.com/api/demo-session/t/",
            "browserPodId": "browser-fleet-abc",
            "expiresAt": "2026-08-04T12:30:00.000Z",
            "region": "us-central1",
            "clusterName": "popcorn-prod-us"
        }"#;
        let parsed: CreateSessionResponse = serde_json::from_str(body).unwrap();
        let session = parsed.session.unwrap();
        assert_eq!(session.session_id, "demo-session");
        assert_eq!(session.cdp_internal_url.scheme(), "wss");
        assert_eq!(session.region.as_deref(), Some("us-central1"));
    }

    #[test]
    fn debug_output_redacts_session_urls() {
        let body = r#"{
            "sessionId": "demo-session",
            "url": "https://browser.example.com/liveview/demo-session/tok/liveview.html",
            "cdpUrl": "wss://browser.example.com/cdp/demo-session/tok/",
            "cdpInternalUrl": "wss://browser.example.com/cdp-internal/demo-session/tok/"
        }"#;
        let session: PopcornSession = serde_json::from_str(body).unwrap();
        let rendered = format!("{session:?}");
        assert!(!rendered.contains("tok"), "session URLs leaked: {rendered}");
        assert!(!rendered.contains("browser.example.com"));
        assert!(rendered.contains("demo-session"));
    }

    #[test]
    fn debug_output_redacts_client_secret() {
        let config = PopcornConfig::new(
            Url::parse("https://control.example.com/").unwrap(),
            "client-id",
            "super-secret",
        );
        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains("super-secret"),
            "secret leaked: {rendered}"
        );
        assert!(rendered.contains("client-id"));
    }

    #[test]
    fn failed_response_surfaces_error_message() {
        let body = r#"{"success": false, "error": "no capacity in region"}"#;
        let parsed: CreateSessionResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.success, Some(false));
        assert_eq!(parsed.error.as_deref(), Some("no capacity in region"));
        assert!(parsed.session.is_none());
    }

    #[test]
    fn config_builds_control_plane_urls() {
        let config = PopcornConfig::new(
            Url::parse("https://control.example.com/").unwrap(),
            "id",
            "secret",
        );
        assert_eq!(
            config.sessions_url().unwrap().as_str(),
            "https://control.example.com/v1/sessions"
        );
        assert_eq!(
            config.session_url("abc").unwrap().as_str(),
            "https://control.example.com/v1/session/abc"
        );
        assert_eq!(config.bearer().unwrap(), "Bearer id:secret");
    }

    #[test]
    fn create_session_result_maps_onto_a_session() {
        // The shape returned by the hosted MCP server: flat snake_case JSON.
        let payload = json!({
            "session_id": "pop-123",
            "live_view_url": "https://browser.example.com/liveview/pop-123/tok/",
            "cdp_url": "wss://browser.example.com/cdp/pop-123/tok/",
            "region": "asia-south1",
            "expires_at": "2026-09-11T09:00:00.000Z",
            "block_seconds": 600
        });
        let session = McpSessionDraft::from_create(&payload)
            .unwrap()
            .finish()
            .unwrap();
        assert_eq!(session.session_id, "pop-123");
        assert_eq!(session.url.scheme(), "https");
        assert_eq!(session.cdp_url.scheme(), "wss");
        // MCP servers return one agent-facing CDP URL for both fields.
        assert_eq!(session.cdp_internal_url, session.cdp_url);
        assert_eq!(session.region.as_deref(), Some("asia-south1"));
        assert_eq!(
            session.expires_at.as_deref(),
            Some("2026-09-11T09:00:00.000Z")
        );
        assert!(session.browser_pod_id.is_none());
    }

    #[test]
    fn create_session_result_accepts_a_camel_case_envelope() {
        let payload = json!({
            "ok": true,
            "session": {
                "sessionId": "pop-456",
                "liveViewUrl": "https://browser.example.com/liveview/pop-456/tok/",
                "connectUrl": "wss://browser.example.com/cdp/pop-456/tok/",
                "expiresAt": "2026-09-11T09:10:00.000Z"
            }
        });
        let session = McpSessionDraft::from_create(&payload)
            .unwrap()
            .finish()
            .unwrap();
        assert_eq!(session.session_id, "pop-456");
        assert_eq!(session.cdp_url.scheme(), "wss");
        assert!(session.region.is_none());
    }

    #[test]
    fn connection_and_live_view_results_fill_a_partial_session() {
        let mut draft = McpSessionDraft::from_create(&json!({ "session_id": "pop-789" })).unwrap();
        assert!(draft.cdp_url.is_none());
        assert!(draft.live_view_url.is_none());

        draft.merge_connection(&json!({
            "cdp_url": "wss://browser.example.com/cdp/pop-789/tok/",
            "region": "us-central1",
            "expires_at": "2026-09-11T09:20:00.000Z"
        }));
        // `get_live_view` names the link `url`, unlike the creation result.
        draft.merge_live_view(&json!({
            "url": "https://browser.example.com/liveview/pop-789/tok/"
        }));

        let session = draft.finish().unwrap();
        assert_eq!(session.session_id, "pop-789");
        assert_eq!(session.region.as_deref(), Some("us-central1"));
        assert_eq!(session.url.path(), "/liveview/pop-789/tok/");
    }

    #[test]
    fn create_session_result_without_a_session_id_is_an_error() {
        let Err(error) = McpSessionDraft::from_create(&json!({ "cdp_url": "wss://example.com/" }))
        else {
            panic!("a session id is required");
        };
        assert!(
            matches!(
                error,
                PopcornError::MissingField {
                    field: "session_id"
                }
            ),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn session_without_a_cdp_url_is_an_error() {
        let draft = McpSessionDraft::from_create(&json!({
            "session_id": "pop-1",
            "live_view_url": "https://example.com/live"
        }))
        .unwrap();
        let error = draft.finish().expect_err("a CDP URL is required");
        assert!(
            matches!(error, PopcornError::MissingField { field: "cdp_url" }),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn session_without_a_live_view_url_is_an_error() {
        let draft = McpSessionDraft::from_create(&json!({
            "session_id": "pop-1",
            "cdp_url": "wss://example.com/cdp"
        }))
        .unwrap();
        let error = draft.finish().expect_err("a LiveView URL is required");
        assert!(
            matches!(
                error,
                PopcornError::MissingField {
                    field: "live_view_url"
                }
            ),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_metered_account_without_credit_reports_a_shortfall() {
        // The shape returned by the hosted server's `get_balance`.
        let empty = json!({
            "credits": 0,
            "metered": true,
            "session_block_seconds": 600,
            "credits_per_operation": 1
        });
        assert!(credit_shortfall(&empty).is_some(), "no credit left");

        let funded = json!({ "credits": 12, "metered": true });
        assert!(credit_shortfall(&funded).is_none());
        // An unmetered deployment reports no shortfall whatever the balance is.
        let unmetered = json!({ "credits": 0, "metered": false });
        assert!(credit_shortfall(&unmetered).is_none());
        let unknown = json!({ "credits": null, "metered": true });
        assert!(credit_shortfall(&unknown).is_none());
    }

    #[test]
    fn an_out_of_credit_refusal_is_recognised_by_its_error_field() {
        assert!(payload_is_out_of_credit(
            &json!({ "error": "INSUFFICIENT_CREDIT" })
        ));
        assert!(payload_is_out_of_credit(
            &json!({ "code": "payment_required" })
        ));
        assert!(!payload_is_out_of_credit(
            &json!({ "error": "no capacity" })
        ));
        assert!(!payload_is_out_of_credit(&json!({ "session_id": "pop-1" })));
    }

    #[test]
    fn the_hosted_servers_refusal_yields_its_approval_link() {
        // Verbatim shape returned by the hosted server when credit runs out.
        let refusal = json!({
            "error": "insufficient_credit",
            "message": "Not enough usage credit for this operation.",
            "next_action": {
                "type": "external_approval",
                "url": "https://popcorn-billing-gcp.reclaimprotocol.org/checkout?token=opaque"
            },
            "next": "Give the human next_action to obtain more credit, then retry with the same idempotency_key."
        });
        assert!(payload_is_out_of_credit(&refusal));
        assert_eq!(
            checkout_link(&refusal).as_deref(),
            Some("https://popcorn-billing-gcp.reclaimprotocol.org/checkout?token=opaque")
        );
    }

    #[test]
    fn a_live_view_url_is_never_read_as_a_payment_link() {
        // A successful creation has a `url`, which must not look like checkout.
        let created = json!({
            "session_id": "pop-1",
            "url": "https://browser.example.com/liveview/pop-1/tok/",
            "cdp_url": "wss://browser.example.com/cdp/pop-1/tok/"
        });
        assert!(checkout_link(&created).is_none());
        assert!(!payload_is_out_of_credit(&created));
    }

    #[test]
    fn the_servers_checkout_link_is_found_in_a_refusal() {
        // Only the creation response carries the link a human can pay at, so it
        // must survive whatever envelope the server wraps it in.
        let flat = json!({
            "error": "insufficient_credit",
            "checkout_url": "https://checkout.example.com/session/abc"
        });
        assert_eq!(
            lookup_str(&flat, CHECKOUT_KEYS).as_deref(),
            Some("https://checkout.example.com/session/abc")
        );
        let nested = json!({
            "error": { "code": "insufficient_credit" },
            "billing": { "checkoutUrl": "https://checkout.example.com/session/xyz" }
        });
        assert_eq!(
            lookup_str(&nested, CHECKOUT_KEYS).as_deref(),
            Some("https://checkout.example.com/session/xyz")
        );
    }

    #[test]
    fn from_env_defaults_to_the_hosted_mcp_server() {
        let config = PopcornConfig::from_lookup(&lookup(&[])).unwrap();
        assert!(config.uses_mcp());
        let PopcornAccess::HostedMcp { server_url, .. } = &config.access else {
            panic!("expected MCP mode");
        };
        assert_eq!(server_url.as_str(), HOSTED_MCP_URL);
    }

    #[test]
    fn from_env_overrides_the_mcp_server_url() {
        let config = PopcornConfig::from_lookup(&lookup(&[(
            "POPCORN_MCP_URL",
            "https://popcorn.internal.example.com/mcp",
        )]))
        .unwrap();
        let PopcornAccess::HostedMcp { server_url, .. } = &config.access else {
            panic!("expected MCP mode");
        };
        assert_eq!(
            server_url.as_str(),
            "https://popcorn.internal.example.com/mcp"
        );
    }

    #[test]
    fn from_env_selects_credentialed_mode_when_all_three_are_set() {
        let config = PopcornConfig::from_lookup(&lookup(&[
            ("POPCORN_CONTROL_PLANE_URL", "https://control.example.com/"),
            ("POPCORN_CLIENT_ID", "id"),
            ("POPCORN_CLIENT_SECRET", "secret"),
            ("POPCORN_REGION", "us-central1, asia-south1"),
            ("POPCORN_TTL_SECONDS", "900"),
        ]))
        .unwrap();
        assert!(!config.uses_mcp());
        assert_eq!(config.bearer().unwrap(), "Bearer id:secret");
        assert_eq!(config.regions, ["us-central1", "asia-south1"]);
        assert_eq!(config.ttl_seconds, Some(900));
    }

    #[test]
    fn from_env_rejects_a_partial_credentialed_configuration() {
        let error = PopcornConfig::from_lookup(&lookup(&[
            ("POPCORN_CONTROL_PLANE_URL", "https://control.example.com/"),
            ("POPCORN_CLIENT_ID", "id"),
        ]))
        .expect_err("a partial credentialed configuration is an error");
        let message = error.to_string();
        assert!(message.contains("POPCORN_CLIENT_SECRET"), "{message}");
    }

    #[test]
    fn from_env_reads_the_mcp_session_options() {
        let config = PopcornConfig::from_lookup(&lookup(&[
            ("POPCORN_PURPOSE", "book a flight"),
            ("POPCORN_IDEMPOTENCY_KEY", "run-42"),
            ("POPCORN_PROXY_COUNTRY", "IN"),
            ("POPCORN_MCP_CREDENTIALS", "/tmp/popcorn.json"),
        ]))
        .unwrap();
        assert_eq!(config.creation_purpose(), "book a flight");
        assert_eq!(config.creation_idempotency_key(), "run-42");
        assert_eq!(config.proxy_country.as_deref(), Some("IN"));
        let PopcornAccess::HostedMcp {
            credentials_path, ..
        } = &config.access
        else {
            panic!("expected MCP mode");
        };
        assert_eq!(
            credentials_path.as_deref(),
            Some(Path::new("/tmp/popcorn.json"))
        );
    }

    #[test]
    fn a_generated_idempotency_key_is_unique_per_attempt() {
        let config = PopcornConfig::hosted_mcp();
        assert_eq!(config.creation_purpose(), DEFAULT_PURPOSE);
        assert_ne!(
            config.creation_idempotency_key(),
            config.creation_idempotency_key()
        );
        // A caller-chosen session id pins the key so a retry cannot pay twice.
        let pinned = PopcornConfig::hosted_mcp().session_id("run-7");
        assert_eq!(pinned.creation_idempotency_key(), "run-7");
    }

    #[test]
    fn mcp_config_debug_names_the_mode_without_a_secret() {
        let rendered = format!("{:?}", PopcornConfig::hosted_mcp());
        assert!(rendered.contains("hosted_mcp"), "{rendered}");
        assert!(!rendered.contains("client_secret"), "{rendered}");
    }

    #[tokio::test]
    async fn the_credential_file_round_trips_and_stays_private() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("oauth.json");
        let store = PopcornCredentialFile::new(path.clone());
        assert!(
            store
                .load(MCP_SERVER_NAME, HOSTED_MCP_URL)
                .await
                .unwrap()
                .is_none()
        );

        let credentials = McpOAuthCredentials::new("client-1", "access-1")
            .refresh_token("refresh-1")
            .issuer("https://issuer.example.com")
            .expires_at_millis(1_800_000_000_000)
            .scopes(["popcorn.sessions", "popcorn.credit"]);
        store
            .save(MCP_SERVER_NAME, HOSTED_MCP_URL, &credentials)
            .await
            .unwrap();

        let loaded = store
            .load(MCP_SERVER_NAME, HOSTED_MCP_URL)
            .await
            .unwrap()
            .expect("stored credentials");
        assert_eq!(loaded.client_id(), "client-1");
        assert_eq!(loaded.access_token(), "access-1");
        assert_eq!(loaded.refresh_token_value(), Some("refresh-1"));
        assert_eq!(loaded.expires_at(), Some(1_800_000_000_000));
        assert_eq!(
            loaded.granted_scopes(),
            ["popcorn.sessions", "popcorn.credit"]
        );
        // Another server URL must not see these credentials.
        assert!(
            store
                .load(MCP_SERVER_NAME, "https://other.example.com/mcp")
                .await
                .unwrap()
                .is_none()
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "credential file is not owner-only");
        }
    }

    #[test]
    fn default_credentials_path_prefers_the_configured_path() {
        let path = default_credentials_path(Some(PathBuf::from("/tmp/popcorn.json"))).unwrap();
        assert_eq!(path, Path::new("/tmp/popcorn.json"));
    }
