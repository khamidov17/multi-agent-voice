//! HTTP server for the Gemini Live mini app.
//!
//! Serves the mini app HTML and validates Telegram initData before returning the Gemini API key.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tracing::{info, warn};

type HmacSha256 = Hmac<Sha256>;

/// Shared state for the API server.
#[derive(Clone)]
pub struct ApiState {
    pub bot_token: String,
    /// All user IDs allowed to access the live mini app (owners + live_allowed_users).
    pub live_allowed_ids: Arc<std::collections::HashSet<i64>>,
    pub gemini_api_key: String,
}

#[derive(Deserialize)]
struct LiveConfigRequest {
    init_data: String,
}

#[derive(Serialize)]
struct LiveConfigResponse {
    gemini_api_key: String,
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/live", get(serve_live_html))
        .route("/api/live-config", post(live_config))
        .with_state(state)
}

async fn serve_live_html() -> Html<&'static str> {
    Html(include_str!("../static/live.html"))
}

async fn live_config(
    State(state): State<ApiState>,
    Json(payload): Json<LiveConfigRequest>,
) -> Response {
    if state.gemini_api_key.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, "Gemini API key not configured").into_response();
    }

    let user_id = match validate_init_data(&payload.init_data, &state.bot_token) {
        Some(id) => id,
        None => {
            // Allow empty initData in dev (no Telegram context)
            if payload.init_data.is_empty() {
                info!("No initData provided — allowing in dev mode");
                // Use first owner ID as fallback
                state.live_allowed_ids.iter().next().copied().unwrap_or(0)
            } else {
                warn!("Invalid Telegram initData");
                return (StatusCode::UNAUTHORIZED, "invalid initData").into_response();
            }
        }
    };

    if !state.live_allowed_ids.contains(&user_id) {
        warn!("Live app access denied for user_id={}", user_id);
        return (StatusCode::FORBIDDEN, "access denied").into_response();
    }

    info!("Live app config served to owner user_id={}", user_id);
    Json(LiveConfigResponse {
        gemini_api_key: state.gemini_api_key.clone(),
    })
    .into_response()
}

/// Validate Telegram WebApp initData using HMAC-SHA256.
///
/// Returns the user_id if valid, None if invalid or not parseable.
fn validate_init_data(init_data: &str, bot_token: &str) -> Option<i64> {
    // Parse as URL query string
    let params: HashMap<String, String> = form_urlencoded::parse(init_data.as_bytes())
        .into_owned()
        .collect();

    let hash = params.get("hash")?.clone();

    // Build the data-check string: sorted "key=value" pairs (excluding "hash"), joined by \n
    let mut parts: Vec<String> = params
        .iter()
        .filter(|(k, _)| *k != "hash")
        .map(|(k, v)| format!("{}={}", k, v))
        .collect();
    parts.sort();
    let check_string = parts.join("\n");

    // secret_key = HMAC-SHA256(key="WebAppData", data=bot_token)
    let mut secret_mac = HmacSha256::new_from_slice(b"WebAppData").ok()?;
    secret_mac.update(bot_token.as_bytes());
    let secret_key = secret_mac.finalize().into_bytes();

    // actual_hash = HMAC-SHA256(key=secret_key, data=check_string)
    let mut data_mac = HmacSha256::new_from_slice(&secret_key).ok()?;
    data_mac.update(check_string.as_bytes());
    let actual_hash = data_mac.finalize().into_bytes();
    let actual_hex = hex::encode(actual_hash);

    if actual_hex != hash {
        return None;
    }

    // Extract user_id from the "user" param (JSON)
    let user_json = params.get("user")?;
    let user: serde_json::Value = serde_json::from_str(user_json).ok()?;
    user["id"].as_i64()
}
