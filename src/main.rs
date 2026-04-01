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
mod storage;
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
const INITIAL_CREDITS: f64 = 3.0;
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
        CREATE TABLE IF NOT EXISTS otps (
            email TEXT PRIMARY KEY, code TEXT NOT NULL, expires_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS feedback (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_email TEXT, category TEXT, message TEXT,
            created_at TEXT, status TEXT DEFAULT 'pending'
        );
    ").expect("init");
    // Migrations
    conn.execute("ALTER TABLE sessions ADD COLUMN claude_sid TEXT", []).ok();
    conn.execute("ALTER TABLE users ADD COLUMN plan TEXT DEFAULT 'free'", []).ok();
    conn.execute("ALTER TABLE users ADD COLUMN stripe_customer_id TEXT", []).ok();
    conn.execute("ALTER TABLE sessions ADD COLUMN share_id TEXT", []).ok();
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS gallery (
            id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL,
            author TEXT NOT NULL,
            project TEXT NOT NULL,
            title TEXT NOT NULL,
            description TEXT DEFAULT '',
            tags TEXT DEFAULT '',
            likes INTEGER DEFAULT 0,
            created_at TEXT,
            FOREIGN KEY(user_id) REFERENCES users(id)
        );
    ").ok();
    conn.execute("ALTER TABLE users ADD COLUMN referral_code TEXT", []).ok();
    conn.execute("ALTER TABLE users ADD COLUMN referred_by TEXT", []).ok();
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS referrals (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            inviter_id TEXT NOT NULL,
            invitee_id TEXT NOT NULL,
            bonus REAL DEFAULT 3.0,
            created_at TEXT,
            UNIQUE(invitee_id)
        );
    ").ok();
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS cron_jobs (
            id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL,
            name TEXT NOT NULL,
            command TEXT NOT NULL,
            project TEXT DEFAULT '',
            interval_secs INTEGER NOT NULL,
            enabled INTEGER DEFAULT 1,
            last_run INTEGER DEFAULT 0,
            next_run INTEGER DEFAULT 0,
            last_result TEXT DEFAULT '',
            last_status TEXT DEFAULT 'pending',
            created_at TEXT,
            FOREIGN KEY(user_id) REFERENCES users(id)
        );
    ").ok();
    conn
}

#[derive(Clone)]
struct AppState {
    command: String,
    workdir: String,
    db: Db,
    storage: storage::Storage,
    admin_token: Option<String>,
    stripe_key: Option<String>,
    resend_key: Option<String>,
    gemini_key: Option<String>,
    base_url: String,
    limiter: Arc<billing::RateLimiter>,
    active_procs: Arc<StdMutex<HashMap<String, bool>>>,
    preview_ports: Arc<StdMutex<HashMap<String, u16>>>, // uid -> port for live preview
}

#[derive(Deserialize)] struct TokenQ { token: Option<String> }
#[derive(Deserialize)] struct FileQ { token: Option<String>, path: Option<String> }
#[derive(Serialize)] struct UserDto { id: String, email: String, credits: f64, has_api_key: bool, plan: String }
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

fn get_user(db: &Connection, token: &str) -> Option<(String, String, f64, Option<String>, String)> {
    db.query_row(
        "SELECT id, email, credits, api_key, COALESCE(plan,'free') FROM users WHERE token=?1",
        [token], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_,Option<String>>(3)?, r.get::<_,String>(4)?))
    ).ok()
}

/// Extract port number from text like "localhost:3000" or "port 5173"
fn extract_port(text: &str) -> Option<u16> {
    // Match patterns: localhost:NNNN, 127.0.0.1:NNNN, 0.0.0.0:NNNN, port NNNN
    for pattern in &["localhost:", "127.0.0.1:", "0.0.0.0:"] {
        if let Some(idx) = text.find(pattern) {
            let after = &text[idx + pattern.len()..];
            let num: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(p) = num.parse::<u16>() {
                if (1024..=65535).contains(&p) { return Some(p); }
            }
        }
    }
    // "port NNNN" pattern
    if let Some(idx) = text.to_lowercase().find("port ") {
        let after = &text[idx + 5..];
        let num: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(p) = num.parse::<u16>() {
            if (1024..=65535).contains(&p) { return Some(p); }
        }
    }
    None
}

