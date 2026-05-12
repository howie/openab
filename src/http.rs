use crate::acp::{classify_notification, AcpEvent, SessionPool};
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

async fn handle_prompt(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<PromptRequest>,
) -> Result<Json<PromptResponse>, (StatusCode, String)> {
    if !state.token.is_empty() {
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
        let ok: bool = provided.as_bytes().ct_eq(state.token.as_bytes()).into();
        if !ok {
            return Err((StatusCode::UNAUTHORIZED, "invalid token".into()));
        }
    }

    if req.session_id.is_empty() || req.prompt.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "session_id and prompt are required".into()));
    }
    if req.session_id.len() > MAX_SESSION_ID {
        return Err((StatusCode::BAD_REQUEST, "session_id too long".into()));
    }
    if req.prompt.len() > MAX_PROMPT {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "prompt too long".into()));
    }

    // Namespace HTTP sessions so they cannot collide with Discord thread IDs.
    let pool_key = format!("http:{}", req.session_id);

    state
        .pool
        .get_or_create(&pool_key)
        .await
        .map_err(|e| {
            tracing::warn!(session_id = %req.session_id, err = %e, "pool unavailable");
            (StatusCode::SERVICE_UNAVAILABLE, "service unavailable".into())
        })?;

    let timeout = std::time::Duration::from_millis(state.timeout_ms);
    tokio::time::timeout(
        timeout,
        run_prompt(&state.pool, &pool_key, &req.prompt),
    )
    .await
    .map_err(|_| (StatusCode::GATEWAY_TIMEOUT, "prompt timeout".into()))?
    .map(Json)
    .map_err(|e| {
        tracing::error!(session_id = %req.session_id, err = %e, "prompt failed");
        (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
    })
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

            let (mut rx, _) = match conn.session_prompt(&prompt).await {
                Ok(r) => r,
                Err(e) => {
                    // Always clear the subscriber slot even on failure.
                    conn.prompt_done().await;
                    return Err(e);
                }
            };

            let mut text_buf = String::new();
            while let Some(notification) = rx.recv().await {
                if notification.id.is_some() {
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
    let listener = tokio::net::TcpListener::bind(&addr).await
        .map_err(|e| anyhow::anyhow!("failed to bind {addr}: {e}"))?;
    axum::serve(listener, app).await?;
    Ok(())
}
