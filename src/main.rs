mod git_versioning;

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use axum::{
    extract::{
        ws::{Message as WsRawMessage, WebSocket, WebSocketUpgrade},
        Path as AxumPath, Query, State, Multipart, DefaultBodyLimit,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response, Redirect},
    routing::{get, post, put, delete, get_service},
    Json, Router,
};
use axum_extra::extract::cookie::{Cookie, CookieJar};
use dashmap::DashMap;
use futures::{sink::SinkExt, stream::StreamExt};
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    io::AsyncWriteExt,
    sync::{broadcast, RwLock},
};
use tokio_stream::wrappers::BroadcastStream;
use tower_http::{cors::CorsLayer, services::ServeDir, trace::TraceLayer};
use tracing::{error, info, Level};
use uuid::Uuid;
use clap::Parser;
use reqwest::Client;
#[cfg(feature = "file-watcher")]
use notify::{Watcher, RecommendedWatcher, RecursiveMode, Event, EventKind, Config};
#[cfg(feature = "file-watcher")]
use std::collections::HashMap;
#[cfg(feature = "file-watcher")]
use std::time::{Duration, Instant};
use anyhow::Context;

#[derive(Clone)]
struct AppState {
    sessions: Arc<DashMap<String, String>>, // session_id -> username
    rooms: Arc<DashMap<String, Arc<Room>>>, // file_key -> room
    data_dir: Arc<PathBuf>,
    auth_token: String,
    require_token: bool,
    #[cfg(feature = "file-watcher")]
    writing_files: Arc<DashMap<String, Instant>>, // file_key -> timestamp when we started writing
    git_manager: Arc<git_versioning::GitVersionManager>,
}

