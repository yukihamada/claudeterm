use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json, Response},
    routing::{delete, get, post},
    Router,
};
mod templates;
mod billing;
mod router;
mod gemini;
mod imagen;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, sync::{Arc, Mutex as StdMutex}};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{watch, Mutex},
    time::Duration,
};

const HTML: &str = include_str!("../static/index.html");
const MANIFEST: &str = include_str!("../static/manifest.json");
const INITIAL_CREDITS: f64 = 1.0;
const COST_MULTIPLIER: f64 = 1.3; // 30% margin on API costs

type Db = Arc<StdMutex<Connection>>;

fn init_db(path: &str) -> Connection {
    let conn = Connection::open(path).expect("open db");
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS users (
            id TEXT PRIMARY KEY, email TEXT UNIQUE, token TEXT UNIQUE,
            credits REAL DEFAULT 10.0, api_key TEXT, created_at TEXT
        );
        CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY, user_id TEXT, name TEXT, created_at TEXT, project TEXT,
            claude_sid TEXT,
            FOREIGN KEY(user_id) REFERENCES users(id)
        );
        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT, role TEXT, content TEXT, timestamp TEXT
        );
    ").expect("init");
    // Add claude_sid column if missing (migration)
    conn.execute("ALTER TABLE sessions ADD COLUMN claude_sid TEXT", []).ok();
    conn
}

#[derive(Clone)]
struct AppState {
    command: String,
    workdir: String,
    db: Db,
    admin_token: Option<String>,
    stripe_key: Option<String>,
    resend_key: Option<String>,
    gemini_key: Option<String>,
    base_url: String,
    limiter: Arc<billing::RateLimiter>,
    otp_store: Arc<StdMutex<HashMap<String, (String, u64)>>>,
    // Per-user active process flag: uid -> bool (true = running)
    active_procs: Arc<StdMutex<HashMap<String, bool>>>,
}

#[derive(Deserialize)] struct TokenQ { token: Option<String> }
#[derive(Deserialize)] struct FileQ { token: Option<String>, path: Option<String> }
#[derive(Serialize)] struct UserDto { id: String, email: String, credits: f64, has_api_key: bool }
#[derive(Serialize, Clone)] struct SessionDto { id: String, name: String, created_at: String, project: String }
#[derive(Serialize)] struct FileEntry { name: String, is_dir: bool, size: u64 }
#[derive(Serialize)] struct ProjectEntry { name: String, path: String }

/// Build a macOS sandbox-exec profile: allow-default but restrict file writes
/// to the user's own sandbox directory. This is compatible with Node.js/claude CLI
/// which needs many mach/IPC services that are hard to enumerate with deny-default.
fn build_sandbox_profile(user_sandbox: &str) -> String {
    format!(r#"(version 1)
(allow default)

; Block writes everywhere except user's own sandbox and system temp dirs
(deny file-write* (subpath "/"))
(allow file-write* (subpath "{user_sandbox}"))
(allow file-write* (subpath "/tmp"))
(allow file-write* (subpath "/private/tmp"))
(allow file-write* (subpath "/dev/null"))
(allow file-write* (literal "/dev/null"))
"#, user_sandbox = user_sandbox)
}

fn get_user(db: &Connection, token: &str) -> Option<(String, String, f64, Option<String>)> {
    db.query_row("SELECT id, email, credits, api_key FROM users WHERE token=?1", [token], |r|
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_,Option<String>>(3)?))
    ).ok()
}

