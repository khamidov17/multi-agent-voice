//! Web dashboard for the Trio multi-agent system.
//!
//! Provides a Trello-like task board, Telegram-style messaging UI, and agent
//! status monitoring — all behind password authentication.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tracing::{error, info, warn};

type HmacSha256 = Hmac<Sha256>;

/// Tokens are valid for 24 hours.
const TOKEN_VALIDITY_SECS: i64 = 86400;

/// Allowed values for the task `priority` field.
const VALID_PRIORITIES: &[&str] = &["low", "medium", "high", "urgent"];
/// Allowed values for the task `status` field.
const VALID_STATUSES: &[&str] = &["todo", "in_progress", "blocked", "done", "cancelled"];

// ---------- State ----------

#[derive(Clone)]
pub struct DashboardState {
    /// Path to data/shared/bot_messages.db
    pub shared_db_path: PathBuf,
    /// Bot token used as HMAC key material for auth tokens.
    pub bot_token: String,
    /// For sending messages to Telegram from the web UI.
    pub telegram_bot_token: String,
    /// The bot_xona group chat ID.
    pub group_chat_id: i64,
    /// Dashboard credentials (loaded from config file, never hardcoded).
    pub auth_username: String,
    pub auth_password: String,
}

// ---------- Auth helpers ----------

fn derive_secret(bot_token: &str) -> Vec<u8> {
    let mut mac =
        HmacSha256::new_from_slice(b"trio-dashboard-auth").expect("HMAC key length is valid");
    mac.update(bot_token.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn create_token(secret: &[u8]) -> String {
    let ts = chrono::Utc::now().timestamp();
    let ts_hex = format!("{ts:x}");
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC key length is valid");
    mac.update(ts_hex.as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());
    format!("{ts_hex}:{sig}")
}

fn verify_token(token: &str, secret: &[u8]) -> bool {
    let Some((ts_hex, sig)) = token.split_once(':') else {
        return false;
    };
    let Ok(ts) = i64::from_str_radix(ts_hex, 16) else {
        return false;
    };
    let now = chrono::Utc::now().timestamp();
    if now - ts > TOKEN_VALIDITY_SECS || ts > now + 60 {
        return false;
    }
    let Ok(mut mac) = HmacSha256::new_from_slice(secret) else {
        return false;
    };
    mac.update(ts_hex.as_bytes());
    // Constant-time comparison via hmac::Mac::verify_slice to prevent timing attacks.
    let Ok(sig_bytes) = hex::decode(sig) else {
        return false;
    };
    mac.verify_slice(&sig_bytes).is_ok()
}

fn check_auth(headers: &HeaderMap, secret: &[u8]) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| verify_token(t, secret))
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
}

// ---------- DB helpers ----------

#[allow(clippy::result_large_err)]
fn open_db(path: &std::path::Path) -> Result<Connection, Response> {
    let conn = Connection::open(path).map_err(|e| {
        error!("Dashboard: failed to open DB: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
    })?;
    conn.execute_batch("PRAGMA journal_mode=WAL;").ok();
    // Ensure tasks table exists.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tasks (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            title       TEXT    NOT NULL,
            description TEXT    NOT NULL DEFAULT '',
            status      TEXT    NOT NULL DEFAULT 'todo',
            priority    TEXT    NOT NULL DEFAULT 'medium',
            assigned_to TEXT,
            created_by  TEXT    NOT NULL DEFAULT 'Owner',
            created_at  TEXT    NOT NULL DEFAULT (datetime('now')),
            updated_at  TEXT    NOT NULL DEFAULT (datetime('now')),
            due_date    TEXT,
            position    INTEGER NOT NULL DEFAULT 0
        );",
    )
    .ok();
    Ok(conn)
}

// ---------- Request/Response types ----------

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct LoginResponse {
    token: String,
}

#[derive(Deserialize)]
struct MessagesQuery {
    /// Return messages with id > since (for polling).
    since: Option<i64>,
    limit: Option<i64>,
}

#[derive(Serialize)]
struct MessageResponse {
    id: i64,
    from_bot: String,
    to_bot: Option<String>,
    message: String,
    message_type: String,
    created_at: String,
}

#[derive(Deserialize)]
struct SendMessageRequest {
    message: String,
    to_bot: Option<String>,
}

#[derive(Serialize)]
struct SendMessageResponse {
    id: i64,
}

#[derive(Serialize)]
struct TaskResponse {
    id: i64,
    title: String,
    description: String,
    status: String,
    priority: String,
    assigned_to: Option<String>,
    created_by: String,
    created_at: String,
    updated_at: String,
    due_date: Option<String>,
    position: i64,
}

#[derive(Deserialize)]
struct CreateTaskRequest {
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default = "default_priority")]
    priority: String,
    assigned_to: Option<String>,
    due_date: Option<String>,
}

