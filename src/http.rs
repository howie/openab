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
use tracing::info;

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
        let provided = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if provided != state.token {
            return Err((StatusCode::UNAUTHORIZED, "invalid token".into()));
        }
    }

    if req.session_id.is_empty() || req.prompt.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "session_id and prompt are required".into()));
    }

    state
        .pool
        .get_or_create(&req.session_id)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;

    let timeout = std::time::Duration::from_millis(state.timeout_ms);
    tokio::time::timeout(
        timeout,
        run_prompt(&state.pool, &req.session_id, &req.prompt),
    )
    .await
    .map_err(|_| (StatusCode::GATEWAY_TIMEOUT, "prompt timeout".into()))?
    .map(Json)
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn run_prompt(
    pool: &SessionPool,
    session_id: &str,
    prompt: &str,
) -> anyhow::Result<PromptResponse> {
    pool.with_connection(session_id, |conn| {
        let prompt = prompt.to_string();
        Box::pin(async move {
            let reset = conn.session_reset;
            conn.session_reset = false;

            let (mut rx, _) = conn.session_prompt(&prompt).await?;
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
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