fn auth_user(state: &AppState, token: Option<&str>) -> Option<(String, String, f64, Option<String>)> {
    let t = token?;
    if t.is_empty() { return None; }
    let db = state.db.lock().unwrap_or_else(|e| e.into_inner());
    get_user(&db, t)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())
    ).init();
    let workdir = std::env::var("WORKDIR").unwrap_or_else(|_| "/tmp/claudeterm-sandbox".to_string());
    // Ensure sandbox exists
    std::fs::create_dir_all(&workdir).ok();
    // DB lives on the persistent volume (parent of workdir), not ephemeral $HOME
    let data_dir = std::path::Path::new(&workdir)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| workdir.clone());
    let db_path = std::env::var("DB_PATH")
        .unwrap_or_else(|_| format!("{}/claudeterm.db", data_dir));
    let state = Arc::new(AppState {
        admin_token: std::env::var("AUTH_TOKEN").ok(),
        command: std::env::var("CLAUDE_COMMAND").unwrap_or_else(|_| "claude".to_string()),
        workdir, db: Arc::new(StdMutex::new(init_db(&db_path))),
        stripe_key: std::env::var("STRIPE_SECRET_KEY").ok(),
        resend_key: std::env::var("RESEND_API_KEY").ok(),
        gemini_key: std::env::var("GEMINI_API_KEY").ok(),
        base_url: std::env::var("BASE_URL").unwrap_or_else(|_| "https://term.pasha.run".to_string()),
        limiter: Arc::new(billing::RateLimiter::new(20)),
        otp_store: Arc::new(StdMutex::new(HashMap::new())),
        active_procs: Arc::new(StdMutex::new(HashMap::new())),
    });
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let app = Router::new()
        .route("/", get(|_: Query<TokenQ>| async { Html(HTML) }))
        .route("/health", get(|| async { (StatusCode::OK, "ok") }))
        .route("/manifest.json", get(|| async {
            (StatusCode::OK, [("content-type","application/manifest+json")], MANIFEST)
        }))
        // Auth
        .route("/api/auth/login", post(login))
        .route("/api/auth/verify", post(verify_otp))
        .route("/api/auth/local-login", get(local_login))
        .route("/api/auth/me", get(me))
        .route("/api/auth/apikey", post(set_api_key))
        // Sessions
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/:id", delete(delete_session))
        .route("/api/sessions/:id/messages", get(get_messages))
        // Files
        .route("/api/files", get(list_files))
        .route("/api/files/read", get(read_file))
        .route("/api/projects", get(list_projects))
        .route("/api/templates", get(list_templates))
        // Billing
        .route("/api/billing/checkout", post(create_checkout))
        .route("/api/billing/webhook", post(stripe_webhook))
        .route("/billing/success", get(billing_success))
        // Image generation
        .route("/api/image", post(generate_image))
        // Admin alerts
        .route("/api/admin/alert", post(admin_alert))
        // WebSocket
        .route("/ws", get(ws_handler))
        .with_state(state);
    let addr = format!("0.0.0.0:{port}");
    tracing::info!("claudeterm v5 → http://{addr}");
    axum::serve(tokio::net::TcpListener::bind(&addr).await.unwrap(), app).await.unwrap();
}

// ── Auth ──

async fn login(State(s): State<Arc<AppState>>, Json(body): Json<serde_json::Value>) -> Response {
    let email = match body.get("email").and_then(|e| e.as_str()) {
        Some(e) if e.contains('@') => e.to_lowercase(),
        _ => return (StatusCode::BAD_REQUEST, "Invalid email").into_response(),
    };

    // Generate 6-digit OTP, store for 10 minutes
    let code: String = (0..6).map(|_| (b'0' + (rand::random::<u8>() % 10)) as char).collect();
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 600;
    s.otp_store.lock().unwrap_or_else(|e| e.into_inner())
        .insert(email.clone(), (code.clone(), expires));

    // Send via Resend if key is set, otherwise log for dev
    if let Some(ref key) = s.resend_key {
        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "from": "Claude Code <noreply@term.pasha.run>",
            "to": [&email],
            "subject": format!("Your login code: {}", code),
            "html": format!(
                "<div style='font-family:system-ui;max-width:400px;margin:40px auto;padding:32px;background:#09090b;border-radius:16px;border:1px solid #27272a'>\
                <div style='width:48px;height:48px;border-radius:12px;background:linear-gradient(135deg,#a78bfa,#60a5fa);margin-bottom:24px'></div>\
                <h2 style='color:#fafafa;font-size:22px;margin:0 0 8px'>Your login code</h2>\
                <p style='color:#a1a1aa;font-size:14px;margin:0 0 24px'>Enter this code to sign in to Claude Code</p>\
                <div style='font-size:36px;font-weight:700;letter-spacing:8px;color:#a78bfa;background:#18181b;padding:20px;border-radius:12px;text-align:center'>{}</div>\
                <p style='color:#52525b;font-size:12px;margin:20px 0 0'>Expires in 10 minutes. If you didn't request this, ignore this email.</p>\
                </div>", code)
        });
        let _ = client.post("https://api.resend.com/emails")
            .header("Authorization", format!("Bearer {key}"))
            .header("Content-Type", "application/json")
            .json(&body).send().await;
        tracing::info!("OTP sent to {email}");
    } else {
        // Dev mode: log the code
        tracing::info!("OTP for {email}: {code}");
    }

    Json(serde_json::json!({"sent": true})).into_response()
}

