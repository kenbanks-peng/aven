use std::time::Duration;

use aven_core::sync::protocol::{discovery_request, protocol_mismatch};
use reqwest::{StatusCode, redirect::Policy};
use serde::Deserialize;

use crate::config::{self, AppConfig};
use crate::sync::{self, wire::SYNC_PROTOCOL_VERSION};

const RESPONSE_LIMIT: usize = 16 * 1024;

#[derive(Clone)]
pub(crate) struct ConfiguredSyncServer {
    url: String,
    auth_token: Option<String>,
}

impl ConfiguredSyncServer {
    pub(crate) fn from_config(config: &AppConfig) -> Option<Self> {
        let url = config::resolve_sync_server(None, config).ok()?;
        let url = url.trim().trim_end_matches('/').to_string();
        if !sync::sync_server_url_is_valid(&url) {
            return None;
        }
        Some(Self {
            url,
            auth_token: config.sync_auth_token().map(str::to_string),
        })
    }

    pub(crate) fn origin(&self) -> String {
        reqwest::Url::parse(&self.url)
            .ok()
            .map(|url| url.origin().ascii_serialization())
            .unwrap_or_else(|| "configured sync server".to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompatibilityFailure {
    ProtocolMarkerMissing,
    ConfigUnreadable,
    Unreachable,
    AuthenticationRejected,
    UnexpectedResponse,
}

impl CompatibilityFailure {
    pub(crate) fn explanation(&self) -> &'static str {
        match self {
            Self::ProtocolMarkerMissing => "The release does not identify its sync protocol.",
            Self::ConfigUnreadable => "Aven could not read the sync server configuration.",
            Self::Unreachable => "The configured sync server could not be reached.",
            Self::AuthenticationRejected => "The configured sync server rejected authentication.",
            Self::UnexpectedResponse => {
                "The configured sync server returned an unexpected response."
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompatibilityResult {
    NotRequired,
    Compatible,
    Incompatible {
        target: u32,
        server: u32,
    },
    Unverified {
        target: Option<u32>,
        reason: CompatibilityFailure,
    },
}

impl CompatibilityResult {
    pub(crate) fn requires_confirmation(&self) -> bool {
        matches!(self, Self::Incompatible { .. } | Self::Unverified { .. })
    }
}

pub(crate) fn retains_current_sync_support(target: Option<u32>, baseline: Option<u32>) -> bool {
    target == Some(SYNC_PROTOCOL_VERSION)
        && baseline.unwrap_or(SYNC_PROTOCOL_VERSION)
            <= aven_core::sync::protocol::MAINTAINED_PROTOCOL_BASELINE
}

pub(crate) async fn assess_sync_compatibility(
    target: Option<u32>,
    baseline: Option<u32>,
    server: Option<ConfiguredSyncServer>,
) -> CompatibilityResult {
    let Some(server) = server else {
        return CompatibilityResult::NotRequired;
    };
    let Some(target) = target else {
        return CompatibilityResult::Unverified {
            target: None,
            reason: CompatibilityFailure::ProtocolMarkerMissing,
        };
    };
    if retains_current_sync_support(Some(target), baseline) {
        return CompatibilityResult::NotRequired;
    }
    probe_server(target, baseline.unwrap_or(target), &server).await
}

async fn probe_server(
    target: u32,
    baseline: u32,
    server: &ConfiguredSyncServer,
) -> CompatibilityResult {
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .redirect(Policy::none())
        .retry(reqwest::retry::never())
        .user_agent(format!("aven/{}", super::CURRENT_VERSION))
        .build()
    {
        Ok(client) => client,
        Err(_) => return unverified(target, CompatibilityFailure::Unreachable),
    };
    let request = client
        .post(format!("{}/sync", server.url))
        .json(&discovery_request(
            target,
            "aven-update-preflight".to_string(),
        ));
    let request = match &server.auth_token {
        Some(token) => request.bearer_auth(token),
        None => request,
    };
    let response = match request.send().await {
        Ok(response) => response,
        Err(_) => return unverified(target, CompatibilityFailure::Unreachable),
    };
    let status = response.status();
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return unverified(target, CompatibilityFailure::AuthenticationRejected);
    }
    let body = match read_body_limited(response).await {
        Some(body) => body,
        None => return unverified(target, CompatibilityFailure::UnexpectedResponse),
    };
    if status.is_success() {
        let parsed = serde_json::from_slice::<ProbeResponse>(&body);
        return match parsed {
            Ok(response)
                if response.protocol_version == target
                    && response.changes.is_empty()
                    && response.push_acks.is_empty() =>
            {
                CompatibilityResult::Compatible
            }
            _ => unverified(target, CompatibilityFailure::UnexpectedResponse),
        };
    }
    if status == StatusCode::BAD_REQUEST
        && let Ok(detail) = std::str::from_utf8(&body)
        && let Some((client, server)) = protocol_mismatch(detail)
        && client == target
    {
        return if (baseline..=target).contains(&server) {
            CompatibilityResult::Compatible
        } else {
            CompatibilityResult::Incompatible { target, server }
        };
    }
    unverified(target, CompatibilityFailure::UnexpectedResponse)
}

#[derive(Deserialize)]
struct ProbeResponse {
    protocol_version: u32,
    #[serde(default)]
    push_acks: Vec<serde_json::Value>,
    #[serde(default)]
    changes: Vec<serde_json::Value>,
}

async fn read_body_limited(mut response: reqwest::Response) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some(body)
}

fn unverified(target: u32, reason: CompatibilityFailure) -> CompatibilityResult {
    CompatibilityResult::Unverified {
        target: Some(target),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_support_requires_matching_active_and_preserved_baseline() {
        let active = SYNC_PROTOCOL_VERSION;
        let baseline = aven_core::sync::protocol::MAINTAINED_PROTOCOL_BASELINE;
        assert!(!retains_current_sync_support(None, None));
        assert!(!retains_current_sync_support(None, Some(baseline)));
        assert_eq!(
            retains_current_sync_support(Some(active), None),
            active == baseline
        );
        assert!(retains_current_sync_support(Some(active), Some(baseline)));
        assert!(retains_current_sync_support(
            Some(active),
            Some(baseline - 1)
        ));
        assert!(!retains_current_sync_support(
            Some(active),
            Some(baseline + 1)
        ));
        assert!(!retains_current_sync_support(
            Some(active + 1),
            Some(baseline)
        ));
    }

    #[test]
    fn parses_only_exact_protocol_mismatch_errors() {
        assert_eq!(
            protocol_mismatch("error sync-protocol-unsupported client=19 server=18"),
            Some((19, 18))
        );
        assert_eq!(
            protocol_mismatch("error sync-protocol-unsupported client=18 server=18"),
            None
        );
        assert_eq!(protocol_mismatch("server=18 client=19"), None);
    }

    #[tokio::test]
    async fn classifies_a_server_protocol_mismatch() {
        let target = SYNC_PROTOCOL_VERSION + 1;
        let app = axum::Router::new().route(
            "/sync",
            axum::routing::post(
                move |axum::Json(request): axum::Json<serde_json::Value>| async move {
                    assert_eq!(request["protocol_version"], target);
                    assert_eq!(request["after"], i64::MAX);
                    assert_eq!(request["pull_limit"], 1);
                    assert_eq!(request["changes"], serde_json::json!([]));
                    (
                        StatusCode::BAD_REQUEST,
                        format!(
                            "error sync-protocol-unsupported client={target} server={SYNC_PROTOCOL_VERSION}"
                        ),
                    )
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let server = ConfiguredSyncServer {
            url: format!("http://{address}"),
            auth_token: None,
        };

        assert_eq!(
            assess_sync_compatibility(
                Some(target),
                Some(SYNC_PROTOCOL_VERSION),
                Some(server.clone())
            )
            .await,
            CompatibilityResult::Compatible,
        );
        assert_eq!(
            assess_sync_compatibility(Some(target), None, Some(server)).await,
            CompatibilityResult::Incompatible {
                target,
                server: SYNC_PROTOCOL_VERSION,
            }
        );
    }

    #[tokio::test]
    async fn absent_server_and_current_protocol_need_no_probe() {
        assert_eq!(
            assess_sync_compatibility(Some(99), None, None).await,
            CompatibilityResult::NotRequired
        );
        let server = ConfiguredSyncServer {
            url: "http://127.0.0.1:1".to_string(),
            auth_token: None,
        };
        assert_eq!(
            assess_sync_compatibility(Some(SYNC_PROTOCOL_VERSION), None, Some(server)).await,
            CompatibilityResult::NotRequired
        );
    }
}
