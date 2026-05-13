use crate::acp::{classify_notification, AcpEvent, ContentBlock, SessionPool};
use crate::config::HttpConfig;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::post,
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tracing::info;

const MAX_SESSION_ID: usize = 128;
const MAX_PROMPT: usize = 64 * 1024;

#[derive(Clone)]
struct AppState {
    pool: Arc<SessionPool>,
    token: String,
    timeout_ms: u64,
}

#[derive(Deserialize)]
struct PromptRequest {
    session_id: String,
    prompt: String,
}

#[derive(Serialize)]
struct PromptResponse {
    response: String,
    session_reset: bool,
}

fn bearer_authorized(headers: &HeaderMap, token: &str) -> bool {
    if token.is_empty() {
        return true;
    }

    // Extract Bearer token; scheme comparison is case-insensitive per RFC 7235.
    let provided = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let mut parts = v.splitn(2, ' ');
            let scheme = parts.next()?;
            if !scheme.eq_ignore_ascii_case("bearer") {
                return None;
            }
            parts.next()
        })
        .unwrap_or("");

    provided.as_bytes().ct_eq(token.as_bytes()).into()
}

fn validate_prompt_request(req: &PromptRequest) -> Result<(), (StatusCode, String)> {
    if req.session_id.is_empty() || req.prompt.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "session_id and prompt are required".into(),
        ));
    }
    if req.session_id.len() > MAX_SESSION_ID {
        return Err((StatusCode::BAD_REQUEST, "session_id too long".into()));
    }
    if req.prompt.len() > MAX_PROMPT {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "prompt too long".into()));
    }
    Ok(())
}

async fn handle_prompt(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<PromptRequest>,
) -> Result<Json<PromptResponse>, (StatusCode, String)> {
    if !bearer_authorized(&headers, &state.token) {
        return Err((StatusCode::UNAUTHORIZED, "invalid token".into()));
    }

    validate_prompt_request(&req)?;

    // Namespace HTTP sessions so they cannot collide with Discord thread IDs.
    let pool_key = format!("http:{}", req.session_id);

    state.pool.get_or_create(&pool_key).await.map_err(|e| {
        tracing::warn!(session_id = %req.session_id, err = %e, "pool unavailable");
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "service unavailable".into(),
        )
    })?;

    let timeout = std::time::Duration::from_millis(state.timeout_ms);
    match tokio::time::timeout(timeout, run_prompt(&state.pool, &pool_key, &req.prompt)).await {
        Ok(Ok(resp)) => Ok(Json(resp)),
        Ok(Err(e)) => {
            tracing::error!(session_id = %req.session_id, err = %e, "prompt failed");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "internal error".into()))
        }
        Err(_) => {
            tracing::warn!(session_id = %req.session_id, "prompt timeout; resetting http session");
            if let Err(e) = state.pool.reset_session(&pool_key).await {
                tracing::warn!(session_id = %req.session_id, err = %e, "failed to reset timed-out http session");
            }
            Err((StatusCode::GATEWAY_TIMEOUT, "prompt timeout".into()))
        }
    }
}

async fn run_prompt(
    pool: &SessionPool,
    pool_key: &str,
    prompt: &str,
) -> anyhow::Result<PromptResponse> {
    pool.with_connection(pool_key, |conn| {
        let prompt = prompt.to_string();
        Box::pin(async move {
            let reset = conn.session_reset;
            conn.session_reset = false;

            let blocks = vec![ContentBlock::Text {
                text: prompt.clone(),
            }];
            let (mut rx, request_id) = match conn.session_prompt(blocks).await {
                Ok(r) => r,
                Err(e) => {
                    // Always clear the subscriber slot even on failure.
                    conn.prompt_done().await;
                    return Err(e);
                }
            };

            let mut text_buf = String::new();
            while let Some(notification) = rx.recv().await {
                if let Some(notification_id) = notification.id {
                    if notification_id != request_id {
                        continue;
                    }
                    if let Some(err) = notification.error {
                        return Err(anyhow::anyhow!("agent returned {err}"));
                    }
                    break;
                }
                if let Some(AcpEvent::Text(t)) = classify_notification(&notification) {
                    text_buf.push_str(&t);
                }
            }

            conn.prompt_done().await;
            Ok(PromptResponse {
                response: text_buf,
                session_reset: reset,
            })
        })
    })
    .await
}

pub async fn run_server(pool: Arc<SessionPool>, cfg: HttpConfig) -> anyhow::Result<()> {
    let state = Arc::new(AppState {
        pool,
        token: cfg.token,
        timeout_ms: cfg.timeout_ms,
    });
    let app = Router::new()
        .route("/prompt", post(handle_prompt))
        .with_state(state);

    let addr = format!("{}:{}", cfg.bind, cfg.port);
    info!(addr = %addr, "http trigger server starting");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {addr}: {e}"))?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::AUTHORIZATION;

    fn prompt_request(session_id: &str, prompt: &str) -> PromptRequest {
        PromptRequest {
            session_id: session_id.to_string(),
            prompt: prompt.to_string(),
        }
    }

    #[test]
    fn bearer_authorized_accepts_valid_token_case_insensitive_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "bEaReR secret".parse().unwrap());

        assert!(bearer_authorized(&headers, "secret"));
    }

    #[test]
    fn bearer_authorized_rejects_missing_wrong_or_wrong_scheme() {
        assert!(!bearer_authorized(&HeaderMap::new(), "secret"));

        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer nope".parse().unwrap());
        assert!(!bearer_authorized(&headers, "secret"));

        headers.insert(AUTHORIZATION, "Basic secret".parse().unwrap());
        assert!(!bearer_authorized(&headers, "secret"));
    }

    #[test]
    fn bearer_authorized_allows_empty_token_for_internal_tests_only() {
        assert!(bearer_authorized(&HeaderMap::new(), ""));
    }

    #[test]
    fn validate_prompt_request_accepts_valid_payload() {
        assert!(validate_prompt_request(&prompt_request("session", "hello")).is_ok());
    }

    #[test]
    fn validate_prompt_request_rejects_empty_fields() {
        let err = validate_prompt_request(&prompt_request("", "hello")).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        let err = validate_prompt_request(&prompt_request("session", "")).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn validate_prompt_request_rejects_large_payloads() {
        let err =
            validate_prompt_request(&prompt_request(&"s".repeat(MAX_SESSION_ID + 1), "hello"))
                .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        let err = validate_prompt_request(&prompt_request("session", &"p".repeat(MAX_PROMPT + 1)))
            .unwrap_err();
        assert_eq!(err.0, StatusCode::PAYLOAD_TOO_LARGE);
    }
}