async fn verify_otp(State(s): State<Arc<AppState>>, Json(body): Json<serde_json::Value>) -> Response {
    let email = match body.get("email").and_then(|e| e.as_str()) {
        Some(e) if e.contains('@') => e.to_lowercase(),
        _ => return (StatusCode::BAD_REQUEST, "Invalid email").into_response(),
    };
    let code = match body.get("code").and_then(|c| c.as_str()) {
        Some(c) => c.to_string(),
        None => return (StatusCode::BAD_REQUEST, "Missing code").into_response(),
    };

    // Validate OTP
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let valid = {
        let mut store = s.otp_store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((stored_code, expires)) = store.get(&email) {
            if *expires > now && *stored_code == code {
                store.remove(&email);
                true
            } else { false }
        } else { false }
    };

    if !valid {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error":"Invalid or expired code"}))).into_response();
    }

    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    let existing: Option<(String, String)> = db.query_row(
        "SELECT id, token FROM users WHERE email=?1", [&email],
        |r| Ok((r.get(0)?, r.get(1)?))
    ).ok();

    let (uid, token) = if let Some((id, tok)) = existing {
        (id, tok)
    } else {
        let uid = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let token = uuid::Uuid::new_v4().to_string().replace("-", "");
        let now = chrono::Utc::now().to_rfc3339();
        db.execute("INSERT INTO users (id, email, token, credits, created_at) VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![uid, email, token, INITIAL_CREDITS, now]).unwrap();
        (uid, token)
    };

    let credits: f64 = db.query_row("SELECT credits FROM users WHERE id=?1", [&uid], |r| r.get(0)).unwrap_or(0.0);
    let has_key: bool = db.query_row("SELECT api_key FROM users WHERE id=?1", [&uid],
        |r| Ok(r.get::<_,Option<String>>(0)?.is_some())).unwrap_or(false);

    Json(serde_json::json!({
        "token": token, "user": { "id": uid, "email": email, "credits": credits, "has_api_key": has_key }
    })).into_response()
}

/// Local auth for NOU macOS app — only works if LOCAL_TOKEN env var is set.
/// The NOU app generates a random token and passes it as ?local=<token>.
/// Returns a permanent local user session (unlimited credits, no OTP needed).
#[derive(Deserialize)] struct LocalLoginQ { local: String }
async fn local_login(Query(q): Query<LocalLoginQ>, State(s): State<Arc<AppState>>) -> Response {
    let expected = match std::env::var("LOCAL_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    if q.local != expected {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let email = "local@localhost";
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    let existing: Option<(String, String)> = db.query_row(
        "SELECT id, token FROM users WHERE email=?1", [email],
        |r| Ok((r.get(0)?, r.get(1)?))).ok();
    let (uid, token) = if let Some(pair) = existing { pair } else {
        let uid = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let tok = uuid::Uuid::new_v4().to_string().replace("-", "");
        let now = chrono::Utc::now().to_rfc3339();
        // Local user gets effectively unlimited credits
        db.execute("INSERT INTO users (id,email,token,credits,created_at) VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![uid, email, tok, 999999.0_f64, now]).unwrap();
        (uid, tok)
    };
    Json(serde_json::json!({
        "token": token,
        "user": {"id": uid, "email": "Local (Mac)", "credits": 999999.0, "has_api_key": false}
    })).into_response()
}

async fn me(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    match auth_user(&s, q.token.as_deref()) {
        Some((id, email, credits, api_key)) => Json(UserDto {
            id, email, credits, has_api_key: api_key.is_some()
        }).into_response(),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

async fn set_api_key(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let key = body.get("api_key").and_then(|k| k.as_str()).unwrap_or("");
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    let key_val: Option<&str> = if key.is_empty() { None } else { Some(key) };
    db.execute("UPDATE users SET api_key=?1 WHERE id=?2", rusqlite::params![key_val, uid]).ok();
    Json(serde_json::json!({"ok": true})).into_response()
}

// ── Sessions ──

async fn list_sessions(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = db.prepare("SELECT id,name,created_at,project FROM sessions WHERE user_id=?1 ORDER BY created_at DESC").unwrap();
    let list: Vec<SessionDto> = st.query_map([&uid], |r| Ok(SessionDto {
        id:r.get(0)?,name:r.get(1)?,created_at:r.get(2)?,project:r.get(3)?
    })).unwrap().filter_map(|r|r.ok()).collect();
    Json(list).into_response()
}

async fn create_session(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    body: Option<Json<serde_json::Value>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let name = body.as_ref().and_then(|b|b.get("name")).and_then(|n|n.as_str()).unwrap_or("New session").to_string();
    let project = body.as_ref().and_then(|b|b.get("project")).and_then(|p|p.as_str()).unwrap_or("").to_string();
    let template_id = body.as_ref().and_then(|b|b.get("template")).and_then(|t|t.as_str()).unwrap_or("general");
    let now = chrono::Utc::now().to_rfc3339();
    s.db.lock().unwrap_or_else(|e| e.into_inner()).execute(
        "INSERT INTO sessions (id,user_id,name,created_at,project) VALUES (?1,?2,?3,?4,?5)",
        rusqlite::params![id,uid,name,now,project]).unwrap();

    // Write CLAUDE.md in user's sandbox
    if let Some(tmpl) = templates::all().iter().find(|t| t.id == template_id) {
        let user_dir = format!("{}/users/{}", s.workdir, uid);
        std::fs::create_dir_all(&user_dir).ok();
        std::fs::write(format!("{}/CLAUDE.md", user_dir), tmpl.claude_md).ok();
    }

    Json(SessionDto{id,name,created_at:now,project}).into_response()
}

async fn delete_session(Path(id): Path<String>, Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    db.execute("DELETE FROM messages WHERE session_id=?1 AND session_id IN (SELECT id FROM sessions WHERE user_id=?2)",
        rusqlite::params![id,uid]).ok();
    db.execute("DELETE FROM sessions WHERE id=?1 AND user_id=?2", rusqlite::params![id,uid]).ok();
    StatusCode::NO_CONTENT.into_response()
}

async fn get_messages(Path(id): Path<String>, Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    // Verify session belongs to user
    let owned: bool = db.query_row("SELECT 1 FROM sessions WHERE id=?1 AND user_id=?2",
        rusqlite::params![id,uid], |_| Ok(true)).unwrap_or(false);
    if !owned { return StatusCode::FORBIDDEN.into_response(); }
    let mut st = db.prepare("SELECT role,content,timestamp FROM messages WHERE session_id=?1 ORDER BY id").unwrap();
    let msgs: Vec<serde_json::Value> = st.query_map([&id], |r| Ok(serde_json::json!({
        "role":r.get::<_,String>(0)?,"content":r.get::<_,String>(1)?,"timestamp":r.get::<_,String>(2)?
    }))).unwrap().filter_map(|r|r.ok()).collect();
    Json(msgs).into_response()
}

// ── Files ──

async fn list_files(Query(q): Query<FileQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let base = PathBuf::from(format!("{}/users/{}", s.workdir, uid));
    std::fs::create_dir_all(&base).ok();
    let rel = q.path.unwrap_or_default();
    let dir = if rel.is_empty() { base.clone() } else { base.join(&rel) };
    if !dir.starts_with(&base) { return StatusCode::FORBIDDEN.into_response(); }
    let mut entries = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') { continue; }
            let meta = e.metadata().ok();
            entries.push(FileEntry { name, is_dir: meta.as_ref().map(|m|m.is_dir()).unwrap_or(false),
                size: meta.as_ref().map(|m|m.len()).unwrap_or(0) });
        }
    }
    entries.sort_by(|a,b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    Json(entries).into_response()
}

async fn read_file(Query(q): Query<FileQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let base = PathBuf::from(format!("{}/users/{}", s.workdir, uid));
    let rel = q.path.unwrap_or_default();
    let file = base.join(&rel);
    if !file.starts_with(&base) { return StatusCode::FORBIDDEN.into_response(); }
    match std::fs::read_to_string(&file) {
        Ok(c) => Json(serde_json::json!({"content":if c.len()>50000{&c[..50000]}else{&c},"path":rel})).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn list_projects(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let mut projects = Vec::new();
    let base = PathBuf::from(format!("{}/users/{}", s.workdir, uid));
    std::fs::create_dir_all(&base).ok();
    if let Ok(rd) = std::fs::read_dir(&base) {
        for e in rd.flatten() {
            if !e.metadata().map(|m|m.is_dir()).unwrap_or(false) { continue; }
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.')||name=="target"||name=="node_modules" { continue; }
            let p = e.path();
            if p.join("Cargo.toml").exists()||p.join("package.json").exists()||p.join("go.mod").exists()
                ||p.join("Makefile").exists()||p.join("CLAUDE.md").exists() {
                projects.push(ProjectEntry{name:name.clone(),path:name});
            }
        }
    }
    projects.sort_by(|a,b|a.name.cmp(&b.name));
    Json(projects).into_response()
}

// ── Billing ──

async fn create_checkout(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>) -> Response {
    let (_, email, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let stripe_key = match &s.stripe_key {
        Some(k) => k, None => return (StatusCode::SERVICE_UNAVAILABLE, "Payments not configured").into_response(),
    };
    let amount = body.get("amount").and_then(|a| a.as_f64()).unwrap_or(10.0);
    let token = q.token.unwrap_or_default();

    match billing::create_checkout_session(stripe_key, &email, &token, amount, &s.base_url).await {
        Ok(url) => Json(serde_json::json!({"url": url})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn stripe_webhook(State(s): State<Arc<AppState>>, body: String) -> Response {
    if let Some((user_token, credits)) = billing::parse_webhook_event(&body) {
        let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
        db.execute("UPDATE users SET credits = credits + ?1 WHERE token = ?2",
            rusqlite::params![credits, user_token]).ok();
        tracing::info!("Credits added: {} for token {}", credits, &user_token[..8]);
    }
    StatusCode::OK.into_response()
}

async fn billing_success(Query(q): Query<TokenQ>) -> Html<&'static str> {
    Html("<html><body style='background:#09090b;color:#fff;font-family:system-ui;display:flex;align-items:center;justify-content:center;height:100vh'><div style='text-align:center'><h1>Payment successful!</h1><p>Credits have been added to your account.</p><a href='/' style='color:#a78bfa'>Back to app</a></div></body></html>")
}

// ── Admin Alert (Telegram notification) ──

async fn admin_alert(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>) -> Response {
    // Any authenticated user can trigger an alert
    if auth_user(&s, q.token.as_deref()).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let alert_type = body.get("type").and_then(|t| t.as_str()).unwrap_or("unknown");
    let message = body.get("message").and_then(|m| m.as_str()).unwrap_or("");
    let user_email = body.get("user").and_then(|u| u.as_str()).unwrap_or("unknown");

    let text = format!(
        "🚨 *CodePal Alert*\nType: `{}`\nMessage: {}\nUser: {}\nTime: {}",
        alert_type, message, user_email, chrono::Utc::now().format("%Y-%m-%d %H:%M UTC")
    );

    // Send to Telegram
    let bot_token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
    let chat_id = std::env::var("TELEGRAM_CHAT_ID").unwrap_or_else(|_| "1136442501".to_string());

    if !bot_token.is_empty() {
        let client = reqwest::Client::new();
        let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);
        let _ = client.post(&url)
            .json(&serde_json::json!({
                "chat_id": chat_id,
                "text": text,
                "parse_mode": "Markdown"
            }))
            .send()
            .await;
        tracing::warn!("Admin alert sent: {} - {}", alert_type, message);
    } else {
        tracing::warn!("Admin alert (no Telegram): {} - {} - user: {}", alert_type, message, user_email);
    }

    StatusCode::OK.into_response()
}

async fn list_templates() -> Json<Vec<templates::Template>> {
    Json(templates::all())
}

// ── Image generation ──

#[derive(Deserialize)]
struct ImageReq { token: Option<String>, prompt: String }

async fn generate_image(State(s): State<Arc<AppState>>, Json(body): Json<ImageReq>) -> Response {
    if auth_user(&s, body.token.as_deref()).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let key = match s.gemini_key.as_deref() {
        Some(k) => k.to_string(),
        None => return (StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error":"Image generation not configured"}))).into_response(),
    };
    match imagen::generate(&key, &body.prompt).await {
        Ok((mime, data)) => Json(serde_json::json!({
            "url": format!("data:{};base64,{}", mime, data)
        })).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e}))).into_response(),
    }
}

// ── WebSocket ──

async fn ws_handler(ws: WebSocketUpgrade, Query(q): Query<TokenQ>, State(state): State<Arc<AppState>>) -> Response {
    let user = auth_user(&state, q.token.as_deref());
    if user.is_none() { return StatusCode::UNAUTHORIZED.into_response(); }
    let (uid, _email, credits, api_key) = user.unwrap();
    ws.on_upgrade(move |socket| handle_ws(socket, state, uid, credits, api_key))
}

async fn handle_ws(mut ws: WebSocket, state: Arc<AppState>, uid: String, mut credits: f64, api_key: Option<String>) {
    let (stop_tx, mut stop_rx) = watch::channel(false);

    loop {
        let msg = ws.recv().await;
        match msg {
            Some(Ok(Message::Text(text))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    if v.get("type").and_then(|t|t.as_str()) == Some("stop") {
                        let _ = stop_tx.send(true); continue;
                    }
                }

                // Rate limit
                if !state.limiter.check(&uid) {
                    let _ = ws.send(Message::Text(serde_json::json!({
                        "type":"error","text":"Rate limited. Please wait a moment."
                    }).to_string().into())).await;
                    continue;
                }

                // Check credits
                if credits <= 0.0 {
                    let _ = ws.send(Message::Text(serde_json::json!({
                        "type":"error","text":"No credits remaining. Please add your own API key in settings, or purchase more credits."
                    }).to_string().into())).await;
                    continue;
                }

                #[derive(Deserialize)]
                struct Cm { session: String, text: String, project: Option<String>, model: Option<String> }
                let cm: Cm = match serde_json::from_str(&text) { Ok(c)=>c, Err(_)=>continue };
                let _ = stop_tx.send(false);

                // Verify session belongs to user
                let (owned, claude_sid) = {
                    let db = state.db.lock().unwrap_or_else(|e| e.into_inner());
                    let r: Option<Option<String>> = db.query_row(
                        "SELECT claude_sid FROM sessions WHERE id=?1 AND user_id=?2",
                        rusqlite::params![cm.session, uid], |r| r.get(0)
                    ).ok();
                    match r { Some(sid) => (true, sid), None => (false, None) }
                };
                if !owned { continue; }

                // Save user message
                { let db=state.db.lock().unwrap_or_else(|e| e.into_inner()); let now=chrono::Utc::now().to_rfc3339();
                  db.execute("INSERT INTO messages (session_id,role,content,timestamp) VALUES (?1,'user',?2,?3)",
                    rusqlite::params![cm.session,cm.text,now]).ok(); }

                // Each user gets their own isolated sandbox
                let user_sandbox = format!("{}/users/{}", state.workdir, uid);
                std::fs::create_dir_all(&user_sandbox).ok();
                let workdir = if let Some(ref p) = cm.project {
                    let w = format!("{}/{}", user_sandbox, p);
                    if std::path::Path::new(&w).is_dir() { w } else { user_sandbox.clone() }
                } else { user_sandbox.clone() };

                // Block if user already has a running process (safety)
                let already_running = {
                    let mut ap = state.active_procs.lock().unwrap_or_else(|e| e.into_inner());
                    if *ap.get(&uid).unwrap_or(&false) { true }
                    else { ap.insert(uid.clone(), true); false }
                };
                if already_running {
                    let _ = ws.send(Message::Text(serde_json::json!({
                        "type":"error","text":"A request is already in progress. Please wait or stop it first."
                    }).to_string().into())).await;
                    continue;
                }

                // Model routing: manual selection or auto
                let use_gemini = cm.model.as_deref() == Some("gemini");
                let (model, effort) = if use_gemini {
                    (gemini::MODEL, "")
                } else {
                    match cm.model.as_deref() {
                        Some("haiku") => ("claude-haiku-4-5-20251001", "low"),
                        Some("sonnet") => ("claude-sonnet-4-6", "medium"),
                        Some("opus") => ("claude-sonnet-4-6", "max"),
                        _ => router::route_message(&cm.text),
                    }
                };

                // ★ Send model_info IMMEDIATELY — client shows thinking indicator at once
                let _ = ws.send(Message::Text(serde_json::json!({
                    "type":"model_info","model":model,"effort":effort
                }).to_string().into())).await;

                // ── Gemini path ───────────────────────────────────────────
                if use_gemini {
                    let gkey = match state.gemini_key.as_deref() {
                        Some(k) => k.to_string(),
                        None => {
                            let _ = ws.send(Message::Text(serde_json::json!({
                                "type":"error","text":"Gemini API key not configured on server."
                            }).to_string().into())).await;
                            state.active_procs.lock().unwrap_or_else(|e| e.into_inner()).remove(&uid);
                            continue;
                        }
                    };
                    // Load conversation history for this session
                    let history: Vec<(String, String)> = {
                        let db = state.db.lock().unwrap_or_else(|e| e.into_inner());
                        let mut stmt = db.prepare(
                            "SELECT role, content FROM messages WHERE session_id=?1 ORDER BY id ASC LIMIT 40"
                        ).unwrap();
                        stmt.query_map(rusqlite::params![cm.session], |r| Ok((r.get(0)?, r.get(1)?)))
                            .unwrap().filter_map(|r| r.ok()).collect()
                    };
                    let result = gemini::stream(&mut ws, &gkey, model, &history, &cm.text).await;
                    state.active_procs.lock().unwrap_or_else(|e| e.into_inner()).remove(&uid);
                    match result {
                        Ok(gr) => {
                            let charge = gr.cost_usd * COST_MULTIPLIER;
                            credits -= charge;
                            { let db = state.db.lock().unwrap_or_else(|e| e.into_inner());
                              db.execute("UPDATE users SET credits = credits - ?1 WHERE id = ?2",
                                rusqlite::params![charge, uid]).ok();
                              let now = chrono::Utc::now().to_rfc3339();
                              if !gr.text.is_empty() {
                                db.execute("INSERT INTO messages (session_id,role,content,timestamp) VALUES (?1,'assistant',?2,?3)",
                                    rusqlite::params![cm.session, gr.text, now]).ok();
                              }
                            }
                            let _ = ws.send(Message::Text(serde_json::json!({
                                "type":"done","credits":credits
                            }).to_string().into())).await;
                        }
                        Err(e) => {
                            let _ = ws.send(Message::Text(serde_json::json!({
                                "type":"error","text":format!("Gemini error: {e}")
                            }).to_string().into())).await;
                        }
                    }
                    continue;
                }
                // ── End Gemini path ───────────────────────────────────────

                // macOS: wrap in sandbox-exec; Linux: Docker container provides isolation
                #[cfg(target_os = "macos")]
                let mut cmd = {
                    let profile = build_sandbox_profile(&user_sandbox);
                    let mut c = Command::new("sandbox-exec");
                    c.arg("-p").arg(profile).arg(&state.command);
                    c
                };
                #[cfg(not(target_os = "macos"))]
                let mut cmd = Command::new(&state.command);

                cmd.arg("-p").arg("--output-format").arg("stream-json")
                    .arg("--verbose").arg("--dangerously-skip-permissions")
                    .arg("--model").arg(model)
                    .arg("--effort").arg(effort)
                    .arg(&cm.text);
                if let Some(ref sid) = claude_sid { cmd.arg("--resume").arg(sid); }
                cmd.current_dir(&workdir)
                    .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
                    // Pipe stdin to /dev/null so interactive prompts fail fast (never hang)
                    .stdin(std::process::Stdio::null())
                    .env("TERM","dumb").env("NO_COLOR","1")
                    // Reduce node/claude startup overhead
                    .env("CI","1").env("NODE_NO_WARNINGS","1")
                    .env("DISABLE_AUTOUPDATE","1").env("DO_NOT_TRACK","1")
                    // Git: fail immediately on auth prompt instead of hanging forever
                    .env("GIT_TERMINAL_PROMPT","0")
                    .env("GIT_ASKPASS","echo")
                    .env("GCM_INTERACTIVE","never");

                // Use user's API key if set, otherwise fall back to system
                if let Some(ref key) = api_key {
                    cmd.env("ANTHROPIC_API_KEY", key);
                }

                let mut child = match cmd.spawn() {
                    Ok(c)=>c, Err(e)=>{
                        tracing::error!("claude spawn failed uid={uid} err={e}");
                        let _ = ws.send(Message::Text(serde_json::json!({"type":"error","text":format!("Failed to start Claude: {e}")}).to_string().into())).await;
                        continue;
                    }
                };

                let stdout=child.stdout.take().unwrap();
                // Log stderr for debugging hung/errored claude processes
                if let Some(stderr) = child.stderr.take() {
                    let uid_c = uid.clone();
                    tokio::spawn(async move {
                        let mut sr = BufReader::new(stderr).lines();
                        while let Ok(Some(line)) = sr.next_line().await {
                            if !line.trim().is_empty() {
                                tracing::warn!("claude stderr uid={uid_c} | {line}");
                            }
                        }
                    });
                }

                let mut reader=BufReader::new(stdout).lines();
                let sid_clone=cm.session.clone();
                let db_ref=Arc::clone(&state.db);
                let mut stopped=false;
                let mut assistant_text=String::new();
                let mut cost_this_turn: f64 = 0.0;

                // 600-second idle timeout: kills only if claude produces NO output at all
                // (resets on every line received)
                let deadline = tokio::time::sleep(Duration::from_secs(600));
                tokio::pin!(deadline);

                // Ping the browser every 20s to keep the Fly.io proxy from closing the WS
                let mut ping_tick = tokio::time::interval(Duration::from_secs(20));
                ping_tick.tick().await; // skip first immediate tick

                loop {
                    tokio::select! {
                        biased;
                        _ = stop_rx.changed() => {
                            if *stop_rx.borrow() { let _ = child.kill().await; stopped=true; break; }
                        }
                        _ = &mut deadline => {
                            tracing::error!("claude timeout uid={uid} session={}", cm.session);
                            let _ = child.kill().await;
                            let _ = ws.send(Message::Text(serde_json::json!({
                                "type":"error","text":"Request timed out after 10 minutes. Please try again."
                            }).to_string().into())).await;
                            stopped=true; break;
                        }
                        _ = ping_tick.tick() => {
                            // WebSocket ping — keeps Fly.io proxy + browser connection alive
                            if ws.send(Message::Ping(vec![])).await.is_err() {
                                let _ = child.kill().await; stopped=true; break;
                            }
                        }
                        line = reader.next_line() => {
                            match line {
                                Ok(Some(line)) => {
                                    if line.trim().is_empty() { continue; }
                                    // Reset the timeout on each received line
                                    deadline.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(120));
                                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                                        // Capture claude session id
                                        if v.get("type").and_then(|t|t.as_str())==Some("system")
                                            && v.get("subtype").and_then(|t|t.as_str())==Some("init") {
                                            if let Some(s)=v.get("session_id").and_then(|s|s.as_str()) {
                                                db_ref.lock().unwrap_or_else(|e| e.into_inner()).execute(
                                                    "UPDATE sessions SET claude_sid=?1 WHERE id=?2",
                                                    rusqlite::params![s, sid_clone]).ok();
                                            }
                                        }
                                        // Collect assistant text
                                        if v.get("type").and_then(|t|t.as_str())==Some("assistant") {
                                            if let Some(ct)=v.get("message").and_then(|m|m.get("content")).and_then(|c|c.as_array()) {
                                                for p in ct { if p.get("type").and_then(|t|t.as_str())==Some("text") {
                                                    if let Some(t)=p.get("text").and_then(|t|t.as_str()) { assistant_text.push_str(t); }
                                                }}
                                            }
                                        }
                                        // Track cost
                                        if v.get("type").and_then(|t|t.as_str())==Some("result") {
                                            if let Some(c) = v.get("total_cost_usd").and_then(|c|c.as_f64()) {
                                                cost_this_turn = c;
                                            }
                                        }
                                    }
                                    if ws.send(Message::Text(line.into())).await.is_err() {
                                        let _ = child.kill().await;
                                        state.active_procs.lock().unwrap_or_else(|e| e.into_inner()).remove(&uid);
                                        stopped=true; break;
                                    }
                                }
                                Ok(None)|Err(_) => break,
                            }
                        }
                    }
                }
                // Release active process lock FIRST so user can retry immediately,
                // then wait for the child (child.wait can block briefly after kill)
                state.active_procs.lock().unwrap_or_else(|e| e.into_inner()).remove(&uid);
                // Wait with a timeout — if git push hangs kill() may take a moment
                tokio::select! {
                    _ = child.wait() => {}
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {
                        let _ = child.kill().await;
                    }
                }

                // Deduct credits with margin
                // BYOK users: charge platform fee (20% of cost for infra)
                // Platform key users: charge full marked-up cost
                if cost_this_turn > 0.0 {
                    let charge = if api_key.is_some() {
                        cost_this_turn * 0.2  // BYOK: 20% platform fee
                    } else {
                        cost_this_turn * COST_MULTIPLIER  // Platform key: 40% margin
                    };
                    credits -= charge;
                    let db = db_ref.lock().unwrap_or_else(|e| e.into_inner());
                    db.execute("UPDATE users SET credits = credits - ?1 WHERE id = ?2",
                        rusqlite::params![charge, uid]).ok();
                }

                // Save assistant response
                if !assistant_text.is_empty() {
                    let db=db_ref.lock().unwrap_or_else(|e| e.into_inner()); let now=chrono::Utc::now().to_rfc3339();
                    db.execute("INSERT INTO messages (session_id,role,content,timestamp) VALUES (?1,'assistant',?2,?3)",
                        rusqlite::params![sid_clone,assistant_text,now]).ok();
                }

                // Send done + updated credits
                let ev = if stopped {"stopped"} else {"done"};
                let _ = ws.send(Message::Text(serde_json::json!({
                    "type": ev, "credits": credits
                }).to_string().into())).await;
            }
            Some(Ok(Message::Close(_)))|None|Some(Err(_)) => break,
            Some(Ok(Message::Ping(data))) => {
                // Respond to client pings
                let _ = ws.send(Message::Pong(data)).await;
            }
            _ => {}
        }
    }
    // Always release lock on disconnect
    state.active_procs.lock().unwrap_or_else(|e| e.into_inner()).remove(&uid);
}