fn auth_user(state: &AppState, token: Option<&str>) -> Option<(String, String, f64, Option<String>, String)> {
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
    let store = storage::Storage::from_env(&workdir);
    let state = Arc::new(AppState {
        admin_token: std::env::var("AUTH_TOKEN").ok(),
        command: std::env::var("CLAUDE_COMMAND").unwrap_or_else(|_| "claude".to_string()),
        storage: store,
        workdir: workdir.clone(), db: Arc::new(StdMutex::new(init_db(&db_path))),
        stripe_key: std::env::var("STRIPE_SECRET_KEY").ok(),
        resend_key: std::env::var("RESEND_API_KEY").ok(),
        gemini_key: std::env::var("GEMINI_API_KEY").ok(),
        base_url: std::env::var("BASE_URL").unwrap_or_else(|_| "https://chatweb.ai".to_string()),
        limiter: Arc::new(billing::RateLimiter::new(20)),
        active_procs: Arc::new(StdMutex::new(HashMap::new())),
        preview_ports: Arc::new(StdMutex::new(HashMap::new())),
    });
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let app = Router::new()
        .route("/", get(|_: Query<TokenQ>| async { Html(HTML) }))
        .route("/health", get(|| async { (StatusCode::OK, "ok") }))
        .route("/manifest.json", get(|| async {
            (StatusCode::OK, [("content-type","application/manifest+json")], MANIFEST)
        }))
        .route("/og.png", get(|| async {
            // SVG served as og image (Twitter/OGP accept SVG via content-type)
            let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="1200" height="630" viewBox="0 0 1200 630">
<defs><linearGradient id="bg" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#09090b"/><stop offset="1" stop-color="#1a1040"/></linearGradient>
<linearGradient id="ac" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#a78bfa"/><stop offset="1" stop-color="#60a5fa"/></linearGradient></defs>
<rect width="1200" height="630" fill="url(#bg)"/>
<rect x="80" y="80" width="80" height="80" rx="22" fill="url(#ac)"/>
<text x="108" y="147" font-size="56" font-family="system-ui" fill="white" font-weight="bold">C</text>
<text x="190" y="142" font-size="60" font-family="system-ui" fill="white" font-weight="700" letter-spacing="-2">ChatWeb</text>
<text x="80" y="260" font-size="36" font-family="system-ui" fill="#a1a1aa">AI Development Terminal</text>
<text x="80" y="320" font-size="28" font-family="system-ui" fill="#52525b">Claude Code in your browser — code, build, and deploy</text>
<text x="80" y="380" font-size="28" font-family="system-ui" fill="#52525b">with AI assistance. Free to start.</text>
<rect x="80" y="450" width="200" height="52" rx="12" fill="url(#ac)"/>
<text x="130" y="484" font-size="24" font-family="system-ui" fill="black" font-weight="600">Try Free</text>
<text x="80" y="570" font-size="22" font-family="system-ui" fill="#3f3f46">chatweb.ai</text>
</svg>"##;
            (StatusCode::OK, [("content-type","image/svg+xml"),("cache-control","public, max-age=86400")], svg)
        }))
        // Auth
        .route("/api/auth/login", post(login))
        .route("/api/auth/verify", post(verify_otp))
        .route("/api/auth/google", get(google_oauth_start))
        .route("/auth/google/callback", get(google_oauth_callback))
        .route("/api/auth/google/callback", get(google_oauth_callback))
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
        .route("/api/projects", get(list_projects).post(create_project))
        .route("/api/projects/clone", post(clone_project))
        .route("/api/projects/merge", post(merge_projects))
        .route("/api/templates", get(list_templates))
        // Billing
        .route("/api/billing/checkout", post(create_checkout))
        .route("/api/billing/subscribe", post(create_subscription))
        .route("/api/billing/webhook", post(stripe_webhook))
        .route("/billing/webhook", post(stripe_webhook))
        .route("/billing/success", get(billing_success))
        .route("/billing/cancel", get(billing_cancel))
        // Admin
        .route("/api/admin/credit", post(admin_credit))
        // Image generation
        .route("/api/image", post(generate_image))
        // Admin alerts
        .route("/api/admin/alert", post(admin_alert))
        // Feedback
        .route("/api/feedback", post(submit_feedback))
        .route("/api/feedback", get(list_feedback))
        // Share
        .route("/api/sessions/:id/share", post(create_share))
        .route("/api/share/:share_id", get(get_shared))
        .route("/api/share/:share_id/fork", post(fork_shared))
        .route("/api/share/:share_id/join", post(join_shared))
        .route("/s/:share_id", get(view_shared))
        // Referral
        .route("/api/referral/code", get(get_referral_code))
        .route("/api/referral/apply", post(apply_referral))
        .route("/r/:code", get(referral_redirect))
        // Preview
        .route("/api/preview/port", get(get_preview_port).post(set_preview_port))
        // Files write
        .route("/api/files/write", post(write_file))
        // GitHub
        .route("/api/github/status", get(github_status))
        // Templates
        .route("/api/projects/from-template", post(create_from_template))
        // Community
        .route("/api/community/publish", post(publish_project))
        .route("/api/community/gallery", get(gallery))
        // Cron
        .route("/api/cron", get(list_crons).post(create_cron))
        .route("/api/cron/:id", delete(delete_cron))
        .route("/api/cron/:id/toggle", post(toggle_cron))
        // Public app preview
        .route("/app/:uid/:project/*path", get(serve_user_app))
        .route("/app/:uid/:project/", get(serve_user_app_index))
        .route("/app/:uid/:project", get(serve_user_app_index))
        // WebSocket
        .route("/ws", get(ws_handler))
        .with_state(state.clone());

    // ── Background cron scheduler ──
    let cron_state = state.clone();
    tokio::spawn(async move { cron_scheduler(cron_state).await });

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

    // Generate 6-digit OTP, store for 10 minutes in DB (survives restarts)
    let code: String = (0..6).map(|_| (b'0' + (rand::random::<u8>() % 10)) as char).collect();
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 600;
    { let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
      db.execute("INSERT OR REPLACE INTO otps (email, code, expires_at) VALUES (?1,?2,?3)",
        rusqlite::params![email, code, expires as i64]).ok(); }

    // Send via Resend if key is set, otherwise log for dev
    if let Some(ref key) = s.resend_key {
        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "from": "chatweb.ai <noreply@chatweb.ai>",
            "to": [&email],
            "subject": format!("Your login code: {}", code),
            "html": format!(
                "<div style='font-family:system-ui;max-width:400px;margin:40px auto;padding:32px;background:#09090b;border-radius:16px;border:1px solid #27272a'>\
                <div style='width:48px;height:48px;border-radius:12px;background:linear-gradient(135deg,#a78bfa,#60a5fa);margin-bottom:24px'></div>\
                <h2 style='color:#fafafa;font-size:22px;margin:0 0 8px'>Your login code</h2>\
                <p style='color:#a1a1aa;font-size:14px;margin:0 0 24px'>Enter this code to sign in to chatweb.ai</p>\
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

// ── Google OAuth ──────────────────────────────────────────────────────────────

/// Returns the OAuth redirect URI registered in Google Cloud Console.
/// Prefer GOOGLE_REDIRECT_URI env var (allows using a canonical domain like claudeterm.fly.dev
/// even when BASE_URL is chatweb.ai), falling back to BASE_URL.
fn google_redirect_uri(base_url: &str) -> String {
    let base = std::env::var("GOOGLE_REDIRECT_URI").unwrap_or_else(|_| base_url.to_string());
    format!("{}/auth/google/callback", base)
}

async fn google_oauth_start(State(s): State<Arc<AppState>>) -> Response {
    let client_id = match std::env::var("GOOGLE_CLIENT_ID") {
        Ok(id) => id,
        Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, "Google OAuth not configured").into_response(),
    };
    let redirect_uri = google_redirect_uri(&s.base_url);
    let url = format!(
        "https://accounts.google.com/o/oauth2/v2/auth\
        ?client_id={}\
        &redirect_uri={}\
        &response_type=code\
        &scope=email+profile\
        &access_type=offline\
        &prompt=select_account",
        urlencoding::encode(&client_id),
        urlencoding::encode(&redirect_uri)
    );
    axum::response::Redirect::temporary(&url).into_response()
}

#[derive(Deserialize)]
struct OAuthCallbackQ { code: Option<String>, error: Option<String> }

async fn google_oauth_callback(
    Query(q): Query<OAuthCallbackQ>,
    State(s): State<Arc<AppState>>,
) -> Response {
    if let Some(err) = q.error {
        return axum::response::Redirect::temporary(&format!("/?oauth_error={}", urlencoding::encode(&err))).into_response();
    }
    let code = match q.code {
        Some(c) => c,
        None => return axum::response::Redirect::temporary("/?oauth_error=missing_code").into_response(),
    };

    let client_id = match std::env::var("GOOGLE_CLIENT_ID") {
        Ok(id) => id,
        Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, "Google OAuth not configured").into_response(),
    };
    let client_secret = match std::env::var("GOOGLE_CLIENT_SECRET") {
        Ok(s) => s,
        Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, "Google OAuth not configured").into_response(),
    };
    let redirect_uri = google_redirect_uri(&s.base_url);

    // Exchange code for token
    let http = reqwest::Client::new();
    let token_resp = http.post("https://oauth2.googleapis.com/token")
        .form(&[
            ("code", code.as_str()),
            ("client_id", &client_id),
            ("client_secret", &client_secret),
            ("redirect_uri", &redirect_uri),
            ("grant_type", "authorization_code"),
        ])
        .send().await;

    let token_json: serde_json::Value = match token_resp {
        Ok(r) => match r.json().await {
            Ok(j) => j,
            Err(_) => return axum::response::Redirect::temporary("/?oauth_error=token_parse").into_response(),
        },
        Err(_) => return axum::response::Redirect::temporary("/?oauth_error=token_request").into_response(),
    };

    let access_token = match token_json["access_token"].as_str() {
        Some(t) => t.to_string(),
        None => return axum::response::Redirect::temporary("/?oauth_error=no_access_token").into_response(),
    };

    // Get user info
    let userinfo_resp = http
        .get("https://www.googleapis.com/oauth2/v3/userinfo")
        .bearer_auth(&access_token)
        .send().await;
    let userinfo: serde_json::Value = match userinfo_resp {
        Ok(r) => r.json::<serde_json::Value>().await.unwrap_or_default(),
        Err(_) => return axum::response::Redirect::temporary("/?oauth_error=userinfo").into_response(),
    };

    let email = match userinfo["email"].as_str() {
        Some(e) => e.to_lowercase(),
        None => return axum::response::Redirect::temporary("/?oauth_error=no_email").into_response(),
    };

    // Upsert user
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    let existing: Option<(String, String)> = db.query_row(
        "SELECT id, token FROM users WHERE email=?1", [&email],
        |r| Ok((r.get(0)?, r.get(1)?))
    ).ok();

    let token = if let Some((_, tok)) = existing {
        tok
    } else {
        let uid = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let tok = uuid::Uuid::new_v4().to_string().replace("-", "");
        let now = chrono::Utc::now().to_rfc3339();
        db.execute("INSERT INTO users (id, email, token, credits, created_at) VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![uid, email, tok, INITIAL_CREDITS, now]).unwrap();
        tok
    };

    // Redirect to frontend with token in fragment (never in server logs)
    // Always redirect to BASE_URL so user lands on the canonical domain (chatweb.ai)
    axum::response::Redirect::temporary(&format!("{}/#google_token={}", s.base_url, token)).into_response()
}

// ── Email OTP ─────────────────────────────────────────────────────────────────

async fn verify_otp(State(s): State<Arc<AppState>>, Json(body): Json<serde_json::Value>) -> Response {
    let email = match body.get("email").and_then(|e| e.as_str()) {
        Some(e) if e.contains('@') => e.to_lowercase(),
        _ => return (StatusCode::BAD_REQUEST, "Invalid email").into_response(),
    };
    let code = match body.get("code").and_then(|c| c.as_str()) {
        Some(c) => c.to_string(),
        None => return (StatusCode::BAD_REQUEST, "Missing code").into_response(),
    };

    // Validate OTP from DB
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let valid = {
        let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
        let row: Option<(String, i64)> = db.query_row(
            "SELECT code, expires_at FROM otps WHERE email=?1", [&email],
            |r| Ok((r.get(0)?, r.get(1)?))
        ).ok();
        if let Some((stored_code, expires)) = row {
            if expires > now && stored_code == code {
                db.execute("DELETE FROM otps WHERE email=?1", [&email]).ok();
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
        "token": token, "user": { "id": uid, "email": email, "credits": credits, "has_api_key": has_key, "plan": "free" }
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
        "user": {"id": uid, "email": "Local (Mac)", "credits": 999999.0, "has_api_key": false, "plan": "power"}
    })).into_response()
}

async fn me(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    match auth_user(&s, q.token.as_deref()) {
        Some((id, email, credits, api_key, plan)) => Json(UserDto {
            id, email, credits, has_api_key: api_key.is_some(), plan
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
            projects.push(ProjectEntry{name:name.clone(),path:name});
        }
    }
    projects.sort_by(|a,b|a.name.cmp(&b.name));
    Json(projects).into_response()
}

#[derive(Deserialize)] struct CreateProjectReq { name: String, #[serde(rename = "type")] project_type: Option<String> }

async fn create_project(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    Json(body): Json<CreateProjectReq>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let name: String = body.name.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .take(50).collect();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "Invalid project name").into_response();
    }
    let dir = PathBuf::from(format!("{}/users/{}/{}", s.workdir, uid, name));
    std::fs::create_dir_all(&dir).ok();

    // Base template (shared across all project types)
    let base_rules = format!(r#"# {name}

## Critical Rules (MUST follow)
- **NEVER say "done" without evidence**: Always show diffs, test output, or logs as proof
- **NEVER guess at fixes**: Read error messages, grep the codebase, trace the stack — then fix
- **NEVER commit secrets**: API keys, tokens, credentials → `.env` only, never in code or logs
- **Ask before acting** on: migrations, schema changes, external API cost increases, destructive operations

## Workflow
1. **Explore** — Read relevant code. `grep`/`glob` before asking questions
2. **Plan** — Write approach in `tasks/todo.md` for 3+ step tasks
3. **Implement** — Small, focused changes following existing patterns
4. **Verify** — Run tests/build, confirm behavior
5. **Report** — Show what changed with evidence

## Learning Loop
- Read `tasks/lessons.md` at session start
- After mistakes or corrections, append lessons immediately

"#);

    let project_section = match body.project_type.as_deref().unwrap_or("general") {
        "webapp" => format!(r#"## Project: Web Application

### Stack
- Framework: React / Next.js / Vue — choose based on requirements
- Language: TypeScript (strict mode)
- Styling: Tailwind CSS or CSS Modules
- Deploy: Fly.io (`fly deploy`) or Cloudflare Pages

### Conventions
- Mobile-first responsive design
- Semantic HTML, WCAG accessibility basics
- Environment variables in `.env` (never hardcode URLs or keys)
- `package.json` scripts: `dev`, `build`, `test`, `lint`

### Commands
- `npm run dev` — development server (hot reload)
- `npm run build` — production build
- `npm test` — run tests
- `fly deploy --remote-only` — deploy to production

### Quality Checklist
- [ ] Responsive on mobile (375px+)
- [ ] No console errors
- [ ] Core Web Vitals pass (LCP < 2.5s)
- [ ] SEO meta tags + OGP set
"#),
        "mobile" => format!(r#"## Project: Mobile App

### Stack
- iOS: Swift 5.9+ / SwiftUI (primary)
- Android: Kotlin / Jetpack Compose (if needed)
- Architecture: MVVM + Repository pattern
- Data: SwiftData or CoreData

### Conventions
- Support dark mode and Dynamic Type
- Follow Apple HIG / Material Design guidelines
- Localize all user-facing strings
- Handle offline/error states gracefully

### Commands
- `xcodebuild -scheme {name} -destination 'platform=iOS Simulator,name=iPhone 16'` — build
- `xcodebuild test` — run tests
- `fastlane ios beta` — upload to TestFlight

### Quality Checklist
- [ ] Works on iPhone SE (smallest screen)
- [ ] Dark mode tested
- [ ] No memory leaks (Instruments)
- [ ] App Store screenshots ready
"#),
        "api" => format!(r#"## Project: API Server

### Stack
- Language: Rust (axum) or Node.js (Express/Fastify)
- Database: SQLite (rusqlite) or PostgreSQL
- API style: RESTful JSON
- Auth: JWT or API key

### Conventions
- All endpoints documented with examples
- Input validation at boundaries
- Structured error responses: `{{"error": "message", "code": "..."}}`
- Database migrations tracked in `migrations/`
- Rate limiting on public endpoints

### Commands
- `cargo run` / `npm start` — start server
- `cargo test` / `npm test` — run tests
- `curl localhost:3000/health` — health check
- `fly deploy --remote-only` — deploy

### Quality Checklist
- [ ] All endpoints tested with curl examples
- [ ] Error cases handled (400, 401, 404, 500)
- [ ] No N+1 queries
- [ ] CORS configured correctly
"#),
        "data" => format!(r#"## Project: Data / ML

### Stack
- Language: Python 3.11+
- Libraries: pandas, numpy, matplotlib, scikit-learn
- Notebooks: Jupyter for exploration
- Pipeline: scripts in `src/`, data in `data/`

### Conventions
- Reproducible: pin dependencies in `requirements.txt`
- Data files > 10MB → `.gitignore`, document download source
- Clean separation: data loading → processing → analysis → output
- Document assumptions and data sources

### Commands
- `python -m venv .venv && source .venv/bin/activate` — setup
- `pip install -r requirements.txt` — install deps
- `python main.py` — run pipeline
- `pytest` — run tests
- `jupyter notebook` — interactive exploration

### Quality Checklist
- [ ] Results reproducible from clean checkout
- [ ] Charts have titles, labels, legends
- [ ] No hardcoded file paths (use relative or env vars)
"#),
        "devops" => format!(r#"## Project: DevOps / Infrastructure

### Stack
- Containers: Docker + docker-compose
- CI/CD: GitHub Actions
- Cloud: Fly.io / AWS / GCP
- IaC: Terraform or Fly.toml
- Monitoring: health check endpoints + uptime alerts

### Conventions
- Dockerfile: multi-stage build, non-root user
- All secrets in environment variables (never in files)
- Infrastructure as code — no manual console changes
- Blue-green or rolling deployments
- Health check endpoint at `/health`

### Commands
- `docker compose up -d` — local environment
- `docker compose logs -f` — watch logs
- `fly deploy --remote-only` — deploy to Fly.io
- `terraform plan` → `terraform apply` — infra changes
- `gh workflow run deploy.yml` — trigger CI/CD

### Quality Checklist
- [ ] `docker compose up` works from clean clone
- [ ] Health check returns 200
- [ ] Secrets not in docker image or logs
- [ ] Rollback procedure documented
"#),
        _ => "## Project: General\n\nFlexible workspace. Ask Claude anything.\n".to_string(),
    };

    let claude_md = format!("{}{}", base_rules, project_section);
    std::fs::write(dir.join("CLAUDE.md"), &claude_md).ok();
    Json(serde_json::json!({"name": name, "path": name})).into_response()
}

#[derive(Deserialize)] struct MergeReq { from: String, to: String }

async fn merge_projects(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    Json(body): Json<MergeReq>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let base = PathBuf::from(format!("{}/users/{}", s.workdir, uid));
    let src = base.join(&body.from);
    let dst = base.join(&body.to);
    if !src.starts_with(&base) || !dst.starts_with(&base) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !src.is_dir() {
        return (StatusCode::NOT_FOUND, "Source project not found").into_response();
    }
    std::fs::create_dir_all(&dst).ok();
    // Recursive copy (merge: existing files in dst are NOT overwritten)
    fn copy_dir_merge(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<u64> {
        let mut count = 0u64;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let name = entry.file_name();
            let s = entry.path();
            let d = dst.join(&name);
            if s.is_dir() {
                std::fs::create_dir_all(&d)?;
                count += copy_dir_merge(&s, &d)?;
            } else if !d.exists() {
                // Only copy if destination file does NOT exist (no overwrite)
                std::fs::copy(&s, &d)?;
                count += 1;
            }
        }
        Ok(count)
    }
    match copy_dir_merge(&src, &dst) {
        Ok(n) => {
            tracing::info!("Merged {} → {}: {} files", body.from, body.to, n);
            Json(serde_json::json!({"ok": true, "files_copied": n})).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── Clone from GitHub ──

#[derive(Deserialize)] struct CloneReq { url: String }

async fn clone_project(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    Json(body): Json<CloneReq>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    // Validate: only allow github.com URLs matching the expected pattern
    let url = body.url.trim();
    let stripped = url.strip_prefix("https://github.com/").unwrap_or("");
    let parts: Vec<&str> = stripped.split('/').collect();
    if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
        return (StatusCode::BAD_REQUEST, "Invalid GitHub URL. Use https://github.com/owner/repo").into_response();
    }
    let owner = parts[0];
    let repo_raw = parts[1].trim_end_matches(".git");
    // Sanitize owner and repo: only allow [a-zA-Z0-9_.-]
    let valid_chars = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if !valid_chars(owner) || !valid_chars(repo_raw) || owner.starts_with('.') || repo_raw.starts_with('.') {
        return (StatusCode::BAD_REQUEST, "Invalid repository name").into_response();
    }
    let repo_name = repo_raw.to_string();
    let clone_url = format!("https://github.com/{}/{}.git", owner, repo_name);
    let user_dir = PathBuf::from(format!("{}/users/{}", s.workdir, uid));
    std::fs::create_dir_all(&user_dir).ok();
    let dest = user_dir.join(&repo_name);
    if dest.exists() {
        return (StatusCode::CONFLICT, "Project already exists").into_response();
    }
    // Run git clone in background
    match Command::new("git")
        .args(["clone", "--depth", "1", &clone_url, &dest.to_string_lossy()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => {
            match child.wait_with_output().await {
                Ok(out) if out.status.success() => {
                    tracing::info!("Cloned {} for user {}", clone_url, uid);
                    Json(serde_json::json!({"name": repo_name, "path": repo_name})).into_response()
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    tracing::warn!("git clone failed: {}", stderr);
                    // Clean up partial clone
                    let _ = std::fs::remove_dir_all(&dest);
                    (StatusCode::BAD_REQUEST, format!("Clone failed: {}", stderr.chars().take(200).collect::<String>())).into_response()
                }
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&dest);
                    (StatusCode::INTERNAL_SERVER_ERROR, format!("Process error: {}", e)).into_response()
                }
            }
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to run git: {}", e)).into_response(),
    }
}

// ── Public App Preview ──

fn content_type_for(path: &str) -> &'static str {
    if path.ends_with(".html") || path.ends_with(".htm") { "text/html; charset=utf-8" }
    else if path.ends_with(".js") || path.ends_with(".mjs") { "application/javascript" }
    else if path.ends_with(".css") { "text/css" }
    else if path.ends_with(".json") { "application/json" }
    else if path.ends_with(".png") { "image/png" }
    else if path.ends_with(".jpg") || path.ends_with(".jpeg") { "image/jpeg" }
    else if path.ends_with(".gif") { "image/gif" }
    else if path.ends_with(".svg") { "image/svg+xml" }
    else if path.ends_with(".ico") { "image/x-icon" }
    else if path.ends_with(".woff2") { "font/woff2" }
    else if path.ends_with(".woff") { "font/woff" }
    else if path.ends_with(".wasm") { "application/wasm" }
    else { "application/octet-stream" }
}

async fn serve_user_app(
    Path((uid, project, path)): Path<(String, String, String)>,
    State(s): State<Arc<AppState>>,
) -> Response {
    // Sanitize uid/project to prevent directory traversal
    if uid.contains("..") || project.contains("..") || path.contains("..") {
        return StatusCode::FORBIDDEN.into_response();
    }
    let file_path = if path.is_empty() || path == "/" {
        PathBuf::from(format!("{}/users/{}/{}/index.html", s.workdir, uid, project))
    } else {
        PathBuf::from(format!("{}/users/{}/{}/{}", s.workdir, uid, project, path))
    };
    // Ensure the resolved path stays within the user dir
    let base = PathBuf::from(format!("{}/users/{}/{}", s.workdir, uid, project));
    if let (Ok(resolved), Ok(base_resolved)) = (file_path.canonicalize(), base.canonicalize()) {
        if !resolved.starts_with(&base_resolved) {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    match std::fs::read(&file_path) {
        Ok(data) => {
            let ct = content_type_for(&file_path.to_string_lossy());
            (StatusCode::OK, [
                ("content-type", ct),
                ("access-control-allow-origin", "*"),
                ("cache-control", "public, max-age=300"),
            ], data).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn serve_user_app_index(
    Path((uid, project)): Path<(String, String)>,
    State(s): State<Arc<AppState>>,
) -> Response {
    serve_user_app(Path((uid, project, String::new())), State(s)).await
}

// ── Community Gallery ──

#[derive(Deserialize)] struct PublishReq { project: String, title: String, description: Option<String>, tags: Option<String> }

async fn publish_project(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    Json(body): Json<PublishReq>) -> Response {
    let (uid, email, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if body.title.trim().is_empty() || body.project.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "title and project required").into_response();
    }
    // Check project exists
    let dir = PathBuf::from(format!("{}/users/{}/{}", s.workdir, uid, body.project));
    if !dir.is_dir() { return (StatusCode::NOT_FOUND, "Project not found").into_response(); }

    let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let author = email.split('@').next().unwrap_or("user").to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    db.execute(
        "INSERT OR REPLACE INTO gallery (id,user_id,author,project,title,description,tags,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        rusqlite::params![id, uid, author, body.project, body.title.trim(),
            body.description.as_deref().unwrap_or(""), body.tags.as_deref().unwrap_or(""), now]
    ).ok();
    Json(serde_json::json!({"id": id, "ok": true})).into_response()
}

async fn gallery(State(s): State<Arc<AppState>>) -> Response {
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = db.prepare(
        "SELECT id,author,project,title,description,tags,likes,created_at,user_id FROM gallery ORDER BY created_at DESC LIMIT 50"
    ).unwrap();
    let items: Vec<serde_json::Value> = st.query_map([], |r| {
        let uid: String = r.get(8)?;
        let project: String = r.get(2)?;
        Ok(serde_json::json!({
            "id": r.get::<_,String>(0)?, "author": r.get::<_,String>(1)?,
            "project": project, "title": r.get::<_,String>(3)?,
            "description": r.get::<_,String>(4)?, "tags": r.get::<_,String>(5)?,
            "likes": r.get::<_,i64>(6)?, "created_at": r.get::<_,String>(7)?,
            "url": format!("/app/{}/{}/", uid, project),
        }))
    }).unwrap().filter_map(|r| r.ok()).collect();
    Json(items).into_response()
}

// ── Live Preview ──

async fn set_preview_port(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let port = body.get("port").and_then(|p| p.as_u64()).unwrap_or(0) as u16;
    if port > 0 {
        s.preview_ports.lock().unwrap_or_else(|e| e.into_inner()).insert(uid, port);
    }
    Json(serde_json::json!({"ok": true})).into_response()
}

async fn get_preview_port(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let port = s.preview_ports.lock().unwrap_or_else(|e| e.into_inner()).get(&uid).copied();
    Json(serde_json::json!({"port": port})).into_response()
}

// ── File Editor ──

#[derive(Deserialize)] struct WriteFileReq { path: String, content: String }

async fn write_file(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    Json(body): Json<WriteFileReq>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let base = PathBuf::from(format!("{}/users/{}", s.workdir, uid));
    let file = base.join(&body.path);
    if !file.starts_with(&base) { return StatusCode::FORBIDDEN.into_response(); }
    if let Some(parent) = file.parent() { std::fs::create_dir_all(parent).ok(); }
    match std::fs::write(&file, &body.content) {
        Ok(_) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── GitHub Status ──

async fn github_status(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let user_dir = format!("{}/users/{}", s.workdir, uid);
    // Check if gh CLI is authenticated
    let output = Command::new("gh")
        .arg("auth").arg("status")
        .current_dir(&user_dir)
        .output().await;
    let authenticated = output.map(|o| o.status.success()).unwrap_or(false);
    Json(serde_json::json!({"authenticated": authenticated})).into_response()
}

// ── Template Starters ──

#[derive(Deserialize)] struct TemplateReq { name: String, template: String }

async fn create_from_template(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    Json(body): Json<TemplateReq>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let name: String = body.name.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .take(50).collect();
    if name.is_empty() { return (StatusCode::BAD_REQUEST, "Invalid name").into_response(); }
    let dir = PathBuf::from(format!("{}/users/{}/{}", s.workdir, uid, name));
    std::fs::create_dir_all(&dir).ok();

    match body.template.as_str() {
        "nextjs-blog" => {
            std::fs::write(dir.join("package.json"), r#"{"name":"my-blog","private":true,"scripts":{"dev":"next dev","build":"next build","start":"next start"},"dependencies":{"next":"14","react":"18","react-dom":"18"},"devDependencies":{"typescript":"5","@types/react":"18","@types/node":"20"}}"#).ok();
            std::fs::create_dir_all(dir.join("app")).ok();
            std::fs::write(dir.join("app/layout.tsx"), "export default function RootLayout({children}:{children:React.ReactNode}){return(<html><body>{children}</body></html>)}\nexport const metadata={title:'My Blog'}").ok();
            std::fs::write(dir.join("app/page.tsx"), "export default function Home(){return(<main style={{maxWidth:640,margin:'0 auto',padding:20}}><h1>My Blog</h1><p>Welcome to my blog built with Next.js.</p><article><h2>First Post</h2><p>Hello, world!</p></article></main>)}").ok();
            std::fs::write(dir.join("tsconfig.json"), r#"{"compilerOptions":{"target":"es5","lib":["dom","es2017"],"jsx":"preserve","module":"esnext","moduleResolution":"bundler","strict":true,"esModuleInterop":true,"skipLibCheck":true}}"#).ok();
        }
        "react-todo" => {
            std::fs::write(dir.join("package.json"), r#"{"name":"todo-app","private":true,"scripts":{"dev":"vite","build":"vite build"},"dependencies":{"react":"18","react-dom":"18"},"devDependencies":{"vite":"5","@vitejs/plugin-react":"4","typescript":"5","@types/react":"18","@types/react-dom":"18"}}"#).ok();
            std::fs::create_dir_all(dir.join("src")).ok();
            std::fs::write(dir.join("index.html"), r#"<!DOCTYPE html><html><head><title>Todo App</title></head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>"#).ok();
            std::fs::write(dir.join("src/main.tsx"), "import React from'react';import ReactDOM from'react-dom/client';import App from'./App';ReactDOM.createRoot(document.getElementById('root')!).render(<App/>)").ok();
            std::fs::write(dir.join("src/App.tsx"), r#"import{useState}from'react';export default function App(){const[todos,setTodos]=useState<{text:string,done:boolean}[]>([]);const[input,setInput]=useState('');const add=()=>{if(!input.trim())return;setTodos([...todos,{text:input,done:false}]);setInput('')};return(<div style={{maxWidth:480,margin:'40px auto',fontFamily:'system-ui'}}><h1>Todo App</h1><div style={{display:'flex',gap:8}}><input value={input} onChange={e=>setInput(e.target.value)} onKeyDown={e=>e.key==='Enter'&&add()} placeholder="Add a todo..." style={{flex:1,padding:8,borderRadius:8,border:'1px solid #ddd'}}/><button onClick={add} style={{padding:'8px 16px',borderRadius:8,background:'#7c3aed',color:'#fff',border:'none'}}>Add</button></div><ul style={{listStyle:'none',padding:0,marginTop:16}}>{todos.map((t,i)=>(<li key={i} onClick={()=>{const n=[...todos];n[i].done=!n[i].done;setTodos(n)}} style={{padding:12,margin:'4px 0',background:'#f9fafb',borderRadius:8,cursor:'pointer',textDecoration:t.done?'line-through':'none',opacity:t.done?.5:1}}>{t.text}</li>))}</ul></div>)}"#).ok();
            std::fs::write(dir.join("vite.config.ts"), "import{defineConfig}from'vite';import react from'@vitejs/plugin-react';export default defineConfig({plugins:[react()]})").ok();
        }
        "api-rust" => {
            std::fs::write(dir.join("Cargo.toml"), &format!("[package]\nname=\"{}\"\nversion=\"0.1.0\"\nedition=\"2021\"\n\n[dependencies]\naxum=\"0.7\"\ntokio={{version=\"1\",features=[\"full\"]}}\nserde={{version=\"1\",features=[\"derive\"]}}\nserde_json=\"1\"\n", name)).ok();
            std::fs::create_dir_all(dir.join("src")).ok();
            std::fs::write(dir.join("src/main.rs"), r#"use axum::{routing::get,Router,Json};
use serde_json::{json,Value};

#[tokio::main]
async fn main(){
    let app=Router::new()
        .route("/",get(||async{Json(json!({"message":"Hello, World!"}))}))
        .route("/health",get(||async{"ok"}));
    let addr="0.0.0.0:3000";
    println!("Listening on {addr}");
    let listener=tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener,app).await.unwrap();
}"#).ok();
            std::fs::write(dir.join("Dockerfile"), &format!("FROM rust:1.88-slim AS build\nWORKDIR /app\nCOPY . .\nRUN cargo build --release\n\nFROM debian:bookworm-slim\nCOPY --from=build /app/target/release/{} /usr/local/bin/app\nEXPOSE 3000\nCMD [\"app\"]\n", name)).ok();
        }
        "api-node" => {
            std::fs::write(dir.join("package.json"), r#"{"name":"api-server","private":true,"scripts":{"dev":"tsx watch src/index.ts","build":"tsc","start":"node dist/index.js"},"dependencies":{"express":"4"},"devDependencies":{"tsx":"4","typescript":"5","@types/express":"4","@types/node":"20"}}"#).ok();
            std::fs::create_dir_all(dir.join("src")).ok();
            std::fs::write(dir.join("src/index.ts"), "import express from'express';\nconst app=express();\napp.use(express.json());\napp.get('/',(_,res)=>res.json({message:'Hello, World!'}));\napp.get('/health',(_,res)=>res.send('ok'));\nconst port=process.env.PORT||3000;\napp.listen(port,()=>console.log(`Listening on port ${port}`));\n").ok();
            std::fs::write(dir.join("tsconfig.json"), r#"{"compilerOptions":{"target":"es2020","module":"commonjs","outDir":"dist","strict":true,"esModuleInterop":true},"include":["src"]}"#).ok();
        }
        "python-data" => {
            std::fs::write(dir.join("requirements.txt"), "pandas>=2.0\nnumpy>=1.24\nmatplotlib>=3.7\nscikit-learn>=1.3\njupyter>=1.0\n").ok();
            std::fs::write(dir.join("main.py"), r#"import pandas as pd
import numpy as np
import matplotlib.pyplot as plt

# Generate sample data
np.random.seed(42)
df = pd.DataFrame({
    'x': np.random.randn(100),
    'y': np.random.randn(100),
    'category': np.random.choice(['A', 'B', 'C'], 100)
})

print(f"Dataset: {len(df)} rows, {len(df.columns)} columns")
print(df.describe())

# Create visualization
fig, axes = plt.subplots(1, 2, figsize=(12, 5))
df.groupby('category')['x'].mean().plot(kind='bar', ax=axes[0], title='Mean X by Category')
axes[1].scatter(df['x'], df['y'], c=df['category'].astype('category').cat.codes, alpha=0.6)
axes[1].set_title('X vs Y')
plt.tight_layout()
plt.savefig('output.png', dpi=150)
print("Chart saved to output.png")
"#).ok();
            std::fs::write(dir.join(".gitignore"), "*.pyc\n__pycache__/\n.venv/\n*.egg-info/\ndata/\n*.csv\n").ok();
        }
        "landing-page" => {
            std::fs::write(dir.join("index.html"), r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>My Landing Page</title>
<style>
*{margin:0;padding:0;box-sizing:border-box}
body{font-family:system-ui,sans-serif;color:#111;background:#fff}
.hero{min-height:100vh;display:flex;flex-direction:column;align-items:center;justify-content:center;text-align:center;padding:40px 20px;background:linear-gradient(135deg,#667eea,#764ba2)}
.hero h1{font-size:clamp(2rem,5vw,4rem);color:#fff;font-weight:800;letter-spacing:-.04em;margin-bottom:16px}
.hero p{font-size:1.2rem;color:rgba(255,255,255,.8);max-width:600px;line-height:1.6;margin-bottom:32px}
.btn{display:inline-block;padding:14px 32px;background:#fff;color:#764ba2;font-size:1rem;font-weight:700;border-radius:99px;text-decoration:none;transition:transform .15s}
.btn:hover{transform:translateY(-2px)}
.features{display:grid;grid-template-columns:repeat(auto-fit,minmax(250px,1fr));gap:24px;max-width:900px;margin:80px auto;padding:0 20px}
.feature{padding:24px;border-radius:16px;background:#f9fafb;border:1px solid #e5e7eb}
.feature h3{font-size:1.1rem;margin-bottom:8px}.feature p{color:#6b7280;line-height:1.6}
footer{text-align:center;padding:40px;color:#9ca3af;font-size:.9rem}
</style>
</head>
<body>
<div class="hero">
  <h1>Your Product Name</h1>
  <p>A brief, compelling description of what your product does and why people should care.</p>
  <a href="#" class="btn">Get Started</a>
</div>
<div class="features">
  <div class="feature"><h3>Feature One</h3><p>Explain the first key benefit of your product.</p></div>
  <div class="feature"><h3>Feature Two</h3><p>Explain the second key benefit of your product.</p></div>
  <div class="feature"><h3>Feature Three</h3><p>Explain the third key benefit of your product.</p></div>
</div>
<footer>&copy; 2026 Your Company. All rights reserved.</footer>
</body>
</html>"##).ok();
        }
        _ => {
            return (StatusCode::BAD_REQUEST, "Unknown template").into_response();
        }
    }

    // Write CLAUDE.md with appropriate type
    let ptype = match body.template.as_str() {
        "nextjs-blog"|"react-todo"|"landing-page" => "webapp",
        "api-rust"|"api-node" => "api",
        "python-data" => "data",
        _ => "general",
    };
    // Reuse the CLAUDE.md generation
    let base_rules = format!("# {name}\n\n## Critical Rules (MUST follow)\n- **NEVER say \"done\" without evidence**: Always show diffs, test output, or logs as proof\n- **NEVER guess at fixes**: Read error messages, grep the codebase, trace the stack — then fix\n- **NEVER commit secrets**: API keys, tokens, credentials → `.env` only, never in code or logs\n\n## Workflow\n1. Explore → 2. Plan → 3. Implement → 4. Verify → 5. Report\n\n");
    std::fs::write(dir.join("CLAUDE.md"), format!("{}## Template: {}\n", base_rules, body.template)).ok();

    Json(serde_json::json!({"name": name, "path": name, "template": body.template})).into_response()
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
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    match billing::parse_webhook_action(&body) {
        Some(billing::WebhookAction::OneTimeCredits { token, credits }) => {
            db.execute("UPDATE users SET credits = credits + ?1 WHERE token = ?2",
                rusqlite::params![credits, token]).ok();
            tracing::info!("One-time +${} credits", credits);
        }
        Some(billing::WebhookAction::SubscriptionStarted { token, plan, customer_id }) => {
            let credits = billing::plan_credits(&plan);
            db.execute(
                "UPDATE users SET credits = credits + ?1, plan = ?2, stripe_customer_id = ?3 WHERE token = ?4",
                rusqlite::params![credits, plan, customer_id, token]).ok();
            tracing::info!("Subscription {} started: +${} credits", plan, credits);
        }
        Some(billing::WebhookAction::SubscriptionRenewed { customer_id }) => {
            let plan: String = db.query_row(
                "SELECT COALESCE(plan,'starter') FROM users WHERE stripe_customer_id = ?1",
                [&customer_id], |r| r.get(0)
            ).unwrap_or_else(|_| "starter".to_string());
            let credits = billing::plan_credits(&plan);
            db.execute("UPDATE users SET credits = credits + ?1 WHERE stripe_customer_id = ?2",
                rusqlite::params![credits, customer_id]).ok();
            tracing::info!("Subscription {} renewed: +${} credits", plan, credits);
        }
        None => {}
    }
    StatusCode::OK.into_response()
}

async fn billing_success(Query(q): Query<TokenQ>) -> Html<&'static str> {
    Html("<html><body style='background:#09090b;color:#fff;font-family:system-ui;display:flex;align-items:center;justify-content:center;height:100dvh'><div style='text-align:center;max-width:400px;padding:24px'><div style='font-size:48px;margin-bottom:16px'>✅</div><h1 style='font-size:24px;font-weight:700;margin-bottom:8px'>Payment successful!</h1><p style='color:#a1a1aa;margin-bottom:24px'>Credits have been added to your account.</p><a href='/' style='background:#a78bfa;color:#000;padding:12px 24px;border-radius:10px;text-decoration:none;font-weight:600'>Back to ChatWeb</a></div></body></html>")
}

async fn billing_cancel() -> Html<&'static str> {
    Html("<html><body style='background:#09090b;color:#fff;font-family:system-ui;display:flex;align-items:center;justify-content:center;height:100dvh'><div style='text-align:center;max-width:400px;padding:24px'><div style='font-size:48px;margin-bottom:16px'>❌</div><h1 style='font-size:24px;font-weight:700;margin-bottom:8px'>Payment cancelled</h1><p style='color:#a1a1aa;margin-bottom:24px'>No charge was made. You can try again from the settings.</p><a href='/' style='background:#a78bfa;color:#000;padding:12px 24px;border-radius:10px;text-decoration:none;font-weight:600'>Back to ChatWeb</a></div></body></html>")
}

#[derive(Deserialize)] struct AdminCreditReq { token: String, email: String, amount: f64 }

// ── Subscription ──

#[derive(Deserialize)] struct SubscribeReq { plan: String }

async fn create_subscription(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    Json(body): Json<SubscribeReq>) -> Response {
    let (_, email, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let stripe_key = match &s.stripe_key {
        Some(k) => k, None => return (StatusCode::SERVICE_UNAVAILABLE, "Payments not configured").into_response(),
    };
    let token = q.token.unwrap_or_default();
    match billing::create_subscription_checkout(stripe_key, &email, &token, &body.plan, &s.base_url).await {
        Ok(url) => Json(serde_json::json!({"url": url})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// ── Admin ──

async fn admin_credit(State(s): State<Arc<AppState>>, Json(body): Json<AdminCreditReq>) -> Response {
    let admin_token = s.admin_token.as_deref().unwrap_or("");
    if body.token != admin_token || admin_token.is_empty() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    let rows = db.execute(
        "UPDATE users SET credits = credits + ?1 WHERE email = ?2",
        rusqlite::params![body.amount, body.email],
    ).unwrap_or(0);
    if rows == 0 {
        return (StatusCode::NOT_FOUND, "user not found").into_response();
    }
    let new_credits: f64 = db.query_row(
        "SELECT credits FROM users WHERE email = ?1", [&body.email], |r| r.get(0)
    ).unwrap_or(0.0);
    tracing::info!("Admin credited {} ${} (now ${})", body.email, body.amount, new_credits);
    Json(serde_json::json!({"ok": true, "credits": new_credits})).into_response()
}

// ── Feedback ──

#[derive(Deserialize)] struct FeedbackReq { category: String, message: String }

async fn submit_feedback(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    Json(body): Json<FeedbackReq>) -> Response {
    let email = auth_user(&s, q.token.as_deref())
        .map(|(_, e, _, _, _)| e)
        .unwrap_or_else(|| "anonymous".to_string());

    if body.message.trim().is_empty() || body.message.len() > 2000 {
        return (StatusCode::BAD_REQUEST, "invalid message").into_response();
    }

    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
    {
        let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
        db.execute(
            "INSERT INTO feedback (user_email, category, message, created_at) VALUES (?1,?2,?3,?4)",
            rusqlite::params![email, body.category, body.message, now],
        ).ok();
    }

    // Telegram notification
    let bot_token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
    let chat_id = std::env::var("TELEGRAM_CHAT_ID").unwrap_or_else(|_| "1136442501".to_string());
    let emoji = match body.category.as_str() { "bug" => "🐛", "feature" => "💡", _ => "💬" };
    let text = format!(
        "{} *Feedback — {}*\nFrom: `{}`\n\n{}\n\n_{}_",
        emoji, body.category, email, body.message, now
    );
    if !bot_token.is_empty() {
        let client = reqwest::Client::new();
        let _ = client.post(format!("https://api.telegram.org/bot{}/sendMessage", bot_token))
            .json(&serde_json::json!({"chat_id": chat_id, "text": text, "parse_mode": "Markdown"}))
            .send().await;
    }
    tracing::info!("Feedback [{}] from {}: {}", body.category, email, &body.message[..body.message.len().min(80)]);
    Json(serde_json::json!({"ok": true})).into_response()
}

async fn list_feedback(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let admin_token = s.admin_token.as_deref().unwrap_or("");
    if q.token.as_deref() != Some(admin_token) || admin_token.is_empty() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    let mut stmt = db.prepare(
        "SELECT id, user_email, category, message, created_at, status FROM feedback ORDER BY id DESC LIMIT 100"
    ).unwrap();
    let rows: Vec<serde_json::Value> = stmt.query_map([], |r| {
        Ok(serde_json::json!({
            "id": r.get::<_,i64>(0)?, "email": r.get::<_,String>(1)?,
            "category": r.get::<_,String>(2)?, "message": r.get::<_,String>(3)?,
            "created_at": r.get::<_,String>(4)?, "status": r.get::<_,String>(5)?
        }))
    }).unwrap().filter_map(|r| r.ok()).collect();
    Json(rows).into_response()
}

// ── Referral ──

const REFERRAL_BONUS: f64 = 3.0; // both inviter and invitee get $3

/// Get or generate user's referral code
async fn get_referral_code(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());

    // Check existing code
    let existing: Option<String> = db.query_row(
        "SELECT referral_code FROM users WHERE id=?1", [&uid],
        |r| r.get(0)).ok().flatten();

    let code = if let Some(c) = existing {
        c
    } else {
        // Generate 8-char code
        let code: String = (0..8).map(|_| {
            let c = rand::random::<u8>() % 36;
            if c < 10 { (b'0' + c) as char } else { (b'A' + c - 10) as char }
        }).collect();
        db.execute("UPDATE users SET referral_code=?1 WHERE id=?2",
            rusqlite::params![code, uid]).ok();
        code
    };

    // Count successful referrals
    let count: i64 = db.query_row(
        "SELECT COUNT(*) FROM referrals WHERE inviter_id=?1", [&uid], |r| r.get(0)
    ).unwrap_or(0);

    let url = format!("{}/r/{}", s.base_url, code);
    Json(serde_json::json!({
        "code": code, "url": url, "referrals": count,
        "bonus": format!("${:.0}", REFERRAL_BONUS)
    })).into_response()
}

/// Apply referral code (called after signup)
async fn apply_referral(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>) -> Response {
    let (uid, _, _, _, _) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let code = match body.get("code").and_then(|c| c.as_str()) {
        Some(c) if !c.is_empty() => c.to_uppercase(),
        _ => return (StatusCode::BAD_REQUEST, "Missing code").into_response(),
    };

    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());

    // Check if already referred
    let already: bool = db.query_row(
        "SELECT 1 FROM referrals WHERE invitee_id=?1", [&uid], |_| Ok(true)
    ).unwrap_or(false);
    if already {
        return (StatusCode::CONFLICT, Json(serde_json::json!({"error":"already_referred"}))).into_response();
    }

    // Find inviter by referral code
    let inviter: Option<String> = db.query_row(
        "SELECT id FROM users WHERE referral_code=?1", [&code], |r| r.get(0)
    ).ok();
    let inviter_id = match inviter {
        Some(id) if id != uid => id, // Can't refer yourself
        _ => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error":"invalid_code"}))).into_response(),
    };

    // Apply bonus to both
    let now = chrono::Utc::now().to_rfc3339();
    db.execute("INSERT INTO referrals (inviter_id, invitee_id, bonus, created_at) VALUES (?1,?2,?3,?4)",
        rusqlite::params![inviter_id, uid, REFERRAL_BONUS, now]).ok();
    db.execute("UPDATE users SET credits = credits + ?1, referred_by = ?2 WHERE id = ?3",
        rusqlite::params![REFERRAL_BONUS, inviter_id, uid]).ok();
    db.execute("UPDATE users SET credits = credits + ?1 WHERE id = ?2",
        rusqlite::params![REFERRAL_BONUS, inviter_id]).ok();

    tracing::info!("Referral: {} invited {} (+${} each)", &inviter_id[..4], &uid[..4], REFERRAL_BONUS);
    Json(serde_json::json!({"ok": true, "bonus": REFERRAL_BONUS})).into_response()
}

/// Redirect /r/:code → landing page with referral code in URL
async fn referral_redirect(Path(code): Path<String>, State(s): State<Arc<AppState>>) -> Response {
    axum::response::Redirect::temporary(&format!("{}/?ref={}", s.base_url, code)).into_response()
}

// ── Cron ──

#[derive(Serialize)] struct CronDto {
    id: String, name: String, command: String, project: String,
    interval_secs: i64, enabled: bool, last_run: i64,
    last_result: String, last_status: String, created_at: String,
}

#[derive(Deserialize)] struct CronCreateReq {
    name: String, command: String, project: Option<String>, interval_secs: i64,
}

async fn list_crons(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = db.prepare(
        "SELECT id,name,command,project,interval_secs,enabled,last_run,last_result,last_status,created_at FROM cron_jobs WHERE user_id=?1 ORDER BY created_at DESC"
    ).unwrap();
    let list: Vec<CronDto> = st.query_map([&uid], |r| Ok(CronDto {
        id: r.get(0)?, name: r.get(1)?, command: r.get(2)?, project: r.get(3)?,
        interval_secs: r.get(4)?, enabled: r.get::<_,i64>(5)? != 0,
        last_run: r.get(6)?, last_result: r.get(7)?, last_status: r.get(8)?,
        created_at: r.get(9)?,
    })).unwrap().filter_map(|r| r.ok()).collect();
    Json(list).into_response()
}

async fn create_cron(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    Json(body): Json<CronCreateReq>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if body.command.trim().is_empty() || body.name.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "name and command required").into_response();
    }
    // Min 5 min, max 7 days
    let interval = body.interval_secs.max(300).min(604800);
    let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let now = chrono::Utc::now();
    let now_ts = now.timestamp();
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    db.execute(
        "INSERT INTO cron_jobs (id,user_id,name,command,project,interval_secs,next_run,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        rusqlite::params![id, uid, body.name.trim(), body.command.trim(),
            body.project.as_deref().unwrap_or(""), interval, now_ts + interval, now.to_rfc3339()]
    ).ok();
    Json(serde_json::json!({"id": id, "ok": true})).into_response()
}

async fn delete_cron(Path(id): Path<String>, Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    db.execute("DELETE FROM cron_jobs WHERE id=?1 AND user_id=?2", rusqlite::params![id, uid]).ok();
    StatusCode::NO_CONTENT.into_response()
}

async fn toggle_cron(Path(id): Path<String>, Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    db.execute("UPDATE cron_jobs SET enabled = 1 - enabled WHERE id=?1 AND user_id=?2",
        rusqlite::params![id, uid]).ok();
    let enabled: bool = db.query_row(
        "SELECT enabled FROM cron_jobs WHERE id=?1", [&id], |r| Ok(r.get::<_,i64>(0)? != 0)
    ).unwrap_or(false);
    Json(serde_json::json!({"enabled": enabled})).into_response()
}

/// Background scheduler: checks every 30s for due cron jobs
async fn cron_scheduler(state: Arc<AppState>) {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;

        // Get all due jobs
        let jobs: Vec<(String, String, String, String, i64)> = {
            let now = chrono::Utc::now().timestamp();
            let db = state.db.lock().unwrap_or_else(|e| e.into_inner());
            let mut st = db.prepare(
                "SELECT c.id, c.user_id, c.command, c.project, c.interval_secs \
                 FROM cron_jobs c JOIN users u ON c.user_id = u.id \
                 WHERE c.enabled=1 AND c.next_run <= ?1 AND u.credits > 0"
            ).unwrap();
            st.query_map([now], |r| Ok((
                r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?
            ))).unwrap().filter_map(|r| r.ok()).collect()
        };

        for (job_id, uid, command, project, interval) in jobs {
            let state = state.clone();
            let job_id = job_id.clone();
            tokio::spawn(async move {
                tracing::info!("Cron [{}] running for user {}: {}", job_id, &uid[..4], &command[..command.len().min(50)]);

                // Mark as running
                {
                    let now = chrono::Utc::now().timestamp();
                    let db = state.db.lock().unwrap_or_else(|e| e.into_inner());
                    db.execute("UPDATE cron_jobs SET last_run=?1, last_status='running', next_run=?2 WHERE id=?3",
                        rusqlite::params![now, now + interval, job_id]).ok();
                }

                // Run command
                let user_sandbox = format!("{}/users/{}", state.workdir, uid);
                let workdir = if project.is_empty() {
                    user_sandbox.clone()
                } else {
                    let w = format!("{}/{}", user_sandbox, project);
                    if std::path::Path::new(&w).is_dir() { w } else { user_sandbox.clone() }
                };

                let mut cmd = Command::new(&state.command);
                cmd.arg("-p").arg("--output-format").arg("stream-json")
                    .arg("--dangerously-skip-permissions")
                    .arg("--model").arg("claude-haiku-4-5-20251001")
                    .arg(&command)
                    .current_dir(&workdir)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .stdin(std::process::Stdio::null())
                    .env("TERM", "dumb").env("NO_COLOR", "1")
                    .env("CI", "1").env("DISABLE_AUTOUPDATE", "1");

                let result = match cmd.output().await {
                    Ok(output) => {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        // Extract assistant text from stream-json
                        let mut text = String::new();
                        for line in stdout.lines() {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                                if v.get("type").and_then(|t| t.as_str()) == Some("assistant") {
                                    if let Some(ct) = v.get("message")
                                        .and_then(|m| m.get("content"))
                                        .and_then(|c| c.as_array()) {
                                        for p in ct {
                                            if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                                                if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                                                    text.push_str(t);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if text.is_empty() { text = "(no output)".to_string(); }
                        (if output.status.success() { "success" } else { "error" }, text)
                    }
                    Err(e) => ("error", format!("Failed to run: {}", e)),
                };

                // Update result
                let db = state.db.lock().unwrap_or_else(|e| e.into_inner());
                let truncated = if result.1.len() > 2000 { &result.1[..2000] } else { &result.1 };
                db.execute("UPDATE cron_jobs SET last_status=?1, last_result=?2 WHERE id=?3",
                    rusqlite::params![result.0, truncated, job_id]).ok();
                tracing::info!("Cron [{}] done: {}", job_id, result.0);
            });
        }
    }
}

// ── Share ──

async fn create_share(Path(id): Path<String>, Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    // Check session belongs to user, and get existing share_id
    let existing: Option<Option<String>> = db.query_row(
        "SELECT share_id FROM sessions WHERE id=?1 AND user_id=?2",
        rusqlite::params![id, uid], |r| r.get(0)
    ).ok();
    match existing {
        None => return StatusCode::NOT_FOUND.into_response(),
        Some(Some(sid)) if !sid.is_empty() => {
            // Already shared
            let url = format!("{}/s/{}", s.base_url, sid);
            return Json(serde_json::json!({"share_id": sid, "url": url})).into_response();
        }
        _ => {}
    }
    // Generate share ID
    // 24-char random alphanumeric (144 bits of entropy, not enumerable)
    let share_id: String = (0..24).map(|_| {
        let c = rand::random::<u8>() % 36;
        if c < 10 { (b'0' + c) as char } else { (b'a' + c - 10) as char }
    }).collect();
    db.execute("UPDATE sessions SET share_id=?1 WHERE id=?2 AND user_id=?3",
        rusqlite::params![share_id, id, uid]).ok();
    let url = format!("{}/s/{}", s.base_url, share_id);
    Json(serde_json::json!({"share_id": share_id, "url": url})).into_response()
}

async fn get_shared(Path(share_id): Path<String>, State(s): State<Arc<AppState>>) -> Response {
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    let session: Option<(String, String, String)> = db.query_row(
        "SELECT s.id, s.name, u.email FROM sessions s JOIN users u ON s.user_id=u.id WHERE s.share_id=?1",
        [&share_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    ).ok();
    let (sid, name, email) = match session {
        Some(s) => s, None => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut st = db.prepare("SELECT role,content,timestamp FROM messages WHERE session_id=?1 ORDER BY id").unwrap();
    let msgs: Vec<serde_json::Value> = st.query_map([&sid], |r| Ok(serde_json::json!({
        "role":r.get::<_,String>(0)?,"content":r.get::<_,String>(1)?,"timestamp":r.get::<_,String>(2)?
    }))).unwrap().filter_map(|r| r.ok()).collect();
    Json(serde_json::json!({
        "name": name,
        "author": email.split('@').next().unwrap_or("user"),
        "messages": msgs
    })).into_response()
}

async fn view_shared(Path(share_id): Path<String>, State(s): State<Arc<AppState>>) -> Response {
    // Check share exists
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    let exists: bool = db.query_row(
        "SELECT 1 FROM sessions WHERE share_id=?1", [&share_id], |_| Ok(true)
    ).unwrap_or(false);
    if !exists {
        return (StatusCode::NOT_FOUND, Html("<html><body style='background:#09090b;color:#fff;font-family:system-ui;display:flex;align-items:center;justify-content:center;height:100dvh'><div style='text-align:center'><h1>Not Found</h1><p style='color:#a1a1aa'>This shared session does not exist or has been removed.</p><a href='/' style='color:#a78bfa'>Go to ChatWeb</a></div></body></html>")).into_response();
    }
    // Serve the main HTML — the frontend JS will detect /s/:id and render shared view
    Html(HTML).into_response()
}

/// Fork: copy shared session + files into user's own account
async fn fork_shared(Path(share_id): Path<String>, Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    // Find source session
    let src: Option<(String, String, String, String)> = db.query_row(
        "SELECT s.id, s.name, s.project, s.user_id FROM sessions s WHERE s.share_id=?1",
        [&share_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    ).ok();
    let (src_sid, src_name, src_project, src_uid) = match src {
        Some(s) => s, None => return StatusCode::NOT_FOUND.into_response(),
    };

    // Create new session for this user
    let new_sid = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let new_name = format!("{} (fork)", src_name);
    db.execute("INSERT INTO sessions (id,user_id,name,created_at,project) VALUES (?1,?2,?3,?4,?5)",
        rusqlite::params![new_sid, uid, new_name, now, src_project]).ok();

    // Copy messages
    db.execute(
        "INSERT INTO messages (session_id,role,content,timestamp) \
         SELECT ?1,role,content,timestamp FROM messages WHERE session_id=?2 ORDER BY id",
        rusqlite::params![new_sid, src_sid]).ok();

    // Copy project files if they exist
    if !src_project.is_empty() {
        let src_dir = format!("{}/users/{}/{}", s.workdir, src_uid, src_project);
        let dst_dir = format!("{}/users/{}/{}", s.workdir, uid, src_project);
        if std::path::Path::new(&src_dir).is_dir() && src_uid != uid {
            std::fs::create_dir_all(&dst_dir).ok();
            fn copy_recursive(src: &std::path::Path, dst: &std::path::Path) {
                if let Ok(rd) = std::fs::read_dir(src) {
                    for e in rd.flatten() {
                        let s = e.path();
                        let d = dst.join(e.file_name());
                        if s.is_dir() {
                            std::fs::create_dir_all(&d).ok();
                            copy_recursive(&s, &d);
                        } else if !d.exists() {
                            std::fs::copy(&s, &d).ok();
                        }
                    }
                }
            }
            copy_recursive(std::path::Path::new(&src_dir), std::path::Path::new(&dst_dir));
        }
    }

    tracing::info!("Forked session {} → {} for user {}", src_sid, new_sid, &uid[..4]);
    Json(serde_json::json!({
        "session_id": new_sid, "name": new_name, "project": src_project
    })).into_response()
}

/// Join: add user as collaborator to original shared session
async fn join_shared(Path(share_id): Path<String>, Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    // Find original session
    let src: Option<(String, String, String, String)> = db.query_row(
        "SELECT s.id, s.name, s.project, s.user_id FROM sessions s WHERE s.share_id=?1",
        [&share_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    ).ok();
    let (src_sid, src_name, src_project, src_uid) = match src {
        Some(s) => s, None => return StatusCode::NOT_FOUND.into_response(),
    };

    // If it's the owner, just return the session
    if src_uid == uid {
        return Json(serde_json::json!({
            "session_id": src_sid, "name": src_name, "project": src_project, "own": true
        })).into_response();
    }

    // Create a linked session pointing to the same project folder (shared workdir)
    // This allows both users to see/edit the same files
    let new_sid = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let collab_name = format!("{} (collab)", src_name);
    // Store original owner's project path as project so workdir resolves to the same folder
    let collab_project = format!("../{}/{}", src_uid, if src_project.is_empty() { "." } else { &src_project });
    db.execute("INSERT INTO sessions (id,user_id,name,created_at,project) VALUES (?1,?2,?3,?4,?5)",
        rusqlite::params![new_sid, uid, collab_name, now, collab_project]).ok();

    // Copy existing messages so collaborator sees history
    db.execute(
        "INSERT INTO messages (session_id,role,content,timestamp) \
         SELECT ?1,role,content,timestamp FROM messages WHERE session_id=?2 ORDER BY id",
        rusqlite::params![new_sid, src_sid]).ok();

    tracing::info!("User {} joined shared session {} as collab", &uid[..4], src_sid);
    Json(serde_json::json!({
        "session_id": new_sid, "name": collab_name, "project": collab_project
    })).into_response()
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
    let (uid, _email, credits, api_key, _plan) = user.unwrap();
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
                        "type":"no_credits"
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

                // Save user message + auto-rename session
                let auto_rename_msg = {
                    let db=state.db.lock().unwrap_or_else(|e| e.into_inner());
                    let now=chrono::Utc::now().to_rfc3339();
                    db.execute("INSERT INTO messages (session_id,role,content,timestamp) VALUES (?1,'user',?2,?3)",
                        rusqlite::params![cm.session,cm.text,now]).ok();
                    // Auto-name: if session is still "Session N", rename from first message
                    let cur_name: Option<String> = db.query_row(
                        "SELECT name FROM sessions WHERE id=?1", [&cm.session], |r| r.get(0)).ok();
                    if let Some(ref n) = cur_name {
                        if n.starts_with("Session ") || n == "New session" {
                            let auto_name: String = cm.text.chars()
                                .filter(|c| !c.is_control())
                                .take(40).collect::<String>().trim().to_string();
                            if !auto_name.is_empty() {
                                db.execute("UPDATE sessions SET name=?1 WHERE id=?2",
                                    rusqlite::params![auto_name, cm.session]).ok();
                                Some((auto_name, cm.session.clone()))
                            } else { None }
                        } else { None }
                    } else { None }
                };
                // Send rename event outside of lock
                if let Some((name, sess_id)) = auto_rename_msg {
                    let _ = ws.send(Message::Text(serde_json::json!({
                        "type":"session_renamed","name":name,"session_id":sess_id
                    }).to_string().into())).await;
                }

                // Each user gets their own isolated sandbox
                let user_sandbox = format!("{}/users/{}", state.workdir, uid);
                std::fs::create_dir_all(&user_sandbox).ok();
                let project_name = cm.project.as_deref().unwrap_or("");
                // R2: pull latest files before Claude runs
                state.storage.pull(&uid, project_name).await;
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
                                    // Detect dev server port from output
                                    if line.contains("localhost:") || line.contains("127.0.0.1:") || line.contains("0.0.0.0:") {
                                        if let Some(port) = extract_port(&line) {
                                            state.preview_ports.lock().unwrap_or_else(|e| e.into_inner())
                                                .insert(uid.clone(), port);
                                            let _ = ws.send(Message::Text(serde_json::json!({
                                                "type":"preview","port":port
                                            }).to_string().into())).await;
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

                // R2: push modified files after Claude finishes
                state.storage.push(&uid, project_name).await;

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