fn default_priority() -> String {
    "medium".to_string()
}

#[derive(Deserialize)]
struct UpdateTaskRequest {
    title: Option<String>,
    description: Option<String>,
    status: Option<String>,
    priority: Option<String>,
    assigned_to: Option<String>,
    due_date: Option<String>,
    position: Option<i64>,
}

#[derive(Serialize)]
struct AgentResponse {
    name: String,
    status: String,
    last_heartbeat: String,
    iteration_count: i64,
}

// ---------- Router ----------

pub fn router(state: Arc<DashboardState>) -> Router {
    Router::new()
        .route("/dashboard", get(serve_dashboard))
        .route("/api/auth/login", post(login))
        .route("/api/auth/check", get(check_auth_endpoint))
        .route("/api/messages", get(get_messages))
        .route("/api/messages", post(send_message))
        .route("/api/tasks", get(get_tasks))
        .route("/api/tasks", post(create_task))
        .route("/api/tasks/{id}", put(update_task))
        .route("/api/tasks/{id}", delete(delete_task))
        .route("/api/agents", get(get_agents))
        .with_state(state)
}

// ---------- Handlers ----------

async fn serve_dashboard() -> Html<&'static str> {
    Html(include_str!("../static/dashboard.html"))
}

async fn login(
    State(state): State<Arc<DashboardState>>,
    Json(payload): Json<LoginRequest>,
) -> Response {
    // Reject if credentials are not configured, or if they don't match.
    if state.auth_username.is_empty()
        || state.auth_password.is_empty()
        || payload.username != state.auth_username
        || payload.password != state.auth_password
    {
        warn!(
            "Dashboard: failed login attempt for user '{}'",
            payload.username
        );
        return (StatusCode::UNAUTHORIZED, "invalid credentials").into_response();
    }
    let secret = derive_secret(&state.bot_token);
    let token = create_token(&secret);
    info!("Dashboard: owner logged in");
    Json(LoginResponse { token }).into_response()
}

async fn check_auth_endpoint(
    State(state): State<Arc<DashboardState>>,
    headers: HeaderMap,
) -> Response {
    let secret = derive_secret(&state.bot_token);
    if !check_auth(&headers, &secret) {
        return unauthorized();
    }
    (StatusCode::OK, "ok").into_response()
}