struct Room {
    tx: broadcast::Sender<ServerWsMessage>,
    content: RwLock<String>,
    version: AtomicU64,
    members: DashMap<String, String>, // username -> color hex
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientWsMessage {
    Replace { version: u64, content: String },
    Ping,
    Cursor { x: f32, y: f32, basis: Option<String> }, // basis: "stage" or "overlay"
    Selection { ids: Vec<String> },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerWsMessage {
    Init { version: u64, content: String, your_id: String },
    Update { version: u64, content: String, username: String, sender_id: String },
    Error { message: String },
    Pong,
    PresenceSnapshot { users: Vec<PresenceUser> },
    PresenceJoin { username: String, color: String },
    PresenceLeave { username: String },
    Cursor { username: String, x: f32, y: f32, basis: Option<String>, sender_id: String },
    Selection { username: String, ids: Vec<String>, sender_id: String },
    AiStatus { username: String, job_id: String, phase: String, detail: String },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct PresenceUser {
    username: String,
    color: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct LoginResponse {
    username: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct SetNameRequest {
    username: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct FileWriteRequest {
    content: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct FileContentResponse {
    name: String,
    version: u64,
    content: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct FileListItem {
    name: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct FsEntry {
    name: String,
    path: String,
    is_dir: bool,
    size: Option<u64>,
    modified_ms: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RenameRequest {
    new_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct RenameBody {
    from: String,
    to: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct MkdirBody {
    path: String,
}

#[derive(Parser, Debug)]
#[command(name = "drawioserver")]
#[command(about = "Axum server for collaborative draw.io editing")]
struct Args {
    /// Require token for all file operations (like Jupyter). Also disables /login.
    #[arg(long, env = "DRAWIO_REQUIRE_TOKEN", default_value_t = true)]
    require_token: bool,

    /// Token value; if not provided, a random token is generated at startup
    #[arg(long, env = "DRAWIO_TOKEN")]
    token: Option<String>,

    /// Port to listen on
    #[arg(long, env = "PORT", default_value_t = 3000)]
    port: u16,

    /// Directory to store .drawio files
    #[arg(long, env = "DATA_DIR", default_value = "data")]
    data_dir: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let args = Args::parse();

    let data_dir = args.data_dir.clone();
    if !data_dir.exists() {
        fs::create_dir_all(&data_dir).await?;
    }

    let auth_token = args.token.unwrap_or_else(|| Uuid::new_v4().to_string());
    let require_token = args.require_token;

    // Initialize Git version manager
    let git_manager = Arc::new(
        git_versioning::GitVersionManager::new(data_dir.clone())
            .context("Failed to initialize Git version manager")?
    );

    let state = AppState {
        sessions: Arc::new(DashMap::new()),
        rooms: Arc::new(DashMap::new()),
        data_dir: Arc::new(data_dir.clone()),
        auth_token,
        require_token,
        #[cfg(feature = "file-watcher")]
        writing_files: Arc::new(DashMap::new()),
        git_manager,
    };
    
    // Start file watcher task
    #[cfg(feature = "file-watcher")]
    {
        let watcher_state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = watch_files(watcher_state, data_dir).await {
                error!("file watcher error: {e:?}");
            }
        });
    }

    let app = Router::new()
        .route("/", get(root_page))
        .route("/healthz", get(health))
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/auth/token", get(token_login))
    .route("/me", get(me).put(put_me))
        .route("/files", get(list_files))
        .route("/files/:name", get(get_file).put(put_file).delete(delete_file))
        .route("/files/:name/rename", post(rename_file))
        // New query-based API for folders and downloads
        .route("/api/list", get(api_list))
        .route("/api/file", get(api_get_file).put(api_put_file).delete(api_delete_file))
        .route("/api/rename", post(api_rename))
        .route("/api/mkdir", post(api_mkdir))
        .route("/api/download", get(api_download))
        .route("/api/upload", post(api_upload))
        .route("/api/ai_modify", post(api_ai_modify))
        .route("/api/versions", get(api_list_versions).post(api_restore_version))
        .route("/api/versions/checkpoint", post(api_checkpoint))
        .route("/api/versions/push", post(api_push_to_remote))
        .route("/api/versions/remote", post(api_set_remote))
        .route("/raw/:name", get(get_raw_file))
        .route("/ws/:name", get(ws_handler))
        .fallback_service(
            get_service(ServeDir::new("static")).handle_error(|_err| async move {
                (StatusCode::INTERNAL_SERVER_ERROR, "static file error")
            }),
        )
        .layer(DefaultBodyLimit::max(100 * 1024 * 1024)) // 100MB limit for file uploads
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::very_permissive(), // simplify testing; tighten for production
        )
        .with_state(state.clone());

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    info!("listening on http://{addr}");
    info!("require_token: {}", state.require_token);
    info!("token login: http://localhost:{}/auth/token?token={}", args.port, state.auth_token);
    info!("Set DRAWIO_TOKEN/--token to control the token. Current token: {}", state.auth_token);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn init_tracing() {
    let env_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info,tower_http=info,axum=info".to_string());
    let _ = tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_env_filter(env_filter)
        .with_target(false)
        .try_init();
}

async fn health() -> &'static str {
    "ok"
}

// ----- Auth helpers -----

async fn root_page(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if state.require_token {
        if get_authorized_user_from_header_or_query(&state, &headers, &q, &jar).is_none() {
            return Redirect::to("/token.html").into_response();
        }
    }
    // serve static index.html
    let bytes = match fs::read("static/index.html").await {
        Ok(b) => b,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut resp = Response::new(axum::body::Body::from(bytes));
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp
}

fn get_session_user(state: &AppState, jar: &CookieJar) -> Option<String> {
    let sid = jar.get("sid")?.value().to_string();
    state.sessions.get(&sid).map(|e| e.value().clone())
}

fn get_authorized_user_from_header_or_query(
    state: &AppState,
    headers: &HeaderMap,
    query: &std::collections::HashMap<String, String>,
    jar: &CookieJar,
) -> Option<String> {
    // Session cookie
    if let Some(u) = get_session_user(state, jar) {
        return Some(u);
    }
    // Header: Authorization: token <TOKEN>
    if let Some(auth) = headers.get(axum::http::header::AUTHORIZATION).and_then(|h| h.to_str().ok()) {
        let prefix = "token ";
        if let Some(rest) = auth.strip_prefix(prefix) {
            if rest.trim() == state.auth_token {
                return Some("token".to_string());
            }
        }
    }
    // Query param ?token=...
    if let Some(t) = query.get("token") {
        if t == &state.auth_token {
            return Some("token".to_string());
        }
    }
    None
}

fn set_session_user<'a>(state: &AppState, mut jar: CookieJar, username: &str) -> CookieJar {
    let sid = Uuid::new_v4().to_string();
    state.sessions.insert(sid.clone(), username.to_string());
    let mut cookie = Cookie::new("sid", sid);
    cookie.set_path("/");
    cookie.set_http_only(true);
    // cookie.set_secure(true); // enable behind HTTPS
    jar.add(cookie)
}

fn clear_session<'a>(state: &AppState, mut jar: CookieJar) -> CookieJar {
    if let Some(cookie) = jar.get("sid") {
        let sid = cookie.value().to_string();
        state.sessions.remove(&sid);
        let mut c = Cookie::from(cookie.clone());
        c.set_value("");
        c.set_max_age(time::Duration::seconds(0));
        c.set_path("/");
        return jar.remove(c);
    }
    jar
}

// ----- Routes: Auth -----

async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(req): Json<LoginRequest>,
) -> impl IntoResponse {
    // Password login removed. Always disabled.
    (StatusCode::FORBIDDEN, "password login disabled; use token and PUT /me to set name").into_response()
}

async fn logout(State(state): State<AppState>, jar: CookieJar) -> impl IntoResponse {
    let jar = clear_session(&state, jar);
    (jar, StatusCode::NO_CONTENT).into_response()
}

async fn me(State(state): State<AppState>, jar: CookieJar) -> impl IntoResponse {
    if let Some(username) = get_session_user(&state, &jar) {
        Json(LoginResponse { username }).into_response()
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

async fn put_me(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    Json(req): Json<SetNameRequest>,
) -> impl IntoResponse {
    // Require authorization via token or existing session
    if get_authorized_user_from_header_or_query(&state, &headers, &q, &jar).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let name = req.username.trim();
    if name.is_empty() || name.len() > 64 {
        return (StatusCode::BAD_REQUEST, "invalid name").into_response();
    }
    // If the caller already has a session, update it; otherwise create one
    if let Some(_existing) = get_session_user(&state, &jar) {
        // Update existing session by re-issuing cookie with new name
        let jar = set_session_user(&state, jar, name);
        (jar, Json(LoginResponse { username: name.to_string() })).into_response()
    } else {
        // No session cookie present (e.g., used Authorization header). Create one.
        let jar = set_session_user(&state, jar, name);
        (jar, Json(LoginResponse { username: name.to_string() })).into_response()
    }
}

// Token login like Jupyter's: /auth/token?token=...
async fn token_login(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if let Some(token) = params.get("token") {
        if token == &state.auth_token {
            let jar = set_session_user(&state, jar, "token");
            return (jar, Redirect::to("/")).into_response();
        }
    }
    StatusCode::UNAUTHORIZED.into_response()
}

// ----- File helpers -----

fn sanitize_name(name: &str) -> Option<String> {
    // Deny path traversal / directories
    if name.contains('/') || name.contains('\\') {
        return None;
    }
    // Only allow a conservative set of characters
    let valid = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' ));
    if !valid {
        return None;
    }
    Some(name.to_string())
}

fn sanitize_rel_path(path: &str) -> Option<String> {
    // allow forward slashes as separators; forbid backslashes
    if path.contains('\\') {
        return None;
    }
    let trimmed = path.trim_matches('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in trimmed.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        let valid = part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ' | '(' | ')' | '+' | ',' ));
        if !valid {
            return None;
        }
        parts.push(part);
    }
    Some(parts.join("/"))
}

fn to_file_key(raw: &str) -> Option<String> {
    sanitize_name(raw)
}

fn to_data_path(data_dir: &Path, name: &str) -> PathBuf {
    data_dir.join(name)
}

fn to_data_rel_path(data_dir: &Path, rel: &str) -> PathBuf {
    let normalized = rel.trim_matches('/');
    data_dir.join(normalized)
}

async fn ensure_room_loaded(state: &AppState, file_key: &str) -> anyhow::Result<Arc<Room>> {
    if let Some(room) = state.rooms.get(file_key) {
        return Ok(room.value().clone());
    }
    let path = to_data_path(&state.data_dir, file_key);
    let content = if path.exists() {
        fs::read_to_string(&path).await.unwrap_or_default()
    } else {
        // Create an empty file for new documents
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).await.ok();
        }
        let mut f = fs::File::create(&path).await?;
        f.write_all(b"").await?;
        "".to_string()
    };

    // Initialize room
    let (tx, _rx) = broadcast::channel::<ServerWsMessage>(64);
    let room = Arc::new(Room {
        tx,
        content: RwLock::new(content),
        version: AtomicU64::new(0),
        members: DashMap::new(),
    });
    let inserted = state.rooms.insert(file_key.to_string(), room.clone());
    if inserted.is_some() {
        // another task inserted concurrently; use that one
        Ok(inserted.unwrap())
    } else {
        Ok(room)
    }
}

// ----- Routes: Files -----

async fn list_files(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if state.require_token {
        if get_authorized_user_from_header_or_query(&state, &headers, &q, &jar).is_none() {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    let mut items = Vec::<FileListItem>::new();
    let mut rd = match fs::read_dir(&*state.data_dir).await {
        Ok(rd) => rd,
        Err(_) => return Json(items).into_response(),
    };
    while let Ok(Some(e)) = rd.next_entry().await {
        if let Ok(ft) = e.file_type().await {
            if ft.is_file() {
                if let Some(name) = e.file_name().to_str() {
                    if name.ends_with(".drawio") {
                        items.push(FileListItem {
                            name: name.to_string(),
                        });
                    }
                }
            }
        }
    }
    Json(items).into_response()
}

async fn get_file(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    AxumPath(name): AxumPath<String>,
) -> impl IntoResponse {
    if state.require_token {
        if get_authorized_user_from_header_or_query(&state, &headers, &q, &jar).is_none() {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    let decoded = percent_decode_str(&name).decode_utf8_lossy().to_string();
    let Some(file_key) = to_file_key(&decoded) else {
        return (StatusCode::BAD_REQUEST, "invalid file name").into_response();
    };
    match ensure_room_loaded(&state, &file_key).await {
        Ok(room) => {
            let content = room.content.read().await.clone();
            let version = room.version.load(Ordering::SeqCst);
            Json(FileContentResponse {
                name: file_key,
                version,
                content,
            })
            .into_response()
        }
        Err(err) => {
            error!("get_file error: {err:?}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn put_file(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    AxumPath(name): AxumPath<String>,
    Json(req): Json<FileWriteRequest>,
) -> impl IntoResponse {
    let Some(_username) = get_authorized_user_from_header_or_query(&state, &headers, &q, &jar) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };

    let decoded = percent_decode_str(&name).decode_utf8_lossy().to_string();
    let Some(file_key) = to_file_key(&decoded) else {
        return (StatusCode::BAD_REQUEST, "invalid file name").into_response();
    };
    match ensure_room_loaded(&state, &file_key).await {
        Ok(room) => {
            {
                let mut guard = room.content.write().await;
                *guard = req.content.clone();
                let _new_ver = room.version.fetch_add(1, Ordering::SeqCst) + 1;
            }
            let path = to_data_path(&state.data_dir, &file_key);
            mark_file_writing(&state, &file_key);
            if let Err(err) = fs::write(&path, req.content.as_bytes()).await {
                error!("write file error: {err:?}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            
            // Create Git commit only if commit=true query parameter is present
            if q.get("commit").map(|s| s == "true").unwrap_or(false) {
                let git_mgr = state.git_manager.clone();
                let file_key_clone = file_key.clone();
                let content_clone = req.content.clone();
                let username = get_authorized_user_from_header_or_query(&state, &headers, &q, &jar)
                    .unwrap_or_else(|| "unknown".to_string());
                let version = room.version.load(Ordering::SeqCst);
                let message = q.get("message").map(|s| s.clone());
                tokio::task::spawn(async move {
                    if let Err(e) = git_mgr.create_version(&file_key_clone, &content_clone, &username, version, message.as_deref(), true).await {
                        error!("Failed to create Git version for {}: {e:?}", file_key_clone);
                    }
                });
            }
            
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => {
            error!("put_file error: {err:?}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn delete_file(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    AxumPath(name): AxumPath<String>,
) -> impl IntoResponse {
    let Some(_username) = get_authorized_user_from_header_or_query(&state, &headers, &q, &jar) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let decoded = percent_decode_str(&name).decode_utf8_lossy().to_string();
    let Some(file_key) = to_file_key(&decoded) else {
        return (StatusCode::BAD_REQUEST, "invalid file name").into_response();
    };
    let path = to_data_path(&state.data_dir, &file_key);
    if let Err(err) = fs::remove_file(&path).await {
        error!("delete file error: {err:?}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    state.rooms.remove(&file_key);
    StatusCode::NO_CONTENT.into_response()
}

async fn rename_file(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    AxumPath(name): AxumPath<String>,
    Json(req): Json<RenameRequest>,
) -> impl IntoResponse {
    let Some(_username) = get_authorized_user_from_header_or_query(&state, &headers, &q, &jar) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let decoded_old = percent_decode_str(&name).decode_utf8_lossy().to_string();
    let Some(old_key) = to_file_key(&decoded_old) else {
        return (StatusCode::BAD_REQUEST, "invalid old file name").into_response();
    };
    let Some(new_key) = to_file_key(&req.new_name) else {
        return (StatusCode::BAD_REQUEST, "invalid new file name").into_response();
    };
    if !new_key.ends_with(".drawio") {
        return (StatusCode::BAD_REQUEST, "new name must end with .drawio").into_response();
    }
    let old_path = to_data_path(&state.data_dir, &old_key);
    let new_path = to_data_path(&state.data_dir, &new_key);
    if let Err(err) = fs::rename(&old_path, &new_path).await {
        error!("rename file error: {err:?}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if let Some((_k, room)) = state.rooms.remove(&old_key) {
        state.rooms.insert(new_key.clone(), room);
    }
    Json(serde_json::json!({ "name": new_key })).into_response()
}

// ----- Routes: WebSocket -----

async fn get_raw_file(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    AxumPath(name): AxumPath<String>,
) -> impl IntoResponse {
    if state.require_token {
        if get_authorized_user_from_header_or_query(&state, &headers, &q, &jar).is_none() {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    let decoded = percent_decode_str(&name).decode_utf8_lossy().to_string();
    let Some(file_key) = to_file_key(&decoded) else {
        return (StatusCode::BAD_REQUEST, "invalid file name").into_response();
    };
    match ensure_room_loaded(&state, &file_key).await {
        Ok(room) => {
            let content = room.content.read().await.clone();
            let mut resp = Response::new(axum::body::Body::from(content));
            let headers = resp.headers_mut();
            headers.insert(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("application/xml; charset=utf-8"));
            headers.insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
            resp
        }
        Err(err) => {
            error!("get_raw_file error: {err:?}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn ws_handler(
    State(state): State<AppState>,
    jar: CookieJar,
    AxumPath(name): AxumPath<String>,
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(username) = get_authorized_user_from_header_or_query(&state, &headers, &q, &jar) else {
        return (StatusCode::UNAUTHORIZED, "login required").into_response();
    };
    // Prefer path from query (?path=...) to support folders; fallback to :name
    let file_key = if let Some(p) = q.get("path") {
        if let Some(safe) = sanitize_rel_path(p) { safe } else { return (StatusCode::BAD_REQUEST, "invalid path").into_response() }
    } else {
        let decoded = percent_decode_str(&name).decode_utf8_lossy().to_string();
        let Some(file_key) = to_file_key(&decoded) else {
            return (StatusCode::BAD_REQUEST, "invalid file name").into_response();
        };
        file_key
    };
    let user_agent = headers
        .get("user-agent")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("-");
    info!("ws connect: user={username} file={file_key} ua={user_agent}");
    ws.on_upgrade(move |socket| handle_ws(socket, state, username, file_key))
}

fn color_for_username(username: &str) -> String {
    // Deterministic color from username: simple hash to HSL then convert to hex (approximate with fixed saturation/lightness)
    let mut hash = 0u32;
    for b in username.bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(b as u32);
    }
    let hue = (hash % 360) as f32;
    let (s, l) = (0.65f32, 0.55f32);
    hsl_to_hex(hue, s, l)
}

fn hsl_to_hex(h: f32, s: f32, l: f32) -> String {
    // Convert HSL to RGB then hex; simple implementation
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (((h / 60.0) % 2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r1, g1, b1) = if (0.0..60.0).contains(&h) {
        (c, x, 0.0)
    } else if (60.0..120.0).contains(&h) {
        (x, c, 0.0)
    } else if (120.0..180.0).contains(&h) {
        (0.0, c, x)
    } else if (180.0..240.0).contains(&h) {
        (0.0, x, c)
    } else if (240.0..300.0).contains(&h) {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let (r, g, b) = (
        ((r1 + m) * 255.0).round() as u8,
        ((g1 + m) * 255.0).round() as u8,
        ((b1 + m) * 255.0).round() as u8,
    );
    format!("#{:02x}{:02x}{:02x}", r, g, b)
}

async fn handle_ws(mut socket: WebSocket, state: AppState, username: String, file_key: String) {
    let Ok(room) = ensure_room_loaded(&state, &file_key).await else {
        let _ = socket
            .send(WsRawMessage::Text(
                serde_json::to_string(&ServerWsMessage::Error {
                    message: "failed to load file".to_string(),
                })
                .unwrap(),
            ))
            .await;
        let _ = socket.close().await;
        return;
    };

    // subscribe to room broadcast
    let mut rx = room.tx.subscribe();
    // unique id for this connection
    let conn_id = Uuid::new_v4().to_string();

    // send init snapshot
    {
        let content = room.content.read().await.clone();
        let version = room.version.load(std::sync::atomic::Ordering::SeqCst);
        let init_msg = ServerWsMessage::Init { version, content, your_id: conn_id.clone() };
        let _ = socket
            .send(WsRawMessage::Text(serde_json::to_string(&init_msg).unwrap()))
            .await;
    }

    // send presence snapshot and announce join
    {
        // record member with color
        let color = color_for_username(&username);
        room.members.insert(username.clone(), color.clone());
        let snapshot = room
            .members
            .iter()
            .filter(|e| e.key() != &username)
            .map(|e| PresenceUser {
                username: e.key().clone(),
                color: e.value().clone(),
            })
            .collect::<Vec<_>>();
        let _ = socket
            .send(WsRawMessage::Text(
                serde_json::to_string(&ServerWsMessage::PresenceSnapshot { users: snapshot }).unwrap(),
            ))
            .await;
        let _ = room.tx.send(ServerWsMessage::PresenceJoin {
            username: username.clone(),
            color,
        });
    }

    let (mut sender, mut receiver) = socket.split();

    // Merge room broadcast and incoming client messages into one loop to use a single sender
    let mut room_stream = BroadcastStream::new(rx);

    loop {
        tokio::select! {
            maybe_incoming = receiver.next() => {
                let Some(Ok(incoming)) = maybe_incoming else { break; };
                match incoming {
                    WsRawMessage::Text(txt) => {
                        match serde_json::from_str::<ClientWsMessage>(&txt) {
                            Ok(ClientWsMessage::Replace { version: _version, content }) => {
                                // naive versioning: accept any update, bump version, save to disk, broadcast
                                {
                                    let mut guard = room.content.write().await;
                                    *guard = content.clone();
                                    room.version.fetch_add(1, Ordering::SeqCst);
                                }
                                let new_version = room.version.load(Ordering::SeqCst);
                                // persist
                                let path = to_data_path(&state.data_dir, &file_key);
                                mark_file_writing(&state, &file_key);
                                if let Err(err) = fs::write(&path, content.as_bytes()).await {
                                    error!("ws write file error: {err:?}");
                                }
                                
                                // Note: Git commits are not created automatically from WebSocket updates.
                                // Use the /api/versions/checkpoint endpoint or save with ?commit=true to create commits.
                                
                                let _ = room.tx.send(ServerWsMessage::Update {
                                    version: new_version,
                                    content,
                                    username: username.clone(),
                                    sender_id: conn_id.clone(),
                                });
                            }
                            Ok(ClientWsMessage::Cursor { x, y, basis }) => {
                                let x = x.clamp(0.0, 1.0);
                                let y = y.clamp(0.0, 1.0);
                                let _ = room.tx.send(ServerWsMessage::Cursor {
                                    username: username.clone(),
                                    x, y,
                                    basis,
                                    sender_id: conn_id.clone(),
                                });
                            }
                            Ok(ClientWsMessage::Ping) => {
                                let _ = sender
                                    .send(WsRawMessage::Text(
                                        serde_json::to_string(&ServerWsMessage::Pong).unwrap(),
                                    ))
                                    .await;
                            }
                            Ok(ClientWsMessage::Selection { ids }) => {
                                let _ = room.tx.send(ServerWsMessage::Selection {
                                    username: username.clone(),
                                    ids,
                                    sender_id: conn_id.clone(),
                                });
                            }
                            Err(err) => {
                                let _ = sender
                                    .send(WsRawMessage::Text(
                                        serde_json::to_string(&ServerWsMessage::Error {
                                            message: format!("invalid message: {err}"),
                                        })
                                        .unwrap(),
                                    ))
                                    .await;
                            }
                        }
                    }
                    WsRawMessage::Close(_f) => {
                        break;
                    }
                    WsRawMessage::Ping(data) => {
                        let _ = sender.send(WsRawMessage::Pong(data)).await;
                    }
                    _ => {}
                }
            }
            maybe_room_msg = room_stream.next() => {
                match maybe_room_msg {
                    Some(Ok(msg)) => {
                        if sender
                            .send(WsRawMessage::Text(
                                serde_json::to_string(&msg).unwrap_or_else(|_| "{\"type\":\"error\",\"message\":\"encode\"}".to_string()),
                            ))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Some(Err(_e)) => {
                        // lagging receiver; skip
                    }
                    None => {
                        // broadcast channel closed
                        break;
                    }
                }
            }
        }
    }

    // on disconnect: announce leave
    room.members.remove(&username);
    let _ = room.tx.send(ServerWsMessage::PresenceLeave { username });
}

// ----- New Folder-capable API -----

async fn api_list(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if state.require_token {
        if get_authorized_user_from_header_or_query(&state, &headers, &q, &jar).is_none() {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    let rel = q.get("path").cloned().unwrap_or_else(|| "".to_string());
    let Some(safe) = sanitize_rel_path(&rel).or_else(|| if rel.is_empty() { Some("".to_string()) } else { None }) else {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    };
    let dir_path = to_data_rel_path(&state.data_dir, &safe);
    let mut out: Vec<FsEntry> = Vec::new();
    let mut rd = match fs::read_dir(&dir_path).await {
        Ok(rd) => rd,
        Err(_) => return Json(out).into_response(),
    };
    while let Ok(Some(ent)) = rd.next_entry().await {
        let name = ent.file_name().to_string_lossy().to_string();
        let meta = match ent.metadata().await {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mut size = None;
        let mut modified_ms = None;
        if meta.is_file() {
            size = Some(meta.len());
        }
        if let Ok(m) = meta.modified() {
            if let Ok(dur) = m.duration_since(std::time::UNIX_EPOCH) {
                modified_ms = Some(dur.as_millis() as i64);
            }
        }
        if meta.is_dir() {
            out.push(FsEntry {
                name: name.clone(),
                path: if safe.is_empty() { name.clone() } else { format!("{}/{}", safe, name) },
                is_dir: true,
                size: None,
                modified_ms,
            });
        } else if meta.is_file() {
            if name.ends_with(".drawio") {
                out.push(FsEntry {
                    name: name.clone(),
                    path: if safe.is_empty() { name.clone() } else { format!("{}/{}", safe, name) },
                    is_dir: false,
                    size,
                    modified_ms,
                });
            }
        } else {
            // symlink or other: try to resolve to target type
            if let Ok(ft) = ent.file_type().await {
                if ft.is_symlink() {
                    // attempt read_link -> metadata
                    if let Ok(target_meta) = tokio::fs::metadata(ent.path()).await {
                        if target_meta.is_dir() {
                            out.push(FsEntry {
                                name: name.clone(),
                                path: if safe.is_empty() { name.clone() } else { format!("{}/{}", safe, name) },
                                is_dir: true,
                                size: None,
                                modified_ms,
                            });
                        } else if target_meta.is_file() && name.ends_with(".drawio") {
                            out.push(FsEntry {
                                name: name.clone(),
                                path: if safe.is_empty() { name.clone() } else { format!("{}/{}", safe, name) },
                                is_dir: false,
                                size,
                                modified_ms,
                            });
                        }
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.is_dir.cmp(&b.is_dir).reverse().then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Json(out).into_response()
}

async fn api_get_file(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(_u) = get_authorized_user_from_header_or_query(&state, &headers, &q, &jar) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(path) = q.get("path") else { return (StatusCode::BAD_REQUEST, "missing path").into_response() };
    let Some(safe) = sanitize_rel_path(path) else { return (StatusCode::BAD_REQUEST, "invalid path").into_response() };
    match ensure_room_loaded(&state, &safe).await {
        Ok(room) => {
            let content = room.content.read().await.clone();
            let version = room.version.load(Ordering::SeqCst);
            Json(FileContentResponse { name: safe, version, content }).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn api_put_file(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    Json(req): Json<FileWriteRequest>,
) -> impl IntoResponse {
    let Some(_u) = get_authorized_user_from_header_or_query(&state, &headers, &q, &jar) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(path) = q.get("path") else { return (StatusCode::BAD_REQUEST, "missing path").into_response() };
    let Some(safe) = sanitize_rel_path(path) else { return (StatusCode::BAD_REQUEST, "invalid path").into_response() };
    let pb = to_data_rel_path(&state.data_dir, &safe);
    if let Some(parent) = pb.parent() {
        let _ = fs::create_dir_all(parent).await;
    }
    let room = ensure_room_loaded(&state, &safe).await.map_err(|_| ()).ok();
    let version = if let Some(ref r) = room {
        {
            let mut guard = r.content.write().await;
            *guard = req.content.clone();
            r.version.fetch_add(1, Ordering::SeqCst);
        }
        room.as_ref().unwrap().version.load(Ordering::SeqCst)
    } else {
        0
    };
    mark_file_writing(&state, &safe);
    if let Err(_) = fs::write(&pb, req.content.as_bytes()).await {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    
    // Create Git commit only if commit=true query parameter is present
    if q.get("commit").map(|s| s == "true").unwrap_or(false) {
        let git_mgr = state.git_manager.clone();
        let safe_clone = safe.clone();
        let content_clone = req.content.clone();
        let username = get_authorized_user_from_header_or_query(&state, &headers, &q, &jar)
            .unwrap_or_else(|| "unknown".to_string());
        let message = q.get("message").map(|s| s.clone());
        tokio::task::spawn(async move {
            if let Err(e) = git_mgr.create_version(&safe_clone, &content_clone, &username, version, message.as_deref(), true).await {
                error!("Failed to create Git version for {}: {e:?}", safe_clone);
            }
        });
    }
    
    StatusCode::NO_CONTENT.into_response()
}

async fn api_delete_file(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(_u) = get_authorized_user_from_header_or_query(&state, &headers, &q, &jar) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(path) = q.get("path") else { return (StatusCode::BAD_REQUEST, "missing path").into_response() };
    let Some(safe) = sanitize_rel_path(path) else { return (StatusCode::BAD_REQUEST, "invalid path").into_response() };
    let pb = to_data_rel_path(&state.data_dir, &safe);
    if let Err(_) = fs::remove_file(&pb).await {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    state.rooms.remove(&safe);
    StatusCode::NO_CONTENT.into_response()
}

async fn api_rename(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<RenameBody>,
) -> impl IntoResponse {
    let Some(_u) = get_authorized_user_from_header_or_query(&state, &headers, &q, &jar) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(from) = sanitize_rel_path(&body.from) else { return (StatusCode::BAD_REQUEST, "invalid from").into_response() };
    let Some(to) = sanitize_rel_path(&body.to) else { return (StatusCode::BAD_REQUEST, "invalid to").into_response() };
    let from_pb = to_data_rel_path(&state.data_dir, &from);
    let to_pb = to_data_rel_path(&state.data_dir, &to);
    // Only enforce .drawio extension when renaming files; allow directories without extension
    match tokio::fs::metadata(&from_pb).await {
        Ok(meta) => {
            if meta.is_file() && !to.ends_with(".drawio") {
                return (StatusCode::BAD_REQUEST, "new file name must end with .drawio").into_response();
            }
        }
        Err(_) => {
            // If we can't read metadata, fall back to requiring extension only when destination looks like a file
            if to.split('/').last().map(|s| !s.is_empty() && !s.ends_with(".drawio")).unwrap_or(false) {
                // We can't tell; proceed anyway to let fs::rename surface an error if needed
            }
        }
    }
    if let Some(parent) = to_pb.parent() {
        let _ = fs::create_dir_all(parent).await;
    }
    if let Err(_) = fs::rename(&from_pb, &to_pb).await {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if let Some((_k, room)) = state.rooms.remove(&from) {
        state.rooms.insert(to.clone(), room);
    }
    Json(serde_json::json!({ "path": to })).into_response()
}

async fn api_mkdir(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<MkdirBody>,
) -> impl IntoResponse {
    let Some(_u) = get_authorized_user_from_header_or_query(&state, &headers, &q, &jar) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(safe) = sanitize_rel_path(&body.path) else { return (StatusCode::BAD_REQUEST, "invalid path").into_response() };
    let pb = to_data_rel_path(&state.data_dir, &safe);
    if let Err(_) = fs::create_dir_all(&pb).await {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn api_download(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(_u) = get_authorized_user_from_header_or_query(&state, &headers, &q, &jar) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(path) = q.get("path") else { return (StatusCode::BAD_REQUEST, "missing path").into_response() };
    let Some(safe) = sanitize_rel_path(path) else { return (StatusCode::BAD_REQUEST, "invalid path").into_response() };
    let pb = to_data_rel_path(&state.data_dir, &safe);
    let Ok(bytes) = fs::read(&pb).await else { return StatusCode::NOT_FOUND.into_response() };
    let mut resp = Response::new(axum::body::Body::from(bytes));
    resp.headers_mut().insert(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("application/xml"));
    let filename = safe.split('/').last().unwrap_or("diagram.drawio");
    let cd = format!("attachment; filename=\"{}\"", filename);
    if let Ok(val) = axum::http::HeaderValue::from_str(&cd) {
        resp.headers_mut().insert(axum::http::header::CONTENT_DISPOSITION, val);
    }
    resp
}

async fn api_upload(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    // Auth check
    let Some(_u) = get_authorized_user_from_header_or_query(&state, &headers, &q, &jar) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };

    // Extract file from multipart form
    let mut file_data: Option<Vec<u8>> = None;
    let mut filename: Option<String> = None;
    let mut form_path: Option<String> = None;

    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let field_name = field.name().unwrap_or("").to_string();
                
                if field_name == "file" {
                    // Get filename from Content-Disposition header
                    if let Some(name) = field.file_name() {
                        filename = Some(name.to_string());
                    }
                    
                    // Read file data
                    match field.bytes().await {
                        Ok(bytes) => {
                            file_data = Some(bytes.to_vec());
                        }
                        Err(e) => {
                            error!("upload error reading file: {e:?}");
                            return (StatusCode::BAD_REQUEST, format!("error reading file: {e}")).into_response();
                        }
                    }
                } else if field_name == "path" {
                    // Optional path field in form (alternative to query param)
                    match field.text().await {
                        Ok(path_str) => {
                            form_path = Some(path_str);
                        }
                        Err(e) => {
                            error!("upload error reading path field: {e:?}");
                            // Continue, path is optional
                        }
                    }
                }
            }
            Ok(None) => {
                // No more fields
                break;
            }
            Err(e) => {
                error!("upload error parsing multipart field: {e:?}");
                return (StatusCode::BAD_REQUEST, format!("error parsing form: {e}")).into_response();
            }
        }
    }

    // Get file data
    let Some(data) = file_data else {
        return (StatusCode::BAD_REQUEST, "missing file field").into_response();
    };

    // Determine target path
    let target_path = if let Some(p) = form_path.or_else(|| q.get("path").cloned()) {
        // Use provided path
        let Some(safe) = sanitize_rel_path(&p) else {
            return (StatusCode::BAD_REQUEST, "invalid path").into_response();
        };
        safe
    } else if let Some(fname) = filename {
        // Use filename from upload
        if let Some(safe) = sanitize_rel_path(&fname) {
            safe
        } else {
            // If filename doesn't pass sanitize_rel_path, try sanitize_name
            match sanitize_name(&fname) {
                Some(safe_name) => safe_name,
                None => {
                    return (StatusCode::BAD_REQUEST, "invalid filename").into_response();
                }
            }
        }
    } else {
        // Generate a unique filename
        let uuid_name = format!("{}.drawio", Uuid::new_v4());
        uuid_name
    };

    // Ensure the target path ends with .drawio for files
    let final_path = if !target_path.ends_with(".drawio") && !target_path.contains('/') {
        // If it's a simple filename without extension, add .drawio
        format!("{}.drawio", target_path)
    } else if target_path.contains('/') {
        // For paths with directories, check if the last component needs .drawio
        let parts: Vec<&str> = target_path.split('/').collect();
        if let Some(last) = parts.last() {
            if !last.ends_with(".drawio") && !last.is_empty() {
                let mut new_parts: Vec<String> = parts[..parts.len() - 1].iter().map(|s| s.to_string()).collect();
                let new_last = format!("{}.drawio", last);
                new_parts.push(new_last);
                new_parts.join("/")
            } else {
                target_path
            }
        } else {
            target_path
        }
    } else {
        target_path
    };

    // Validate final path
    let Some(safe_path) = sanitize_rel_path(&final_path) else {
        return (StatusCode::BAD_REQUEST, "invalid final path").into_response();
    };

    // Convert file data to string (assuming it's XML/text)
    let content = match String::from_utf8(data) {
        Ok(s) => s,
        Err(e) => {
            error!("upload error: file is not valid UTF-8: {e:?}");
            return (StatusCode::BAD_REQUEST, "file must be valid UTF-8 text").into_response();
        }
    };

    // Save file
    let pb = to_data_rel_path(&state.data_dir, &safe_path);
    if let Some(parent) = pb.parent() {
        if let Err(e) = fs::create_dir_all(parent).await {
            error!("upload error creating directory: {e:?}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    // Update or create room
    let room = match ensure_room_loaded(&state, &safe_path).await {
        Ok(r) => r,
        Err(e) => {
            error!("upload error loading room: {e:?}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    {
        let mut guard = room.content.write().await;
        *guard = content.clone();
        room.version.fetch_add(1, Ordering::SeqCst);
    }

    mark_file_writing(&state, &safe_path);
    if let Err(e) = fs::write(&pb, content.as_bytes()).await {
        error!("upload error writing file: {e:?}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // Return success with path
    Json(serde_json::json!({ "path": safe_path })).into_response()
}


// ----- AI Modify -----

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AiProvider {
    Openrouter,
    Ollama,
}

#[derive(Debug, Serialize, Deserialize)]
struct AiModifyRequest {
    prompt: String,
    provider: AiProvider,
    model: String,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    system: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AiModifyResponse {
    version: u64,
    content: String,
}

fn extract_xml(input: &str) -> Option<String> {
    // Strip code fences if present (handles both ``` and ''')
    let mut s = input.trim();
    
    // Handle triple backticks (```) or triple single quotes (''')
    if s.starts_with("```") {
        // Find the first newline after the opening fence (may have language identifier like "xml")
        if let Some(start) = s.find('\n') {
            s = &s[start + 1..];
            // Find closing fence
            if let Some(end) = s.rfind("```") {
                s = &s[..end];
            }
        }
        s = s.trim();
    } else if s.starts_with("'''") {
        // Handle triple single quotes (''')
        if let Some(start) = s.find('\n') {
            s = &s[start + 1..];
            // Find closing fence
            if let Some(end) = s.rfind("'''") {
                s = &s[..end];
            }
        }
        s = s.trim();
    }
    
    // Find xml span - look for <?xml or <mxfile
    let start_pos = s.find("<?xml").or_else(|| s.find("<mxfile"))?;
    let end_tag = "</mxfile>";
    let end_pos = s[start_pos..].rfind(end_tag)?;
    let end_idx = start_pos + end_pos + end_tag.len();
    let out = &s[start_pos..end_idx];
    let xml = out.trim().to_string();
    
    // Basic XML validation: check for well-formed structure
    if !validate_xml_basic(&xml) {
        return None;
    }
    
    Some(xml)
}

fn validate_xml_basic(xml: &str) -> bool {
    // Check that it starts with <?xml or <mxfile
    if !xml.starts_with("<?xml") && !xml.starts_with("<mxfile") {
        return false;
    }
    // Check that it ends with </mxfile>
    if !xml.trim_end().ends_with("</mxfile>") {
        return false;
    }
    // Basic check: count opening and closing mxfile tags (must match)
    let open_count = xml.matches("<mxfile").count();
    let close_count = xml.matches("</mxfile>").count();
    if open_count != close_count || open_count == 0 {
        return false;
    }
    // Check for basic XML structure: must have at least one <diagram> or <mxGraphModel>
    if !xml.contains("<diagram") && !xml.contains("<mxGraphModel") {
        return false;
    }
    true
}

async fn call_openrouter(model: &str, sys: &str, user: &str, temperature: Option<f32>) -> anyhow::Result<String> {
    let key = std::env::var("OPENROUTER_API_KEY")
        .map_err(|_| anyhow::anyhow!("OPENROUTER_API_KEY not set"))?;
    let client = Client::new();
    let mut body = serde_json::json!({
        "model": model,
        "messages": [
            {"role":"system","content": sys},
            {"role":"user","content": user}
        ]
    });
    if let Some(t) = temperature {
        if let Some(map) = body.as_object_mut() {
            map.insert("temperature".to_string(), serde_json::json!(t));
        }
    }
    let resp = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(key)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let txt = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("openrouter error: {}", txt));
    }
    let json: serde_json::Value = resp.json().await?;
    let content = json
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("no content in openrouter response"))?;
    Ok(content.to_string())
}

async fn call_ollama(model: &str, sys: &str, user: &str, temperature: Option<f32>) -> anyhow::Result<String> {
    let url = std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://localhost:11434/api/chat".to_string());
    let client = Client::new();
    // Try chat endpoint
    let mut body = serde_json::json!({
        "model": model,
        "messages": [
            {"role":"system","content": sys},
            {"role":"user","content": user}
        ],
        "stream": false
    });
    if let Some(t) = temperature {
        if let Some(map) = body.as_object_mut() {
            map.insert("options".to_string(), serde_json::json!({ "temperature": t }));
        }
    }
    let resp = client.post(&url).json(&body).send().await?;
    if !resp.status().is_success() {
        let txt = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("ollama error: {}", txt));
    }
    let json: serde_json::Value = resp.json().await?;
    // Prefer chat message.content; fallback to response field (for generate)
    if let Some(s) = json.pointer("/message/content").and_then(|v| v.as_str()) {
        return Ok(s.to_string());
    }
    if let Some(s) = json.get("response").and_then(|v| v.as_str()) {
        return Ok(s.to_string());
    }
    Err(anyhow::anyhow!("no content in ollama response"))
}

async fn stream_openrouter(
    model: &str,
    sys: &str,
    user: &str,
    temperature: Option<f32>,
    tx: broadcast::Sender<ServerWsMessage>,
    job_id: &str,
) -> anyhow::Result<String> {
    let key = std::env::var("OPENROUTER_API_KEY")
        .map_err(|_| anyhow::anyhow!("OPENROUTER_API_KEY not set"))?;
    let client = Client::new();
    let mut body = serde_json::json!({
        "model": model,
        "messages": [
            {"role":"system","content": sys},
            {"role":"user","content": user}
        ],
        "stream": true
    });
    if let Some(t) = temperature {
        if let Some(map) = body.as_object_mut() {
            map.insert("temperature".to_string(), serde_json::json!(t));
        }
    }
    let resp = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(key)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let txt = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("openrouter error: {}", txt));
    }
    let mut acc = String::new();
    let mut buf = String::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        let s = String::from_utf8_lossy(&bytes);
        buf.push_str(&s);
        // SSE events separated by \n\n
        loop {
            if let Some(idx) = buf.find("\n\n") {
                let event = buf[..idx].to_string();
                buf.drain(..idx + 2);
                for line in event.lines() {
                    let line = line.trim_start();
                    if let Some(rest) = line.strip_prefix("data: ") {
                        if rest.trim() == "[DONE]" {
                            return Ok(acc);
                        }
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(rest) {
                            // OpenAI-style delta
                            if let Some(delta) = json
                                .pointer("/choices/0/delta/content")
                                .and_then(|v| v.as_str())
                            {
                                acc.push_str(delta);
                                let _ = tx.send(ServerWsMessage::AiStatus {
                                    username: "ai".to_string(),
                                    job_id: job_id.to_string(),
                                    phase: "stream".to_string(),
                                    detail: delta.to_string(),
                                });
                            } else if let Some(done_content) = json
                                .pointer("/choices/0/message/content")
                                .and_then(|v| v.as_str())
                            {
                                // Some providers send the full content at end
                                acc.push_str(done_content);
                                let _ = tx.send(ServerWsMessage::AiStatus {
                                    username: "ai".to_string(),
                                    job_id: job_id.to_string(),
                                    phase: "stream".to_string(),
                                    detail: done_content.to_string(),
                                });
                            }
                        }
                    }
                }
            } else {
                break;
            }
        }
    }
    Ok(acc)
}

async fn stream_ollama(
    model: &str,
    sys: &str,
    user: &str,
    temperature: Option<f32>,
    tx: broadcast::Sender<ServerWsMessage>,
    job_id: &str,
) -> anyhow::Result<String> {
    let url = std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://localhost:11434/api/chat".to_string());
    let client = Client::new();
    let mut body = serde_json::json!({
        "model": model,
        "messages": [
            {"role":"system","content": sys},
            {"role":"user","content": user}
        ],
        "stream": true
    });
    if let Some(t) = temperature {
        if let Some(map) = body.as_object_mut() {
            map.insert("options".to_string(), serde_json::json!({ "temperature": t }));
        }
    }
    let resp = client.post(&url).json(&body).send().await?;
    if !resp.status().is_success() {
        let txt = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("ollama error: {}", txt));
    }
    let mut acc = String::new();
    let mut buf = String::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        let s = String::from_utf8_lossy(&bytes);
        buf.push_str(&s);
        // Ollama streams JSON objects separated by newlines
        loop {
            if let Some(pos) = buf.find('\n') {
                let line = buf[..pos].to_string();
                buf.drain(..pos + 1);
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&line) {
                    if json.get("done").and_then(|v| v.as_bool()).unwrap_or(false) {
                        // final chunk may contain "message" with entire content too; we already accumulated
                        break;
                    }
                    if let Some(delta) = json
                        .pointer("/message/content")
                        .and_then(|v| v.as_str())
                        .or_else(|| json.get("response").and_then(|v| v.as_str()))
                    {
                        acc.push_str(delta);
                        let _ = tx.send(ServerWsMessage::AiStatus {
                            username: "ai".to_string(),
                            job_id: job_id.to_string(),
                            phase: "stream".to_string(),
                            detail: delta.to_string(),
                        });
                    }
                }
            } else {
                break;
            }
        }
    }
    Ok(acc)
}
async fn api_ai_modify(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    Json(req): Json<AiModifyRequest>,
) -> impl IntoResponse {
    // Auth check
    let Some(_u) = get_authorized_user_from_header_or_query(&state, &headers, &q, &jar) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    // Path required in query (?path=...)
    let Some(path) = q.get("path") else {
        return (StatusCode::BAD_REQUEST, "missing path").into_response();
    };
    let Some(safe) = sanitize_rel_path(path) else {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    };
    // Load current content via room
    let room = match ensure_room_loaded(&state, &safe).await {
        Ok(r) => r,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let current_xml = { room.content.read().await.clone() };
    let job_id = Uuid::new_v4().to_string();
    let _ = room.tx.send(ServerWsMessage::AiStatus {
        username: "ai".to_string(),
        job_id: job_id.clone(),
        phase: "started".to_string(),
        detail: "Preparing prompt".to_string(),
    });
    // Compose prompts
    let sys = req.system.clone().unwrap_or_else(|| {
        "You are a precise draw.io (.drawio) diagram editor. Receive the full .drawio XML and the user instruction, apply the changes, and return ONLY the full updated .drawio XML. Do not include explanations, markdown, or code fences.".to_string()
    });
    let user = format!(
        "Instruction:\n{}\n\nCurrent .drawio XML:\n{}",
        req.prompt, current_xml
    );
    // Call provider
    let _ = room.tx.send(ServerWsMessage::AiStatus {
        username: "ai".to_string(),
        job_id: job_id.clone(),
        phase: "calling_provider".to_string(),
        detail: format!("provider={:?}, model={}", req.provider, req.model),
    });
    let ai_result = match req.provider {
        AiProvider::Openrouter => stream_openrouter(&req.model, &sys, &user, req.temperature, room.tx.clone(), &job_id).await,
        AiProvider::Ollama => stream_ollama(&req.model, &sys, &user, req.temperature, room.tx.clone(), &job_id).await,
    };
    let raw = match ai_result {
        Ok(s) => {
            let _ = room.tx.send(ServerWsMessage::AiStatus {
                username: "ai".to_string(),
                job_id: job_id.clone(),
                phase: "received".to_string(),
                detail: format!("received {} chars", s.len()),
            });
            s
        }
        Err(err) => {
            let _ = room.tx.send(ServerWsMessage::AiStatus {
                username: "ai".to_string(),
                job_id: job_id.clone(),
                phase: "error".to_string(),
                detail: format!("provider error: {err}"),
            });
            return (StatusCode::BAD_GATEWAY, format!("ai error: {err}")).into_response();
        }
    };
    let _ = room.tx.send(ServerWsMessage::AiStatus {
        username: "ai".to_string(),
        job_id: job_id.clone(),
        phase: "parsing".to_string(),
        detail: "Extracting .drawio XML from response".to_string(),
    });
    let Some(new_xml) = extract_xml(&raw) else {
        // Show first 500 chars of raw response for debugging
        let preview = if raw.len() > 500 {
            format!("{}...", &raw[..500])
        } else {
            raw.clone()
        };
        let error_detail = format!("AI did not return valid .drawio XML. Received: {}", preview);
        error!("extract_xml failed. Raw length: {}, preview: {}", raw.len(), preview);
        let _ = room.tx.send(ServerWsMessage::AiStatus {
            username: "ai".to_string(),
            job_id: job_id.clone(),
            phase: "error".to_string(),
            detail: error_detail.clone(),
        });
        return (StatusCode::BAD_GATEWAY, error_detail).into_response();
    };
    info!("extract_xml succeeded. Extracted XML length: {}, starts with: {}", new_xml.len(), &new_xml[..new_xml.len().min(100)]);
    // Update room, persist, broadcast
    {
        let mut guard = room.content.write().await;
        let old_len = guard.len();
        *guard = new_xml.clone();
        room.version.fetch_add(1, Ordering::SeqCst);
        info!("Room updated: old length {}, new length {}, version {}", old_len, new_xml.len(), room.version.load(Ordering::SeqCst));
    }
    let _ = room.tx.send(ServerWsMessage::AiStatus {
        username: "ai".to_string(),
        job_id: job_id.clone(),
        phase: "updating".to_string(),
        detail: "Persisting file and broadcasting update".to_string(),
    });
    let version = room.version.load(Ordering::SeqCst);
    let pb = to_data_rel_path(&state.data_dir, &safe);
    if let Some(parent) = pb.parent() {
        let _ = fs::create_dir_all(parent).await;
    }
    mark_file_writing(&state, &safe);
    if let Err(e) = fs::write(&pb, new_xml.as_bytes()).await {
        error!("ai_modify write error: {e:?}");
        let _ = room.tx.send(ServerWsMessage::AiStatus {
            username: "ai".to_string(),
            job_id: job_id.clone(),
            phase: "error".to_string(),
            detail: "Failed to write updated file".to_string(),
        });
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    info!("Broadcasting AI update: version {}, content length {}", version, new_xml.len());
    let _ = room.tx.send(ServerWsMessage::Update {
        version,
        content: new_xml.clone(),
        username: "ai".to_string(),
        sender_id: "ai".to_string(),
    });
    let _ = room.tx.send(ServerWsMessage::AiStatus {
        username: "ai".to_string(),
        job_id: job_id.clone(),
        phase: "done".to_string(),
        detail: "AI modification applied".to_string(),
    });
    Json(AiModifyResponse { version, content: new_xml }).into_response()
}

// ----- File Watcher Helpers -----

fn mark_file_writing(state: &AppState, file_key: &str) {
    #[cfg(feature = "file-watcher")]
    {
        state.writing_files.insert(file_key.to_string(), Instant::now());
        // Clean up old entries (older than 5 seconds) in background
        let state_clone = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let cutoff = Instant::now() - Duration::from_secs(5);
            state_clone.writing_files.retain(|_, &mut time| time > cutoff);
        });
    }
    #[cfg(not(feature = "file-watcher"))]
    {
        let _ = (state, file_key); // Suppress unused variable warnings
    }
}

// ----- File Watcher -----

#[cfg(feature = "file-watcher")]
async fn watch_files(state: AppState, data_dir: PathBuf) -> anyhow::Result<()> {
    use std::sync::mpsc;
    use tokio::sync::mpsc as tokio_mpsc;
    
    // Convert to absolute path for proper path comparison
    let data_dir_abs = data_dir.canonicalize().unwrap_or_else(|_| {
        // If canonicalize fails (e.g., path doesn't exist yet), use current_dir + relative path
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(&data_dir)
            .canonicalize()
            .unwrap_or(data_dir.clone())
    });
    
    let (notify_tx, notify_rx) = mpsc::channel();
    let mut watcher: RecommendedWatcher = Watcher::new(notify_tx, Config::default())?;
    watcher.watch(&data_dir_abs, RecursiveMode::Recursive)?;
    info!("File watcher started for {} (recursive mode - watching all subdirectories)", data_dir_abs.display());
    
    // Verify the watcher is set up correctly by checking if we can watch
    // Note: On some platforms, recursive watching might have limitations
    // If files in new subdirectories aren't detected, we might need to re-watch
    
    // Bridge from blocking channel to async channel
    let (async_tx, mut async_rx) = tokio_mpsc::unbounded_channel();
    
    // Spawn blocking task to bridge from blocking notify channel to async channel
    tokio::task::spawn_blocking(move || {
        loop {
            match notify_rx.recv() {
                Ok(event_result) => {
                    info!("File watcher bridge: received event from notify, sending to async channel");
                    // Send to async channel - this is safe because unbounded_channel::send is non-blocking
                    if async_tx.send(event_result).is_err() {
                        error!("File watcher bridge: failed to send to async channel (receiver dropped)");
                        break;
                    } else {
                        info!("File watcher bridge: successfully sent event to async channel");
                    }
                }
                Err(e) => {
                    error!("File watcher bridge: notify channel error: {:?}", e);
                    break;
                }
            }
        }
    });
    
    // Debounce map: file_key -> (last_event_time, task_handle)
    let mut debounce_map: HashMap<String, (Instant, tokio::task::JoinHandle<()>)> = HashMap::new();
    let debounce_duration = Duration::from_millis(500); // 500ms debounce
    
    info!("File watcher: entering main event loop, waiting for events...");
    loop {
        info!("File watcher: waiting for next event from async channel...");
        match async_rx.recv().await {
            Some(res) => {
                info!("File watcher: received event from async channel");
        match res {
            Ok(Event { kind, paths, .. }) => {
                info!("File watcher: parsed event - kind={:?}, paths={:?}", kind, paths);
                match kind {
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Any => {
                        info!("File watcher: event kind matches, processing {} paths", paths.len());
                        for path in paths {
                            let path_str = path.to_string_lossy();
                            info!("File watcher: checking path {}", path_str);
                            
                            // Check if it's a .drawio file
                            if !path_str.ends_with(".drawio") {
                                info!("File watcher: skipping non-.drawio file: {}", path_str);
                                continue;
                            }
                            info!("File watcher: processing .drawio file: {}", path_str);
                            
                            // Check if file exists (might not exist yet for Create events)
                            if !path.exists() {
                                info!("File watcher: path {} does not exist yet, skipping", path_str);
                                continue;
                            }
                            
                            // Check if it's actually a file (not a directory)
                            if !path.is_file() {
                                info!("File watcher: path {} is not a file, skipping", path_str);
                                continue;
                            }
                            
                                                    // Get relative path from data_dir (use absolute path for comparison)
                            let Ok(rel_path) = path.strip_prefix(&data_dir_abs) else {
                                info!("File watcher: path {} is not under data_dir {}, skipping", path_str, data_dir_abs.display());
                                continue;
                            };
                            // Normalize path separators (Windows uses \, Unix uses /)
                            let mut file_key = rel_path.to_string_lossy().replace('\\', "/");
                            // Remove leading slash if present
                            file_key = file_key.trim_start_matches('/').to_string();
                            info!("File watcher: detected change in file_key='{}', data_dir={}, rel_path={:?}", 
                                  file_key, data_dir_abs.display(), rel_path);
                            
                            // Debug: list all current rooms (only log first few to avoid spam)
                            let room_keys: Vec<String> = state.rooms.iter().map(|r| r.key().clone()).take(10).collect();
                            if !room_keys.is_empty() {
                                info!("File watcher: sample of current rooms (showing first 10): {:?}", room_keys);
                            } else {
                                info!("File watcher: no active rooms currently");
                            }
                            
                            // Check if we're currently writing this file (ignore our own writes)
                            if let Some(write_time) = state.writing_files.get(&file_key) {
                                let elapsed = write_time.elapsed();
                                if elapsed < Duration::from_secs(2) {
                                    // We wrote this file recently, ignore
                                    info!("File watcher: ignoring change to {} (we wrote it {}ms ago)", file_key, elapsed.as_millis());
                                    continue;
                                }
                            }
                            
                            // Cancel previous debounce task for this file if it exists
                            if let Some((_, handle)) = debounce_map.remove(&file_key) {
                                info!("File watcher: canceling previous debounce for {}", file_key);
                                handle.abort();
                            }
                            
                            // Create new debounce task
                            let state_clone = state.clone();
                            let file_key_clone = file_key.clone();
                            let path_clone = path.clone();
                            info!("File watcher: scheduling reload of {} after {}ms", file_key_clone, debounce_duration.as_millis());
                            let handle = tokio::spawn(async move {
                                tokio::time::sleep(debounce_duration).await;
                                info!("File watcher: debounce complete, reloading {}", file_key_clone);
                                if let Err(e) = reload_file_from_disk(&state_clone, &file_key_clone, &path_clone).await {
                                    error!("Failed to reload file {}: {e:?}", file_key_clone);
                                }
                            });
                            
                            debounce_map.insert(file_key.clone(), (Instant::now(), handle));
                        }
                    }
                    _ => {
                        info!("File watcher: ignoring event kind {:?}", kind);
                    }
                }
            }
            Err(e) => {
                error!("File watcher: error parsing event: {e:?}");
            }
        }
            }
            None => {
                error!("File watcher: async channel closed (sender dropped)");
                break;
            }
        }
    }
    error!("File watcher: main event loop exited");
    Ok(())
}

#[cfg(feature = "file-watcher")]
async fn reload_file_from_disk(state: &AppState, file_key: &str, path: &Path) -> anyhow::Result<()> {
    info!("reload_file_from_disk: reading file_key={}, path={}", file_key, path.display());
    
    // Read file from disk
    let new_content = match fs::read_to_string(path).await {
        Ok(c) => {
            info!("reload_file_from_disk: read {} bytes from {}", c.len(), path.display());
            c
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // File was deleted, remove room
            info!("reload_file_from_disk: file {} was deleted, removing room", file_key);
            state.rooms.remove(file_key);
            return Ok(());
        }
        Err(e) => {
            error!("reload_file_from_disk: read error for {}: {e}", path.display());
            return Err(anyhow::anyhow!("read error: {e}"));
        }
    };
    
    // Get or create room - try exact match first
    let room = if let Some(existing) = state.rooms.get(file_key) {
        info!("reload_file_from_disk: room exists for {}", file_key);
        existing.value().clone()
    } else {
        // Room doesn't exist yet - try to find by matching file paths
        // Some editors might have opened the file with a different key format
        info!("reload_file_from_disk: room not found for key '{}', searching by path", file_key);
        let mut found_room = None;
        let mut found_key = None;
        for entry in state.rooms.iter() {
            let room_key = entry.key();
            // Check if this room's file path matches the changed file
            let room_path = to_data_rel_path(&state.data_dir, room_key);
            if room_path == path {
                info!("reload_file_from_disk: found matching room with key '{}' for path {}", room_key, path.display());
                found_room = Some(entry.value().clone());
                found_key = Some(room_key.clone());
                break;
            }
        }
        
        if let Some(room) = found_room {
            // Update the room key mapping if it was different
            if let Some(old_key) = found_key {
                if old_key != file_key {
                    info!("reload_file_from_disk: updating room key from '{}' to '{}'", old_key, file_key);
                    state.rooms.remove(&old_key);
                    state.rooms.insert(file_key.to_string(), room.clone());
                }
            }
            room
        } else {
            // Room doesn't exist yet, create it
            info!("reload_file_from_disk: creating new room for {}", file_key);
            let (tx, _rx) = broadcast::channel::<ServerWsMessage>(64);
            let room = Arc::new(Room {
                tx,
                content: RwLock::new(new_content.clone()),
                version: AtomicU64::new(0),
                members: DashMap::new(),
            });
            state.rooms.insert(file_key.to_string(), room.clone());
            room
        }
    };
    
    // Check if content actually changed
    let current_content = room.content.read().await.clone();
    if current_content == new_content {
        // No change, skip
        info!("reload_file_from_disk: content unchanged for {}, skipping broadcast", file_key);
        return Ok(());
    }
    
    info!("reload_file_from_disk: content changed for {} (old: {} bytes, new: {} bytes)", 
          file_key, current_content.len(), new_content.len());
    
    // Update room content
    {
        let mut guard = room.content.write().await;
        *guard = new_content.clone();
        room.version.fetch_add(1, Ordering::SeqCst);
    }
    
    let version = room.version.load(Ordering::SeqCst);
    info!("Reloaded file {} from disk (external change), version {}, broadcasting update", file_key, version);
    
    // Broadcast update to all connected clients
    let update_msg = ServerWsMessage::Update {
        version,
        content: new_content,
        username: "external".to_string(),
        sender_id: "file_watcher".to_string(),
    };
    let send_result = room.tx.send(update_msg);
    match send_result {
        Ok(_) => info!("reload_file_from_disk: successfully broadcast update for {}", file_key),
        Err(e) => error!("reload_file_from_disk: failed to broadcast update for {}: {e}", file_key),
    }
    
    Ok(())
}

// ----- Git Version Control API -----

#[derive(Debug, Serialize, Deserialize)]
struct RestoreVersionRequest {
    commit_oid: String,
    message: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CheckpointRequest {
    message: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SetRemoteRequest {
    remote_name: String,
    url: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct PushRequest {
    remote_name: Option<String>,
    branch: Option<String>,
}

async fn api_list_versions(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(_u) = get_authorized_user_from_header_or_query(&state, &headers, &q, &jar) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    
    let Some(path) = q.get("path") else {
        return (StatusCode::BAD_REQUEST, "missing path").into_response();
    };
    
    let Some(safe) = sanitize_rel_path(path) else {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    };
    
    match state.git_manager.list_versions(&safe).await {
        Ok(versions) => Json(serde_json::json!({
            "file": safe,
            "versions": versions
        })).into_response(),
        Err(e) => {
            error!("Failed to list versions: {e:?}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to list versions: {e}")).into_response()
        }
    }
}

async fn api_restore_version(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    Json(req): Json<RestoreVersionRequest>,
) -> impl IntoResponse {
    let Some(username) = get_authorized_user_from_header_or_query(&state, &headers, &q, &jar) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    
    let Some(path) = q.get("path") else {
        return (StatusCode::BAD_REQUEST, "missing path").into_response();
    };
    
    let Some(safe) = sanitize_rel_path(path) else {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    };
    
    // Get current version from room
    let version = if let Ok(room) = ensure_room_loaded(&state, &safe).await {
        room.version.load(Ordering::SeqCst) + 1
    } else {
        1
    };
    
    match state.git_manager.restore_version(&safe, &req.commit_oid, &username, version).await {
        Ok(version_info) => {
            // Reload room with restored content
            if let Ok(room) = ensure_room_loaded(&state, &safe).await {
                let content = state.git_manager.get_version_content(&safe, &req.commit_oid).await
                    .unwrap_or_default();
                {
                    let mut guard = room.content.write().await;
                    *guard = content.clone();
                    room.version.store(version, Ordering::SeqCst);
                }
                
                // Write to disk
                let pb = to_data_rel_path(&state.data_dir, &safe);
                if let Err(e) = fs::write(&pb, content.as_bytes()).await {
                    error!("Failed to write restored file: {e:?}");
                }
            }
            
            Json(version_info).into_response()
        }
        Err(e) => {
            error!("Failed to restore version: {e:?}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to restore version: {e}")).into_response()
        }
    }
}

async fn api_checkpoint(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    Json(req): Json<CheckpointRequest>,
) -> impl IntoResponse {
    let Some(username) = get_authorized_user_from_header_or_query(&state, &headers, &q, &jar) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    
    let Some(path) = q.get("path") else {
        return (StatusCode::BAD_REQUEST, "missing path").into_response();
    };
    
    let Some(safe) = sanitize_rel_path(path) else {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    };
    
    // Get current content from room
    let (content, version) = if let Ok(room) = ensure_room_loaded(&state, &safe).await {
        let content = room.content.read().await.clone();
        let version = room.version.load(Ordering::SeqCst);
        (content, version)
    } else {
        return (StatusCode::NOT_FOUND, "File not found").into_response();
    };
    
    // Force commit (checkpoint)
    match state.git_manager.create_version(&safe, &content, &username, version, req.message.as_deref(), true).await {
        Ok(Some(version_info)) => Json(version_info).into_response(),
        Ok(None) => (StatusCode::INTERNAL_SERVER_ERROR, "Failed to create checkpoint").into_response(),
        Err(e) => {
            error!("Failed to create checkpoint: {e:?}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to create checkpoint: {e}")).into_response()
        }
    }
}

async fn api_set_remote(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(_q): Query<std::collections::HashMap<String, String>>,
    Json(req): Json<SetRemoteRequest>,
) -> impl IntoResponse {
    let Some(_u) = get_authorized_user_from_header_or_query(&state, &headers, &_q, &jar) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    
    match state.git_manager.set_remote(&req.remote_name, &req.url).await {
        Ok(_) => Json(serde_json::json!({
            "remote_name": req.remote_name,
            "url": req.url,
            "status": "configured"
        })).into_response(),
        Err(e) => {
            error!("Failed to set remote: {e:?}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to set remote: {e}")).into_response()
        }
    }
}

async fn api_push_to_remote(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(_q): Query<std::collections::HashMap<String, String>>,
    Json(req): Json<PushRequest>,
) -> impl IntoResponse {
    let Some(_u) = get_authorized_user_from_header_or_query(&state, &headers, &_q, &jar) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    
    let remote_name = req.remote_name.unwrap_or_else(|| "origin".to_string());
    let branch = req.branch.unwrap_or_else(|| "main".to_string());
    
    match state.git_manager.push_to_remote(&remote_name, &branch).await {
        Ok(_) => Json(serde_json::json!({
            "remote": remote_name,
            "branch": branch,
            "status": "pushed"
        })).into_response(),
        Err(e) => {
            error!("Failed to push to remote: {e:?}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to push: {e}")).into_response()
        }
    }
}