async fn get_messages(
    State(state): State<Arc<DashboardState>>,
    headers: HeaderMap,
    Query(query): Query<MessagesQuery>,
) -> Response {
    let secret = derive_secret(&state.bot_token);
    if !check_auth(&headers, &secret) {
        return unauthorized();
    }

    let db_path = state.shared_db_path.clone();
    let since = query.since.unwrap_or(0);
    let limit = query.limit.unwrap_or(200);

    let result = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        let mut stmt = conn.prepare(
            "SELECT id, from_bot, to_bot, message, message_type, created_at
             FROM bot_messages
             WHERE id > ?1
             ORDER BY id DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![since, limit], |row| {
            Ok(MessageResponse {
                id: row.get(0)?,
                from_bot: row.get(1)?,
                to_bot: row.get(2)?,
                message: row.get(3)?,
                message_type: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        let mut messages = Vec::new();
        for row in rows {
            messages.push(row?);
        }
        messages.reverse(); // chronological order
        Ok::<_, anyhow::Error>(messages)
    })
    .await;

    match result {
        Ok(Ok(messages)) => Json(messages).into_response(),
        Ok(Err(e)) => {
            error!("Dashboard: get_messages error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
        Err(e) => {
            error!("Dashboard: spawn_blocking error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

async fn send_message(
    State(state): State<Arc<DashboardState>>,
    headers: HeaderMap,
    Json(payload): Json<SendMessageRequest>,
) -> Response {
    let secret = derive_secret(&state.bot_token);
    if !check_auth(&headers, &secret) {
        return unauthorized();
    }

    let db_path = state.shared_db_path.clone();
    let message = payload.message.clone();
    let to_bot = payload.to_bot.clone();

    // Insert into shared bus so bots see it
    let result = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        // Ensure table exists (in case dashboard starts before any bot)
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS bot_messages (
                id               INTEGER PRIMARY KEY AUTOINCREMENT,
                from_bot         TEXT    NOT NULL,
                to_bot           TEXT,
                message          TEXT    NOT NULL,
                message_type     TEXT    NOT NULL DEFAULT 'chat',
                reply_to_msg_id  INTEGER,
                telegram_msg_id  INTEGER,
                created_at       TEXT    NOT NULL DEFAULT (datetime('now')),
                read_by          TEXT    NOT NULL DEFAULT ''
            );",
        )?;
        conn.execute(
            "INSERT INTO bot_messages (from_bot, to_bot, message, message_type)
             VALUES (?1, ?2, ?3, 'chat')",
            params!["Owner", to_bot, message],
        )?;
        Ok::<_, anyhow::Error>(conn.last_insert_rowid())
    })
    .await;

    // Also send to Telegram group so it appears there
    let tg_token = state.telegram_bot_token.clone();
    let chat_id = state.group_chat_id;
    let msg_text = payload.message.clone();
    tokio::spawn(async move {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", tg_token);
        let client = reqwest::Client::new();
        let _ = client
            .post(&url)
            .json(&serde_json::json!({
                "chat_id": chat_id,
                "text": msg_text
            }))
            .send()
            .await;
    });

    match result {
        Ok(Ok(id)) => {
            info!("Dashboard: owner sent message id={id}");
            Json(SendMessageResponse { id }).into_response()
        }
        Ok(Err(e)) => {
            error!("Dashboard: send_message error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
        Err(e) => {
            error!("Dashboard: spawn_blocking error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

async fn get_tasks(State(state): State<Arc<DashboardState>>, headers: HeaderMap) -> Response {
    let secret = derive_secret(&state.bot_token);
    if !check_auth(&headers, &secret) {
        return unauthorized();
    }

    let db_path = state.shared_db_path.clone();
    let result = tokio::task::spawn_blocking(move || {
        let conn = open_db(&db_path).map_err(|_| anyhow::anyhow!("db open failed"))?;
        let mut stmt = conn.prepare(
            "SELECT id, title, description, status, priority, assigned_to,
                    created_by, created_at, updated_at, due_date, position
             FROM tasks
             ORDER BY
                CASE priority
                    WHEN 'urgent' THEN 0
                    WHEN 'high' THEN 1
                    WHEN 'medium' THEN 2
                    WHEN 'low' THEN 3
                    ELSE 4
                END,
                position ASC,
                created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(TaskResponse {
                id: row.get(0)?,
                title: row.get(1)?,
                description: row.get(2)?,
                status: row.get(3)?,
                priority: row.get(4)?,
                assigned_to: row.get(5)?,
                created_by: row.get(6)?,
                created_at: row.get(7)?,
                updated_at: row.get(8)?,
                due_date: row.get(9)?,
                position: row.get(10)?,
            })
        })?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok::<_, anyhow::Error>(tasks)
    })
    .await;

    match result {
        Ok(Ok(tasks)) => Json(tasks).into_response(),
        Ok(Err(e)) => {
            error!("Dashboard: get_tasks error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
        Err(e) => {
            error!("Dashboard: spawn_blocking error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

async fn create_task(
    State(state): State<Arc<DashboardState>>,
    headers: HeaderMap,
    Json(payload): Json<CreateTaskRequest>,
) -> Response {
    let secret = derive_secret(&state.bot_token);
    if !check_auth(&headers, &secret) {
        return unauthorized();
    }

    // Validate priority against allowlist.
    if !VALID_PRIORITIES.contains(&payload.priority.as_str()) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("invalid priority: must be one of {:?}", VALID_PRIORITIES),
        )
            .into_response();
    }

    let db_path = state.shared_db_path.clone();
    let result = tokio::task::spawn_blocking(move || {
        let conn = open_db(&db_path).map_err(|_| anyhow::anyhow!("db open failed"))?;
        conn.execute(
            "INSERT INTO tasks (title, description, priority, assigned_to, due_date)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                payload.title,
                payload.description,
                payload.priority,
                payload.assigned_to,
                payload.due_date
            ],
        )?;
        Ok::<_, anyhow::Error>(conn.last_insert_rowid())
    })
    .await;

    match result {
        Ok(Ok(id)) => {
            info!("Dashboard: task created id={id}");
            (StatusCode::CREATED, Json(serde_json::json!({"id": id}))).into_response()
        }
        Ok(Err(e)) => {
            error!("Dashboard: create_task error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
        Err(e) => {
            error!("Dashboard: spawn_blocking error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

async fn update_task(
    State(state): State<Arc<DashboardState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateTaskRequest>,
) -> Response {
    let secret = derive_secret(&state.bot_token);
    if !check_auth(&headers, &secret) {
        return unauthorized();
    }

    // Validate priority and status against allowlists when provided.
    if let Some(ref priority) = payload.priority
        && !VALID_PRIORITIES.contains(&priority.as_str())
    {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("invalid priority: must be one of {:?}", VALID_PRIORITIES),
        )
            .into_response();
    }
    if let Some(ref status) = payload.status
        && !VALID_STATUSES.contains(&status.as_str())
    {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("invalid status: must be one of {:?}", VALID_STATUSES),
        )
            .into_response();
    }

    let db_path = state.shared_db_path.clone();
    let result = tokio::task::spawn_blocking(move || {
        let conn = open_db(&db_path).map_err(|_| anyhow::anyhow!("db open failed"))?;

        // Build dynamic UPDATE query
        let mut sets = vec!["updated_at = datetime('now')".to_string()];
        let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(ref title) = payload.title {
            sets.push(format!("title = ?{}", values.len() + 1));
            values.push(Box::new(title.clone()));
        }
        if let Some(ref desc) = payload.description {
            sets.push(format!("description = ?{}", values.len() + 1));
            values.push(Box::new(desc.clone()));
        }
        if let Some(ref status) = payload.status {
            sets.push(format!("status = ?{}", values.len() + 1));
            values.push(Box::new(status.clone()));
        }
        if let Some(ref priority) = payload.priority {
            sets.push(format!("priority = ?{}", values.len() + 1));
            values.push(Box::new(priority.clone()));
        }
        if let Some(ref assigned) = payload.assigned_to {
            sets.push(format!("assigned_to = ?{}", values.len() + 1));
            values.push(Box::new(assigned.clone()));
        }
        if let Some(ref due) = payload.due_date {
            sets.push(format!("due_date = ?{}", values.len() + 1));
            values.push(Box::new(due.clone()));
        }
        if let Some(pos) = payload.position {
            sets.push(format!("position = ?{}", values.len() + 1));
            values.push(Box::new(pos));
        }

        let id_param = values.len() + 1;
        let sql = format!(
            "UPDATE tasks SET {} WHERE id = ?{}",
            sets.join(", "),
            id_param
        );
        values.push(Box::new(id));

        let params: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();
        conn.execute(&sql, params.as_slice())?;
        Ok::<_, anyhow::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            info!("Dashboard: task {id} updated");
            (StatusCode::OK, "ok").into_response()
        }
        Ok(Err(e)) => {
            error!("Dashboard: update_task error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
        Err(e) => {
            error!("Dashboard: spawn_blocking error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

async fn delete_task(
    State(state): State<Arc<DashboardState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    let secret = derive_secret(&state.bot_token);
    if !check_auth(&headers, &secret) {
        return unauthorized();
    }

    let db_path = state.shared_db_path.clone();
    let result = tokio::task::spawn_blocking(move || {
        let conn = open_db(&db_path).map_err(|_| anyhow::anyhow!("db open failed"))?;
        conn.execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
        Ok::<_, anyhow::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            info!("Dashboard: task {id} deleted");
            (StatusCode::OK, "ok").into_response()
        }
        Ok(Err(e)) => {
            error!("Dashboard: delete_task error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
        Err(e) => {
            error!("Dashboard: spawn_blocking error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

async fn get_agents(State(state): State<Arc<DashboardState>>, headers: HeaderMap) -> Response {
    let secret = derive_secret(&state.bot_token);
    if !check_auth(&headers, &secret) {
        return unauthorized();
    }

    let db_path = state.shared_db_path.clone();
    let result = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        // Ensure heartbeats table exists
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS heartbeats (
                bot_name         TEXT    PRIMARY KEY,
                last_heartbeat   TEXT    NOT NULL,
                iteration_count  INTEGER NOT NULL DEFAULT 0
            );",
        )?;
        let mut stmt = conn.prepare(
            "SELECT bot_name, last_heartbeat, iteration_count FROM heartbeats ORDER BY bot_name",
        )?;
        let rows = stmt.query_map([], |row| {
            let name: String = row.get(0)?;
            let last_hb: String = row.get(1)?;
            let iter_count: i64 = row.get(2)?;

            // Determine status: online if heartbeat within last 2 minutes
            let status = if let Ok(hb_time) =
                chrono::NaiveDateTime::parse_from_str(&last_hb, "%Y-%m-%d %H:%M:%S")
            {
                let now = chrono::Utc::now().naive_utc();
                let diff = now.signed_duration_since(hb_time);
                if diff.num_seconds() < 120 {
                    "online".to_string()
                } else {
                    "offline".to_string()
                }
            } else {
                "unknown".to_string()
            };

            Ok(AgentResponse {
                name,
                status,
                last_heartbeat: last_hb,
                iteration_count: iter_count,
            })
        })?;
        let mut agents = Vec::new();
        for row in rows {
            agents.push(row?);
        }

        // Always include all three bots even if no heartbeat yet
        let known = ["Atlas", "Nova", "Sentinel"];
        for bot in known {
            if !agents.iter().any(|a| a.name == bot) {
                agents.push(AgentResponse {
                    name: bot.to_string(),
                    status: "offline".to_string(),
                    last_heartbeat: String::new(),
                    iteration_count: 0,
                });
            }
        }
        agents.sort_by(|a, b| a.name.cmp(&b.name));

        Ok::<_, anyhow::Error>(agents)
    })
    .await;

    match result {
        Ok(Ok(agents)) => Json(agents).into_response(),
        Ok(Err(e)) => {
            error!("Dashboard: get_agents error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
        Err(e) => {
            error!("Dashboard: spawn_blocking error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}
