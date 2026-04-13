use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Host, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Json, Response},
    routing::{delete, get, post},
    Router,
};
mod templates;
mod billing;
mod router;
mod gemini;
mod imagen;
mod veo;
mod storage;
mod nou;
mod email_templates;
mod drip;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use base64::Engine as _;
use rand::RngCore as _;
use std::{collections::HashMap, path::PathBuf, sync::{Arc, Mutex as StdMutex}};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::watch,
    time::Duration,
};

const HTML: &str = include_str!("../static/index.html");
const MANIFEST: &str = include_str!("../static/manifest.json");
const DEMO_SONG: &[u8] = include_bytes!("../static/demo-song.mp3");
const OG_PNG: &[u8] = include_bytes!("../static/og.png");
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
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS usage_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id TEXT NOT NULL,
            session_id TEXT,
            model TEXT,
            cost_usd REAL,
            created_at TEXT
        );
    ").ok();
    // ── Pro Memory ──
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS user_memory (
            user_id TEXT PRIMARY KEY,
            content TEXT NOT NULL DEFAULT '',
            updated_at TEXT NOT NULL
        );
    ").ok();
    // ── Deploy, Agents, Live ──
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS deployed_apps (
            id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL,
            slug TEXT UNIQUE NOT NULL,
            project TEXT NOT NULL,
            title TEXT DEFAULT '',
            created_at TEXT,
            FOREIGN KEY(user_id) REFERENCES users(id)
        );
        CREATE TABLE IF NOT EXISTS agent_marketplace (
            id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL,
            author TEXT NOT NULL,
            name TEXT NOT NULL,
            description TEXT DEFAULT '',
            command TEXT NOT NULL,
            project TEXT DEFAULT '',
            interval_secs INTEGER NOT NULL,
            tags TEXT DEFAULT '',
            installs INTEGER DEFAULT 0,
            created_at TEXT,
            FOREIGN KEY(user_id) REFERENCES users(id)
        );
    ").ok();
    // ── Drip email campaigns (P2 conversion) ──
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS email_campaigns (
            id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL,
            template TEXT NOT NULL,
            variant TEXT NOT NULL,
            sent_at TEXT NOT NULL,
            opened_at TEXT,
            clicked_at TEXT,
            converted_at TEXT,
            stripe_amt INTEGER DEFAULT 0,
            FOREIGN KEY(user_id) REFERENCES users(id)
        );
        CREATE INDEX IF NOT EXISTS idx_email_campaigns_user ON email_campaigns(user_id);
        CREATE INDEX IF NOT EXISTS idx_email_campaigns_template ON email_campaigns(template);
        CREATE TABLE IF NOT EXISTS email_suppression (
            email TEXT PRIMARY KEY,
            reason TEXT NOT NULL,
            created_at TEXT NOT NULL
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
    anthropic_key: Option<String>,
    nou_relay_url: String,
    nou_node_id: Option<String>,
    base_url: String,
    limiter: Arc<billing::RateLimiter>,
    active_procs: Arc<StdMutex<HashMap<String, bool>>>,
    preview_ports: Arc<StdMutex<HashMap<String, u16>>>,
    live_broadcasts: Arc<StdMutex<HashMap<String, LiveBroadcast>>>,
    oauth_states: Arc<StdMutex<HashMap<String, std::time::Instant>>>,
    encryption_key: [u8; 32], // AES-256-GCM key for encrypting user API keys
}

/// Encrypt a plaintext API key using AES-256-GCM. Stored as "enc:<base64(nonce||ciphertext)>".
fn encrypt_api_key(key: &[u8; 32], plaintext: &str) -> String {
    use aes_gcm::{Aes256Gcm, Key, Nonce, aead::{Aead, KeyInit}};
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher.encrypt(nonce, plaintext.as_bytes()).expect("api key encryption failed");
    let mut combined = nonce_bytes.to_vec();
    combined.extend_from_slice(&ciphertext);
    format!("enc:{}", base64::engine::general_purpose::STANDARD.encode(&combined))
}

/// Decrypt an AES-256-GCM encrypted API key. Returns None on failure.
/// Falls back transparently for legacy plaintext values (no "enc:" prefix).
fn decrypt_api_key(key: &[u8; 32], stored: &str) -> Option<String> {
    let encoded = match stored.strip_prefix("enc:") {
        Some(e) => e,
        None => return Some(stored.to_string()), // legacy plaintext — backward compat
    };
    use aes_gcm::{Aes256Gcm, Key, Nonce, aead::{Aead, KeyInit}};
    let combined = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
    if combined.len() <= 12 { return None; }
    let (nonce_bytes, ciphertext) = combined.split_at(12);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(nonce_bytes);
    let plaintext = cipher.decrypt(nonce, ciphertext).ok()?;
    String::from_utf8(plaintext).ok()
}

#[derive(Clone)]
struct LiveBroadcast {
    session_id: String,
    user_email: String,
    session_name: String,
    project: String,
    started_at: String,
    tx: tokio::sync::broadcast::Sender<String>,
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

fn get_user(db: &Connection, token: &str, enc_key: &[u8; 32]) -> Option<(String, String, f64, Option<String>, String)> {
    db.query_row(
        "SELECT id, email, credits, api_key, COALESCE(plan,'free') FROM users WHERE token=?1",
        [token], |r| {
            let raw_key: Option<String> = r.get(3)?;
            let api_key = raw_key.and_then(|k| decrypt_api_key(enc_key, &k));
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, api_key, r.get::<_,String>(4)?))
        }
    ).ok()
}

/// Redact secret values from Claude CLI output before sending to client
fn redact_secrets(line: &str, keys: &[(String, String)]) -> String {
    let mut output = line.to_string();
    for (_, val) in keys {
        if val.len() >= 8 && output.contains(val.as_str()) {
            let mask = format!("{}••••••••", &val[..4.min(val.len())]);
            output = output.replace(val.as_str(), &mask);
        }
    }
    // Also redact common secret patterns
    // sk-ant-*, ghp_*, fm2_*, fo1_*, AKIA*
    for prefix in &["sk-ant-", "ghp_", "fm2_", "fo1_", "AKIA"] {
        if let Some(idx) = output.find(prefix) {
            let end = output[idx..].find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '\n')
                .map(|i| idx + i).unwrap_or(output.len());
            if end - idx > 8 {
                let visible = &output[idx..idx + prefix.len() + 4.min(end - idx - prefix.len())];
                let replacement = format!("{}••••••••", visible);
                output.replace_range(idx..end, &replacement);
            }
        }
    }
    output
}

/// Verify Stripe webhook signature (HMAC-SHA256)
fn verify_stripe_signature(payload: &str, sig_header: &str, secret: &str) -> bool {
    // Parse sig header: t=timestamp,v1=signature
    let mut timestamp = "";
    let mut signature = "";
    for part in sig_header.split(',') {
        if let Some(t) = part.strip_prefix("t=") { timestamp = t; }
        if let Some(v) = part.strip_prefix("v1=") { signature = v; }
    }
    if timestamp.is_empty() || signature.is_empty() { return false; }

    // Reject old timestamps (5 min tolerance)
    let ts: i64 = timestamp.parse().unwrap_or(0);
    let now = chrono::Utc::now().timestamp();
    if (now - ts).abs() > 300 {
        tracing::warn!("Stripe webhook: timestamp too old ({} seconds)", now - ts);
        return false;
    }

    // Compute HMAC-SHA256
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let signed_payload = format!("{}.{}", timestamp, payload);
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC key");
    mac.update(signed_payload.as_bytes());

    // Compare with provided signature
    let expected = hex::encode(mac.finalize().into_bytes());
    expected == signature
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

/// Load user keys: tries KAGI Vault first, falls back to local .env
async fn load_user_keys(state: &AppState, uid: &str) -> Vec<(String, String)> {
    let mut keys = Vec::new();

    // 1. Try local .env (legacy / fallback)
    let env_path = format!("{}/users/{}/.env", state.workdir, uid);
    if let Ok(content) = std::fs::read_to_string(&env_path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            if let Some((k, v)) = line.split_once('=') {
                keys.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
    }

    // 2. Try KAGI Vault (overwrites .env keys with same name)
    let vault_url = std::env::var("KAGI_VAULT_URL")
        .unwrap_or_else(|_| "https://kagi-server.fly.dev".to_string());
    let vault_token_path = format!("{}/users/{}/.vault_token", state.workdir, uid);
    if let Ok(token) = std::fs::read_to_string(&vault_token_path) {
        let token = token.trim();
        if !token.is_empty() {
            let client = reqwest::Client::new();
            if let Ok(resp) = client.post(format!("{}/api/v1/vault/list", vault_url))
                .json(&serde_json::json!({"session_token": token}))
                .send().await
            {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if let Some(items) = body.get("items").and_then(|i| i.as_array()) {
                        for item in items {
                            if let (Some(name), Some(val)) = (
                                item.get("key_name").and_then(|n| n.as_str()),
                                item.get("encrypted_value").and_then(|v| v.as_str()),
                            ) {
                                // Note: In production, encrypted_value would need client-side
                                // decryption. For now, if the value is stored as plaintext via
                                // the ChatWeb settings UI (which encrypts on client), we use it.
                                // The real E2E flow: browser decrypts → passes to server → env var
                                keys.push((name.to_string(), val.to_string()));
                            }
                        }
                    }
                }
            }
        }
    }

    keys
}

fn auth_user(state: &AppState, token: Option<&str>) -> Option<(String, String, f64, Option<String>, String)> {
    let t = token?;
    if t.is_empty() { return None; }
    let db = state.db.lock().unwrap_or_else(|e| e.into_inner());
    get_user(&db, t, &state.encryption_key)
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
    // Load or generate AES-256 encryption key for user API keys.
    // Set ENCRYPTION_KEY env var (base64-encoded 32 bytes) to persist across restarts.
    let encryption_key: [u8; 32] = {
        if let Ok(s) = std::env::var("ENCRYPTION_KEY") {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(s.trim())
                .expect("ENCRYPTION_KEY must be base64-encoded 32 bytes");
            let mut k = [0u8; 32];
            k.copy_from_slice(&decoded[..32]);
            k
        } else {
            tracing::warn!("ENCRYPTION_KEY not set — generating ephemeral key. User API keys will be invalid after restart!");
            let mut k = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut k);
            k
        }
    };
    let state = Arc::new(AppState {
        admin_token: std::env::var("AUTH_TOKEN").ok(),
        command: std::env::var("CLAUDE_COMMAND").unwrap_or_else(|_| "claude".to_string()),
        storage: store,
        workdir: workdir.clone(), db: Arc::new(StdMutex::new(init_db(&db_path))),
        stripe_key: std::env::var("STRIPE_SECRET_KEY").ok(),
        resend_key: std::env::var("RESEND_API_KEY").ok(),
        gemini_key: std::env::var("GEMINI_API_KEY").ok(),
        anthropic_key: std::env::var("ANTHROPIC_API_KEY").ok(),
        nou_relay_url: std::env::var("NOU_RELAY_URL").unwrap_or_else(|_| "https://nou-relay.fly.dev".to_string()),
        nou_node_id: std::env::var("NOU_NODE_ID").ok(),
        base_url: std::env::var("BASE_URL").unwrap_or_else(|_| "https://chatweb.ai".to_string()),
        limiter: Arc::new(billing::RateLimiter::new(20)),
        active_procs: Arc::new(StdMutex::new(HashMap::new())),
        preview_ports: Arc::new(StdMutex::new(HashMap::new())),
        live_broadcasts: Arc::new(StdMutex::new(HashMap::new())),
        oauth_states: Arc::new(StdMutex::new(HashMap::new())),
        encryption_key,
    });
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let app = Router::new()
        .route("/", get(|_: Query<TokenQ>| async { Html(HTML) }))
        .route("/health", get(|| async { (StatusCode::OK, "ok") }))
        .route("/manifest.json", get(|| async {
            (StatusCode::OK, [("content-type","application/manifest+json")], MANIFEST)
        }))
        .route("/demo-song.mp3", get(|| async {
            (StatusCode::OK, [("content-type","audio/mpeg"),("cache-control","public, max-age=604800")], DEMO_SONG)
        }))
        .route("/demo", get(|_: Query<TokenQ>| async { Html(HTML) }))
        .route("/sitemap.xml", get(|| async {
            (StatusCode::OK, [("content-type","application/xml"),("cache-control","public, max-age=86400")],
r#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc>https://chatweb.ai/</loc><changefreq>weekly</changefreq><priority>1.0</priority></url>
  <url><loc>https://chatweb.ai/demo</loc><changefreq>monthly</changefreq><priority>0.8</priority></url>
</urlset>"#)
        }))
        .route("/robots.txt", get(|| async {
            (StatusCode::OK, [("content-type","text/plain"),("cache-control","public, max-age=86400")],
"User-agent: *\nAllow: /\nDisallow: /api/\nDisallow: /ws\nDisallow: /app/\nSitemap: https://chatweb.ai/sitemap.xml")
        }))
        .route("/og.png", get(|| async {
            (StatusCode::OK, [("content-type","image/png"),("cache-control","public, max-age=86400")], OG_PNG)
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
        // Video generation (Veo 3)
        .route("/api/video", post(generate_video))
        .route("/api/image/nanobanana", post(generate_nanobanana))
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
        // Keys (vault)
        .route("/api/keys", get(list_keys).post(save_key))
        .route("/api/keys/:name", delete(delete_key))
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
        // Usage
        .route("/api/usage", get(get_usage))
        .route("/api/memory", get(get_memory).delete(delete_memory))
        .route("/api/nou/status", get(nou_status))
        // Cron
        .route("/api/cron", get(list_crons).post(create_cron))
        .route("/api/cron/:id", delete(delete_cron))
        .route("/api/cron/:id/toggle", post(toggle_cron))
        // Public app preview
        .route("/app/:uid/:project/*path", get(serve_user_app))
        .route("/app/:uid/:project/", get(serve_user_app_index))
        .route("/app/:uid/:project", get(serve_user_app_index))
        // Deploy (subdomain hosting)
        .route("/api/deploy", post(create_deploy).get(list_deploys))
        .route("/api/deploy/:id", delete(delete_deploy))
        // Agent marketplace
        .route("/api/agents", get(list_agents))
        .route("/api/agents/publish", post(publish_agent))
        .route("/api/agents/:id/install", post(install_agent))
        // Live coding
        .route("/api/live", get(list_live))
        .route("/api/sessions/:id/broadcast", post(toggle_broadcast))
        .route("/ws/watch/:session_id", get(ws_watch_handler))
        // Gallery enhancements
        .route("/api/community/gallery/:id/like", post(like_gallery))
        .route("/api/community/gallery/:id/remix", post(remix_gallery))
        // Pair programming
        .route("/api/share/:share_id/pair", post(pair_session))
        // Time machine
        .route("/api/snapshots/:project", get(list_snapshots))
        .route("/api/snapshots/:project/revert", post(revert_snapshot))
        // Widget builder
        .route("/w/:widget_id", get(get_widget))
        .route("/api/widget/:slug/embed", get(widget_embed_info))
        // App Store
        .route("/apps", get(app_store_page))
        // Subdomain landing
        .route("/live", get(live_page))
        // WebSocket
        .route("/ws", get(ws_handler))
        .with_state(state.clone());

    // ── Background cron scheduler ──
    let cron_state = state.clone();
    tokio::spawn(async move { cron_scheduler(cron_state).await });

    // ── Subdomain router: xxxx.chatweb.ai serves deployed apps ──
    let sub_state = state.clone();
    let app = app.layer(axum::middleware::from_fn_with_state(sub_state, subdomain_middleware));
    let app = app.layer(axum::middleware::from_fn(security_headers));

    // ── IP rate limiter: /api/auth/* and /api/billing/* (30 req / 60 sec) ──
    let rate_limiter = IpRateLimiter::new(30);
    let rate_state = Arc::new(rate_limiter);
    let app = app.layer(axum::middleware::from_fn_with_state(rate_state, ip_rate_limit));

    let addr = format!("0.0.0.0:{port}");
    tracing::info!("claudeterm v6 → http://{addr}");
    axum::serve(tokio::net::TcpListener::bind(&addr).await.unwrap(), app).await.unwrap();
}

// ── Security Headers Middleware ──
async fn security_headers(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    use axum::http::header::{HeaderName, HeaderValue};
    let set = |h: &'static str, v: &'static str| {
        (HeaderName::from_static(h), HeaderValue::from_static(v))
    };
    let pairs = [
        set("x-content-type-options",   "nosniff"),
        set("x-frame-options",          "DENY"),
        set("x-xss-protection",         "0"),
        set("referrer-policy",          "strict-origin-when-cross-origin"),
        set("permissions-policy",       "camera=(), microphone=(), geolocation=()"),
        set("strict-transport-security","max-age=63072000; includeSubDomains; preload"),
        set("content-security-policy",  "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data: https:; connect-src 'self' wss: https:; frame-ancestors 'none'"),
    ];
    for (k, v) in pairs { headers.insert(k, v); }
    response
}

// ── IP Rate Limiter ──
#[derive(Clone)]
struct IpRateLimiter {
    windows: Arc<tokio::sync::Mutex<HashMap<String, Vec<std::time::Instant>>>>,
    max_per_window: usize,
}

impl IpRateLimiter {
    fn new(max_per_window: usize) -> Self {
        Self {
            windows: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            max_per_window,
        }
    }

    async fn check(&self, ip: &str) -> bool {
        let mut w = self.windows.lock().await;
        let now = std::time::Instant::now();
        let entry = w.entry(ip.to_string()).or_default();
        entry.retain(|t| now.duration_since(*t).as_secs() < 60);
        if entry.len() >= self.max_per_window {
            return false;
        }
        entry.push(now);
        true
    }
}

async fn ip_rate_limit(
    State(limiter): State<Arc<IpRateLimiter>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path().to_string();
    // Apply rate limit only to auth and billing endpoints
    if path.starts_with("/api/auth/") || path.starts_with("/api/billing/") {
        let ip = request
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .unwrap_or("unknown")
            .trim()
            .to_string();
        if !limiter.check(&ip).await {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [("retry-after", "60"), ("content-type", "application/json")],
                r#"{"error":"rate limit exceeded","retry_after":60}"#,
            )
                .into_response();
        }
    }
    next.run(request).await
}

// ── Subdomain Middleware ──
// Intercepts requests to xxxx.chatweb.ai and serves deployed app files

const BADGE_HTML: &str = r##"<div style="position:fixed;bottom:12px;right:12px;z-index:99999;font-family:system-ui;font-size:12px"><a href="https://chatweb.ai" target="_blank" rel="noopener" style="display:flex;align-items:center;gap:6px;background:rgba(9,9,11,.85);color:#a1a1aa;padding:6px 12px;border-radius:20px;text-decoration:none;border:1px solid rgba(255,255,255,.1);backdrop-filter:blur(8px)"><svg width="14" height="14" viewBox="0 0 32 32"><rect width="32" height="32" rx="8" fill="#a78bfa"/><text x="7" y="22" font-size="18" fill="white" font-weight="bold">C</text></svg>Made with ChatWeb</a></div>"##;

async fn subdomain_middleware(
    State(state): State<Arc<AppState>>,
    host: Host,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let hostname = host.0;
    // Extract subdomain: "myapp.chatweb.ai" → "myapp"
    let slug = if let Some(sub) = hostname.strip_suffix(".chatweb.ai") {
        if sub.is_empty() || sub.contains('.') { return next.run(request).await; }
        sub.to_string()
    } else if let Some(sub) = hostname.strip_suffix(".localhost") {
        // Dev mode
        if sub.is_empty() || sub.contains('.') || sub.contains(':') { return next.run(request).await; }
        sub.to_string()
    } else {
        return next.run(request).await;
    };

    // Look up deployed app
    let deploy = {
        let db = state.db.lock().unwrap_or_else(|e| e.into_inner());
        db.query_row(
            "SELECT d.user_id, d.project, d.title FROM deployed_apps d WHERE d.slug=?1",
            [&slug], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?, r.get::<_,String>(2)?))
        ).ok()
    };
    let (uid, project, _title) = match deploy {
        Some(d) => d,
        None => {
            // No deployed app — show a nice 404
            return Html(format!(r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>Not Found</title>
<style>body{{background:#09090b;color:#f4f4f5;font-family:system-ui;display:flex;align-items:center;justify-content:center;height:100vh;margin:0}}
.c{{text-align:center}}h1{{font-size:48px;opacity:.3}}p{{color:#a1a1aa;margin:12px 0}}a{{color:#a78bfa}}</style></head>
<body><div class="c"><h1>404</h1><p><b>{slug}.chatweb.ai</b> is not deployed yet.</p>
<a href="https://chatweb.ai">Create your own app on ChatWeb →</a></div></body></html>"#)).into_response();
        }
    };

    // Serve the file from the user's project directory
    let path = request.uri().path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    // Security checks
    if path.contains("..") || is_sensitive_file(path) {
        return StatusCode::FORBIDDEN.into_response();
    }

    let file_path = PathBuf::from(format!("{}/users/{}/{}/{}", state.workdir, uid, project, path));
    let base = PathBuf::from(format!("{}/users/{}/{}", state.workdir, uid, project));

    // Try exact path, then index.html for SPA routing
    let resolved = if file_path.exists() {
        file_path
    } else {
        // SPA fallback: serve index.html for non-file paths
        base.join("index.html")
    };

    match (resolved.canonicalize(), base.canonicalize()) {
        (Ok(r), Ok(b)) if r.starts_with(&b) => {
            match std::fs::read(&r) {
                Ok(data) => {
                    let ct = content_type_for(&r.to_string_lossy());
                    if ct.contains("text/html") {
                        let html = String::from_utf8_lossy(&data);
                        let injected = if html.contains("</body>") {
                            html.replace("</body>", &format!("{BADGE_HTML}</body>"))
                        } else {
                            format!("{html}{BADGE_HTML}")
                        };
                        return (StatusCode::OK, [
                            ("content-type", ct),
                            ("access-control-allow-origin", "*"),
                            ("cache-control", "public, max-age=60"),
                        ], injected).into_response();
                    }
                    (StatusCode::OK, [
                        ("content-type", ct),
                        ("access-control-allow-origin", "*"),
                        ("cache-control", "public, max-age=300"),
                    ], data).into_response()
                }
                Err(_) => StatusCode::NOT_FOUND.into_response(),
            }
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

// ── Email ────────────────────────────────────────────────────────────────────

/// Send an HTML email via Resend. When `campaign_id` is Some, the caller has
/// already recorded an email_campaigns row; this function does not touch the
/// DB. In dev mode (no RESEND_API_KEY) the email is logged instead of sent.
pub(crate) async fn send_email(
    state: &Arc<AppState>,
    to: &str,
    subject: &str,
    html: &str,
    campaign_id: Option<&str>,
) -> Result<(), String> {
    let Some(key) = state.resend_key.as_ref() else {
        tracing::info!(
            "send_email (dev): to={} subject={:?} campaign={:?}",
            to, subject, campaign_id
        );
        return Ok(());
    };
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "from": "chatweb.ai <noreply@chatweb.ai>",
        "to": [to],
        "subject": subject,
        "html": html,
    });
    let resp = client
        .post("https://api.resend.com/emails")
        .header("Authorization", format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("resend request: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        return Err(format!("resend {}: {}", status, txt));
    }
    tracing::info!("send_email ok: to={} campaign={:?}", to, campaign_id);
    Ok(())
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
    if s.resend_key.is_some() {
        let subject = format!("Your login code: {}", code);
        let html = format!(
            "<div style='font-family:system-ui;max-width:400px;margin:40px auto;padding:32px;background:#09090b;border-radius:16px;border:1px solid #27272a'>\
            <div style='width:48px;height:48px;border-radius:12px;background:linear-gradient(135deg,#a78bfa,#60a5fa);margin-bottom:24px'></div>\
            <h2 style='color:#fafafa;font-size:22px;margin:0 0 8px'>Your login code</h2>\
            <p style='color:#a1a1aa;font-size:14px;margin:0 0 24px'>Enter this code to sign in to chatweb.ai</p>\
            <div style='font-size:36px;font-weight:700;letter-spacing:8px;color:#a78bfa;background:#18181b;padding:20px;border-radius:12px;text-align:center'>{}</div>\
            <p style='color:#52525b;font-size:12px;margin:20px 0 0'>Expires in 10 minutes. If you didn't request this, ignore this email.</p>\
            </div>", code);
        if let Err(e) = send_email(&s, &email, &subject, &html, None).await {
            tracing::warn!("OTP send failed for {email}: {e}");
        } else {
            tracing::info!("OTP sent to {email}");
        }
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
    // Generate CSRF state token, store with 5-minute expiry
    let state_token = uuid::Uuid::new_v4().to_string().replace("-", "");
    let expiry = std::time::Instant::now() + std::time::Duration::from_secs(300);
    {
        let mut states = s.oauth_states.lock().unwrap_or_else(|e| e.into_inner());
        // Prune expired states
        let now = std::time::Instant::now();
        states.retain(|_, exp| *exp > now);
        states.insert(state_token.clone(), expiry);
    }
    let redirect_uri = google_redirect_uri(&s.base_url);
    let url = format!(
        "https://accounts.google.com/o/oauth2/v2/auth\
        ?client_id={}\
        &redirect_uri={}\
        &response_type=code\
        &scope=email+profile\
        &access_type=offline\
        &prompt=select_account\
        &state={}",
        urlencoding::encode(&client_id),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(&state_token)
    );
    axum::response::Redirect::temporary(&url).into_response()
}

#[derive(Deserialize)]
struct OAuthCallbackQ { code: Option<String>, error: Option<String>, state: Option<String> }

async fn google_oauth_callback(
    Query(q): Query<OAuthCallbackQ>,
    State(s): State<Arc<AppState>>,
) -> Response {
    if let Some(err) = q.error {
        return axum::response::Redirect::temporary(&format!("/?oauth_error={}", urlencoding::encode(&err))).into_response();
    }
    // Validate CSRF state
    let state_token = match q.state {
        Some(st) => st,
        None => return axum::response::Redirect::temporary("/?oauth_error=missing_state").into_response(),
    };
    {
        let mut states = s.oauth_states.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        match states.remove(&state_token) {
            Some(expiry) if expiry > now => {} // valid
            _ => return axum::response::Redirect::temporary("/?oauth_error=invalid_state").into_response(),
        }
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

    // Rate limit OTP verification (max 5 per email per 10 minutes)
    if !s.limiter.check(&format!("otp:{}", email)) {
        return (StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({"error":"Too many attempts. Please wait."}))).into_response();
    }

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
            // Constant-time comparison to prevent timing attacks
            let code_match = stored_code.len() == code.len()
                && stored_code.bytes().zip(code.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;
            if expires > now && code_match {
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
    let encrypted: Option<String> = if key.is_empty() {
        None
    } else {
        Some(encrypt_api_key(&s.encryption_key, key))
    };
    db.execute("UPDATE users SET api_key=?1 WHERE id=?2", rusqlite::params![encrypted, uid]).ok();
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
    if is_sensitive_file(&rel) { return StatusCode::FORBIDDEN.into_response(); }
    let file = base.join(&rel);
    if !file.starts_with(&base) { return StatusCode::FORBIDDEN.into_response(); }
    // Resolve symlinks and verify still within base
    if let Ok(resolved) = file.canonicalize() {
        if let Ok(base_resolved) = base.canonicalize() {
            if !resolved.starts_with(&base_resolved) { return StatusCode::FORBIDDEN.into_response(); }
        }
    }
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
- **NEVER expose secrets**: Do not run `printenv`, `env`, `echo $KEY`, `cat .env` or similar commands that would display secret values. Use keys silently via environment variables.
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

## Deploy (IMPORTANT — MUST follow)
This project runs on **ChatWeb** (chatweb.ai). Multiple deploy targets are available:

### 1. ChatWeb Subdomain (instant, no config needed)
- Deploy via the Deploy menu → project goes live at `https://SLUG.chatweb.ai`
- If already deployed, always mention the URL at the end of your response

### 2. Fly.io — full-stack apps, Docker, databases
- Token: `FLY_API_TOKEN` in Keys | Commands: `fly launch`, `fly deploy --remote-only`

### 3. Vercel — frontend, Next.js, static sites
- Token: `VERCEL_TOKEN` in Keys | Command: `vercel --yes --token $VERCEL_TOKEN`

### 4. Cloudflare — Workers, Pages, edge functions
- Token: `CLOUDFLARE_API_TOKEN` in Keys | Command: `wrangler deploy`

### 5. Netlify — static sites, serverless, forms
- Token: `NETLIFY_AUTH_TOKEN` in Keys | Command: `netlify deploy --prod --dir .`

### 6. Railway — full-stack, databases, cron
- Token: `RAILWAY_TOKEN` in Keys | Command: `railway up`

### 7. Supabase — backend, auth, database, edge functions
- Token: `SUPABASE_ACCESS_TOKEN` in Keys | Command: `supabase functions deploy`

### Rules
- **RULE**: Every time you create or modify web files, you MUST end your response with the deploy URL or deploy instructions.
- If the project is already deployed to a subdomain, say: "サイトを更新しました！こちらで確認できます: https://SLUG.chatweb.ai"
- If not yet deployed, list ALL available deploy options based on the user's configured tokens.
- Example: "アプリが完成しました！デプロイ方法:\n  1. メニューの Deploy → my-app.chatweb.ai (即公開)\n  2. `fly deploy` → Fly.io\n  3. `vercel --yes` → Vercel\n  4. `wrangler deploy` → Cloudflare\n  5. `netlify deploy --prod` → Netlify\n  6. `railway up` → Railway"
- NEVER skip this. ALWAYS include deploy options.

## KAGI Smart Home Integration
If the user has `KAGI_AUTH_TOKEN` in their Keys, they can interact with KAGI smart home system:
- **KAGI Server**: `https://kagi-server.fly.dev` (or user's custom KAGI_SERVER_URL)
- **Vault sync**: KAGI iOS app syncs API keys to ChatWeb via 6-digit transfer code (Settings → Keys → "受け取る")
- **Available APIs** (all require Authorization: Bearer $KAGI_AUTH_TOKEN):
  - `POST /api/v1/vault/list` — list synced secrets
  - `POST /api/v1/vault/get` body: `{{"key_name":"..."}}` — get a specific secret
  - `GET /api/v1/family/:token/status` — get family/property status
  - `POST /api/v1/beds24/register` — register Beds24 PMS integration
- **Smart lock control**: Use SwitchBot API directly (token synced via vault):
  - `POST https://api.switch-bot.com/v1.1/devices/:id/commands` with `{{"command":"turnOn"}}` (unlock) or `{{"command":"turnOff"}}` (lock)
  - Header: `Authorization: $SWITCHBOT_TOKEN`, `Content-Type: application/json`
- **Reservation data**: If `BEDS24_API_KEY` is in Keys, use Beds24 API v2:
  - `GET https://api.beds24.com/v2/bookings?arrivalFrom=TODAY` — today's check-ins
  - Header: `token: $BEDS24_API_KEY`
- When the user asks about properties, reservations, or smart devices, use these APIs with curl.

## GitHub Integration
If the user has `GITHUB_TOKEN` in Keys:
- `gh auth login --with-token` is already configured
- Create repos: `gh repo create PROJECT --public --source .`
- Push code: `git init && git add . && git commit -m "init" && git push -u origin main`
- Create PRs: `gh pr create --title "..." --body "..."`
- After pushing to GitHub, Vercel/Netlify auto-deploy if connected

## Database Integration
If the user wants a database:
- **Supabase**: If `SUPABASE_ACCESS_TOKEN` is set, use `supabase init` → `supabase db push`
- **Turso (SQLite)**: If `TURSO_AUTH_TOKEN` is set, use `turso db create` → provide connection URL
- **PlanetScale**: If `PLANETSCALE_TOKEN` is set, use their CLI
- For simple apps, suggest SQLite (built into most runtimes, zero config)
- Always write `.env` with connection strings, never hardcode

## Fork the Internet
If the user pastes a URL and says "make something like this" or "clone this design":
1. Use `curl -s URL` to fetch the HTML
2. Analyze the structure, design patterns, and layout
3. Recreate a clean version using modern HTML/CSS (don't copy verbatim — create inspired-by version)
4. Deploy to their subdomain
Example: "stripe.comみたいなランディング作って" → fetch → analyze → recreate → deploy

## KAGI Autopilot (Smart Home Automation)
If the user wants to automate property management:
- **Morning routine cron**: "毎朝8時に今日のチェックイン一覧を取得してTelegramに送信"
  - curl Beds24 API → format → post to Telegram bot
- **Auto-unlock before check-in**: "チェックイン30分前に自動解錠"
  - Cron every 15min → check Beds24 arrivals → SwitchBot unlock if within 30min
- **Cleaning dispatch**: "チェックアウト後に清掃チームにLINE通知"
  - Cron → check departures → send LINE/Telegram notification
- **Guest auto-reply**: FAQ answers (WiFi password, check-out time, etc.)
These can all be built as Cron jobs. Guide the user to set up the tokens and cron schedules.

## Gmail Integration
If the user wants to check email, read invoices, or take action on messages, use the Gmail REST API via curl.

### Get access token (do this first, every time)
```bash
GMAIL_TOKEN=$(curl -s -X POST "https://oauth2.googleapis.com/token" \
  -d "client_id=$GMAIL_CLIENT_ID&client_secret=$GMAIL_CLIENT_SECRET&refresh_token=$GMAIL_REFRESH_TOKEN&grant_type=refresh_token" \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['access_token'])")
```

### List unread emails
```bash
curl -s "https://gmail.googleapis.com/gmail/v1/users/me/messages?labelIds=INBOX&q=is:unread+-category:promotions&maxResults=20" \
  -H "Authorization: Bearer $GMAIL_TOKEN" \
  | python3 -c "
import json,sys,subprocess
data = json.load(sys.stdin)
for m in data.get('messages',[]):
    r = subprocess.run(['curl','-s',f'https://gmail.googleapis.com/gmail/v1/users/me/messages/{{m["id"]}}?format=metadata&metadataHeaders=From&metadataHeaders=Subject&metadataHeaders=Date','-H','Authorization: Bearer '+os.environ.get('GMAIL_TOKEN','')],capture_output=True,text=True)
    d = json.loads(r.stdout)
    h = {{x['name']:x['value'] for x in d['payload']['headers']}}
    print(h.get('Date','')[:16], '|', h.get('From','')[:30], '|', h.get('Subject','')[:50])
"
```

### Get a specific message (full body)
```bash
MSG_ID="MESSAGE_ID_HERE"
curl -s "https://gmail.googleapis.com/gmail/v1/users/me/messages/$MSG_ID?format=full" \
  -H "Authorization: Bearer $GMAIL_TOKEN" \
  | python3 -c "
import json,sys,base64,re
d=json.load(sys.stdin)
def get_body(p):
    if p.get('body',{{}}).get('data'): return base64.urlsafe_b64decode(p['body']['data']).decode('utf-8','replace')
    for pp in p.get('parts',[]):
        r=get_body(pp)
        if r: return r
    return ''
body=get_body(d['payload'])
text=re.sub(r'<[^>]+>',' ',body); text=re.sub(r'\s+',' ',text).strip()
print(text[:2000])
"
```

### Search emails
```bash
# 請求書を探す
curl -s "https://gmail.googleapis.com/gmail/v1/users/me/messages?q=subject:請求書+OR+subject:invoice+is:unread&maxResults=10" \
  -H "Authorization: Bearer $GMAIL_TOKEN"
```

### Send a reply or draft
```bash
# Base64エンコードしてsend
ENCODED=$(echo -e "To: TARGET@example.com\nSubject: Re: ...\n\n本文ここ" | base64 -w 0 | tr '+/' '-_')
curl -s -X POST "https://gmail.googleapis.com/gmail/v1/users/me/messages/send" \
  -H "Authorization: Bearer $GMAIL_TOKEN" -H "Content-Type: application/json" \
  -d '{{"raw":"'"$ENCODED"'"}}'
```

When user says "メール見て", "メールチェック", "未読確認": automatically run the unread list command above and summarize in Japanese. Identify: invoices (請求書), App Store notifications, CI failures, and other action items.

## Anime & Video Creation (Veo 3 + Nano Banana)
ChatWeb has built-in video/image generation. The user can create anime, short films, and music videos.

### Available Models
1. **Nano Banana** (`/api/image/nanobanana`) — Ultra-fast image gen for character sheets, storyboards, backgrounds
2. **Gemini Image** (`/api/image`) — Higher quality images via gemini-3-pro-image-preview
3. **Veo 3** (`/api/video`) — 8-second cinematic video clips WITH audio, dialogue, and music

### Workflow for Creating Anime/Film
1. **Character Design**: Use Nano Banana to generate character reference sheets
   - `curl -X POST /api/image/nanobanana -d '{{"token":"...","prompt":"anime character sheet, young woman with blue hair..."}}' `
2. **Storyboard**: Generate key frames for each scene
3. **Video Clips**: Use Veo 3 to generate 8-second clips with dialogue
   - `curl -X POST /api/video -d '{{"token":"...","prompt":"cinematic anime scene...","duration":8}}' `
4. **Editing**: Use ffmpeg to concatenate clips, add music, subtitles
   - `ffmpeg -f concat -i clips.txt -c:v copy output.mp4`

### Veo 3 Prompt Tips
- Always describe: camera angle, lighting, character appearance, action, dialogue
- For anime: "anime style, cel-shaded, vibrant colors, Studio Ghibli quality"
- Include dialogue in quotes: `The character says "こんにちは"`
- Veo 3 generates audio including voice, music, and sound effects
- Use reference images for character consistency across clips

### Python Script Pattern (for multi-clip productions)
```python
import os, time
from google import genai
from google.genai import types
client = genai.Client(api_key=os.environ['GEMINI_API_KEY'])

def gen_clip(prompt, outpath, duration=8):
    op = client.models.generate_videos(
        model='veo-3.0-generate-001', prompt=prompt,
        config=types.GenerateVideosConfig(aspect_ratio='16:9', duration_seconds=duration, number_of_videos=1))
    t=0
    while not op.done:
        time.sleep(15); t+=15; op=client.operations.get(op)
        if t>600: return False
    if op.response and op.response.generated_videos:
        data=client.files.download(file=op.response.generated_videos[0].video)
        open(outpath,'wb').write(data); return True
    return False
```

### Concatenation & Post-production
```bash
# clips.txt: file 'clips/scene1.mp4'\nfile 'clips/scene2.mp4'\n...
ffmpeg -f concat -safe 0 -i clips.txt -c copy joined.mp4
# Add subtitles (ASS format for styled text)
ffmpeg -i joined.mp4 -vf "ass=subs.ass" -c:a copy final.mp4
# Add background music
ffmpeg -i final.mp4 -i bgm.mp3 -filter_complex "[0:a][1:a]amix=inputs=2:duration=shortest" -c:v copy output.mp4
```

## Web Site Creation Guide
When the user asks to create a website or landing page:
1. **ALWAYS create a single index.html** with embedded CSS and JS (no build step needed)
2. Use modern design: dark theme, gradients, glassmorphism, subtle animations
3. Make it responsive (mobile-first, flexbox/grid)
4. Include SEO meta tags (title, description, og:image, twitter:card)
5. After creating, remind: "Deploy メニューからサブドメインにデプロイできます"
6. If the project is already deployed, changes are **instantly live** — remind the URL

### Design Patterns to Follow
- Hero section with large text + CTA button
- Feature cards with icons (3-4 column grid)
- Smooth scroll with `scroll-behavior: smooth`
- Dark background: `#09090b` to `#1a1040` gradient
- Accent: purple `#a78bfa` to blue `#60a5fa`
- Font: `system-ui, -apple-system, sans-serif`
- Border radius: `12-16px` for cards, `8-12px` for buttons
- Subtle `backdrop-filter: blur()` for glass effect

## Document & Presentation Creation
When the user asks to create reports, documents, slides, or presentations:
1. Create as HTML (single file, printable)
2. Use `@media print` CSS for clean PDF export
3. Include charts using inline SVG or CSS (no JS libraries needed)
4. For slides: create HTML with CSS `scroll-snap` for slide-like navigation
5. For PDFs: user can Cmd+P / Ctrl+P from the deployed URL

## What You Can Do (Capabilities Summary)
Tell users they can:
- **Build websites** → chat to create, deploy to subdomain instantly
- **Create anime/films** → Veo 3 generates 8s video clips with audio + dialogue
- **Design images** → Nano Banana for fast generation, Gemini for quality
- **Generate documents** → HTML-based reports, slides, charts
- **Deploy anywhere** → ChatWeb, Fly.io, Vercel, Cloudflare, Netlify, Railway
- **Control smart home** → KAGI integration (SwitchBot, Beds24, Hue)
- **Automate tasks** → Cron jobs for scheduled AI operations
- **Collaborate** → Share URLs for pair programming
- **Version control** → Time Machine auto-snapshots every AI turn
- **Embed anywhere** → Widget embed code for any deployed app

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

/// Blocked filenames/extensions in public app preview
fn is_sensitive_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.contains(".env") || lower.contains(".git") || lower.contains("node_modules")
        || lower.ends_with(".pem") || lower.ends_with(".key") || lower.ends_with(".p8")
        || lower.ends_with(".p12") || lower.contains("secret") || lower.contains("credential")
        || lower.contains(".vault_token") || lower.contains("id_rsa") || lower.contains("id_ed25519")
}

async fn serve_user_app(
    Path((uid, project, path)): Path<(String, String, String)>,
    State(s): State<Arc<AppState>>,
) -> Response {
    // Block directory traversal
    if uid.contains("..") || project.contains("..") || path.contains("..") {
        return StatusCode::FORBIDDEN.into_response();
    }
    // Block sensitive files
    if is_sensitive_file(&path) || is_sensitive_file(&project) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let file_path = if path.is_empty() || path == "/" {
        PathBuf::from(format!("{}/users/{}/{}/index.html", s.workdir, uid, project))
    } else {
        PathBuf::from(format!("{}/users/{}/{}/{}", s.workdir, uid, project, path))
    };
    // Canonicalize and verify path stays within project dir
    let base = PathBuf::from(format!("{}/users/{}/{}", s.workdir, uid, project));
    match (file_path.canonicalize(), base.canonicalize()) {
        (Ok(resolved), Ok(base_resolved)) => {
            if !resolved.starts_with(&base_resolved) {
                return StatusCode::FORBIDDEN.into_response();
            }
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
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

// ── Key Management ──

async fn list_keys(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let env_path = format!("{}/users/{}/.env", s.workdir, uid);
    let mut keys = Vec::new();
    if let Ok(content) = std::fs::read_to_string(&env_path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            if let Some((k, _)) = line.split_once('=') {
                keys.push(serde_json::json!({"name": k.trim(), "masked": "••••••••"}));
            }
        }
    }
    Json(serde_json::json!({"keys": keys})).into_response()
}

#[derive(Deserialize)] struct SaveKeyReq { name: String, value: String }

async fn save_key(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    Json(body): Json<SaveKeyReq>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    // Validate key name (only uppercase letters, digits, underscore)
    let name: String = body.name.chars()
        .filter(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
        .collect();
    if name.is_empty() || body.value.is_empty() {
        return (StatusCode::BAD_REQUEST, "Invalid key").into_response();
    }
    // Read existing .env, update or append
    let env_path = format!("{}/users/{}/.env", s.workdir, uid);
    std::fs::create_dir_all(format!("{}/users/{}", s.workdir, uid)).ok();
    let mut lines: Vec<String> = std::fs::read_to_string(&env_path)
        .unwrap_or_default().lines().map(|l| l.to_string()).collect();
    let mut found = false;
    for line in &mut lines {
        if line.starts_with(&format!("{}=", name)) {
            *line = format!("{}={}", name, body.value);
            found = true;
            break;
        }
    }
    if !found { lines.push(format!("{}={}", name, body.value)); }
    std::fs::write(&env_path, lines.join("\n") + "\n").ok();
    // Set restrictive permissions
    #[cfg(unix)]
    { use std::os::unix::fs::PermissionsExt;
      std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600)).ok(); }
    Json(serde_json::json!({"ok": true})).into_response()
}

async fn delete_key(Path(name): Path<String>, Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let env_path = format!("{}/users/{}/.env", s.workdir, uid);
    if let Ok(content) = std::fs::read_to_string(&env_path) {
        let filtered: Vec<&str> = content.lines()
            .filter(|l| !l.starts_with(&format!("{}=", name)))
            .collect();
        std::fs::write(&env_path, filtered.join("\n") + "\n").ok();
    }
    StatusCode::NO_CONTENT.into_response()
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

async fn get_usage(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    let total: f64 = db.query_row(
        "SELECT COALESCE(SUM(cost_usd),0) FROM usage_log WHERE user_id=?1", [&uid], |r| r.get(0)
    ).unwrap_or(0.0);
    let month_start = chrono::Utc::now().format("%Y-%m-01").to_string();
    let month_total: f64 = db.query_row(
        "SELECT COALESCE(SUM(cost_usd),0) FROM usage_log WHERE user_id=?1 AND created_at>=?2",
        rusqlite::params![uid, month_start], |r| r.get(0)
    ).unwrap_or(0.0);
    let mut stmt = db.prepare(
        "SELECT created_at, model, cost_usd FROM usage_log WHERE user_id=?1 ORDER BY id DESC LIMIT 30"
    ).unwrap();
    let entries: Vec<serde_json::Value> = stmt.query_map([&uid], |r| {
        Ok(serde_json::json!({
            "date": r.get::<_,String>(0)?, "model": r.get::<_,String>(1)?, "cost": r.get::<_,f64>(2)?
        }))
    }).unwrap().filter_map(|r| r.ok()).collect();
    Json(serde_json::json!({ "total": total, "month_total": month_total, "entries": entries })).into_response()
}

async fn gallery(State(s): State<Arc<AppState>>) -> Response {
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = db.prepare(
        "SELECT id,author,project,title,description,tags,likes,created_at,user_id FROM gallery ORDER BY created_at DESC LIMIT 50"
    ).unwrap();
    let items: Vec<serde_json::Value> = st.query_map([], |r| {
        let gallery_id: String = r.get(0)?;
        Ok(serde_json::json!({
            "id": gallery_id, "author": r.get::<_,String>(1)?,
            "title": r.get::<_,String>(3)?,
            "description": r.get::<_,String>(4)?, "tags": r.get::<_,String>(5)?,
            "likes": r.get::<_,i64>(6)?, "created_at": r.get::<_,String>(7)?,
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
    if is_sensitive_file(&body.path) { return StatusCode::FORBIDDEN.into_response(); }
    let file = base.join(&body.path);
    if !file.starts_with(&base) { return StatusCode::FORBIDDEN.into_response(); }
    if let Some(parent) = file.parent() { std::fs::create_dir_all(parent).ok(); }
    // Check resolved path after parent creation
    if let (Ok(resolved), Ok(base_r)) = (file.parent().unwrap_or(&file).canonicalize(), base.canonicalize()) {
        if !resolved.starts_with(&base_r) { return StatusCode::FORBIDDEN.into_response(); }
    }
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

async fn stripe_webhook(headers: HeaderMap, State(s): State<Arc<AppState>>, body: String) -> Response {
    // STRIPE_WEBHOOK_SECRET must be set; if missing, refuse to process
    let wh_secret = match std::env::var("STRIPE_WEBHOOK_SECRET").ok().filter(|v| !v.is_empty()) {
        Some(s) => s,
        None => {
            tracing::error!("Stripe webhook: STRIPE_WEBHOOK_SECRET is not configured");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let sig = headers.get("stripe-signature").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !verify_stripe_signature(&body, sig, &wh_secret) {
        tracing::warn!("Stripe webhook: invalid signature");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    match billing::parse_webhook_action(&body) {
        Some(billing::WebhookAction::OneTimeCredits { token, credits }) => {
            db.execute("UPDATE users SET credits = credits + ?1 WHERE token = ?2",
                rusqlite::params![credits, token]).ok();
            tracing::info!("One-time +${} credits", credits);
            // ── Drip conversion attribution ──
            // Look up the user by token, find their most-recent unconverted
            // campaign and mark it converted with the $amount (cents).
            let uid: Option<String> = db.query_row(
                "SELECT id FROM users WHERE token=?1", [&token], |r| r.get(0)
            ).ok();
            if let Some(uid) = uid {
                let now = chrono::Utc::now().to_rfc3339();
                let amt_cents = (credits * 100.0) as i64;
                db.execute(
                    "UPDATE email_campaigns \
                     SET converted_at = ?1, stripe_amt = stripe_amt + ?2 \
                     WHERE id = ( \
                       SELECT id FROM email_campaigns \
                       WHERE user_id=?3 AND converted_at IS NULL \
                       ORDER BY sent_at DESC LIMIT 1 \
                     )",
                    rusqlite::params![now, amt_cents, uid],
                ).ok();
            }
        }
        Some(billing::WebhookAction::SubscriptionStarted { token, plan, customer_id }) => {
            let credits = billing::plan_credits(&plan);
            db.execute(
                "UPDATE users SET credits = credits + ?1, plan = ?2, stripe_customer_id = ?3 WHERE token = ?4",
                rusqlite::params![credits, plan, customer_id, token]).ok();
            tracing::info!("Subscription {} started: +${} credits", plan, credits);
            // Drip conversion attribution (subscription start)
            let uid: Option<String> = db.query_row(
                "SELECT id FROM users WHERE token=?1", [&token], |r| r.get(0)
            ).ok();
            if let Some(uid) = uid {
                let now = chrono::Utc::now().to_rfc3339();
                let amt_cents = (credits * 100.0) as i64;
                db.execute(
                    "UPDATE email_campaigns \
                     SET converted_at = ?1, stripe_amt = stripe_amt + ?2 \
                     WHERE id = ( \
                       SELECT id FROM email_campaigns \
                       WHERE user_id=?3 AND converted_at IS NULL \
                       ORDER BY sent_at DESC LIMIT 1 \
                     )",
                    rusqlite::params![now, amt_cents, uid],
                ).ok();
            }
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

async fn billing_success() -> Html<&'static str> {
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
    let chat_id = std::env::var("TELEGRAM_CHAT_ID").unwrap_or_default();
    let emoji = match body.category.as_str() { "bug" => "🐛", "feature" => "💡", _ => "💬" };
    let text = format!(
        "{} *Feedback — {}*\nFrom: `{}`\n\n{}\n\n_{}_",
        emoji, body.category, email, body.message, now
    );
    if !bot_token.is_empty() && !chat_id.is_empty() {
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
#[derive(Deserialize)]
struct RedirectQ { to: Option<String> }

/// Unified /r/:id handler. First tries to match an email_campaigns.id
/// (drip click tracker). If no campaign is found, falls back to the
/// original referral-code behaviour (/r/:code → /?ref=CODE).
async fn referral_redirect(
    Path(code): Path<String>,
    Query(q): Query<RedirectQ>,
    State(s): State<Arc<AppState>>,
) -> Response {
    // 1. Email-campaign click tracker
    let is_campaign = {
        let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
        let exists: Option<i64> = db
            .query_row(
                "SELECT 1 FROM email_campaigns WHERE id=?1",
                [&code],
                |r| r.get(0),
            )
            .ok();
        if exists.is_some() {
            let now = chrono::Utc::now().to_rfc3339();
            db.execute(
                "UPDATE email_campaigns SET clicked_at=COALESCE(clicked_at,?1) WHERE id=?2",
                rusqlite::params![now, code],
            )
            .ok();
        }
        exists.is_some()
    };

    if is_campaign {
        // Allow arbitrary internal paths via ?to=/path, default /topup
        let to = q.to.unwrap_or_else(|| "/topup".to_string());
        // Safety: only allow internal paths (starts with '/')
        let target = if to.starts_with('/') {
            format!("{}{}", s.base_url, to)
        } else {
            format!("{}/topup", s.base_url)
        };
        return axum::response::Redirect::to(&target).into_response();
    }

    // 2. Fallback: referral code
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

fn validate_cron_command(cmd: &str) -> Result<(), &'static str> {
    if cmd.len() > 2000 {
        return Err("コマンドが長すぎます（最大2000文字）");
    }
    // Block shell patterns that could escape the sandbox or cause system damage
    let dangerous = [
        "rm -rf /", "rm -fr /",
        ":(){ :|:& };:", // fork bomb
        "mkfs", "dd if=/dev/",
        "> /dev/sd", "chmod -R 777 /", "chmod 777 /",
        "shutdown", "reboot", "halt",
        "--dangerously-skip-permissions", // must not be injected into claude args
        "/etc/passwd", "/etc/shadow",
    ];
    let lower = cmd.to_lowercase();
    for pattern in &dangerous {
        if lower.contains(&pattern.to_lowercase()) {
            return Err("危険なコマンドパターンが検出されました");
        }
    }
    Ok(())
}

async fn create_cron(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    Json(body): Json<CronCreateReq>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if body.command.trim().is_empty() || body.name.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "name and command required").into_response();
    }
    if let Err(e) = validate_cron_command(body.command.trim()) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok": false, "error": e}))).into_response();
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

/// Background scheduler: checks every 30s for due cron jobs, and
/// every ~5 minutes runs the trial→paid drip email tick.
async fn cron_scheduler(state: Arc<AppState>) {
    let mut drip_ticks: u64 = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;

        // ── Drip emails (every 10 * 30s = 5 min) ──
        drip_ticks = drip_ticks.wrapping_add(1);
        if drip_ticks % 10 == 0 {
            if let Err(e) = drip::drip_tick(&state).await {
                tracing::warn!("drip_tick error: {e}");
            }
        }

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
                // Load user keys for cron
                let vault_keys = load_user_keys(&state, &uid).await;
                for (k, v) in &vault_keys { cmd.env(k, v); }

                let result = match cmd.output().await {
                    Ok(output) => {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        // Extract assistant text and cost from stream-json
                        let mut text = String::new();
                        let mut cost_usd: f64 = 0.0;
                        for line in stdout.lines() {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                                match v.get("type").and_then(|t| t.as_str()) {
                                    Some("assistant") => {
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
                                    Some("result") => {
                                        if let Some(c) = v.get("cost_usd").and_then(|c| c.as_f64()) {
                                            cost_usd = c;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        if text.is_empty() { text = "(no output)".to_string(); }
                        (if output.status.success() { "success" } else { "error" }, text, cost_usd)
                    }
                    Err(e) => ("error", format!("Failed to run: {}", e), 0.0),
                };

                // Deduct credits for cron run
                if result.2 > 0.0 {
                    let charge = result.2 * COST_MULTIPLIER;
                    let db = state.db.lock().unwrap_or_else(|e| e.into_inner());
                    db.execute("UPDATE users SET credits = credits - ?1 WHERE id = ?2",
                        rusqlite::params![charge, uid]).ok();
                    db.execute("INSERT INTO usage_log (user_id,session_id,model,cost_usd,created_at) VALUES (?1,'cron','claude-haiku-4-5-20251001',?2,?3)",
                        rusqlite::params![uid, charge, chrono::Utc::now().to_rfc3339()]).ok();
                }

                // Update result
                let db = state.db.lock().unwrap_or_else(|e| e.into_inner());
                let truncated = if result.1.len() > 2000 { &result.1[..2000] } else { &result.1 };
                db.execute("UPDATE cron_jobs SET last_status=?1, last_result=?2 WHERE id=?3",
                    rusqlite::params![result.0, truncated, job_id]).ok();
                tracing::info!("Cron [{}] done: {} (cost: ${:.6})", job_id, result.0, result.2);
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
    let session: Option<(String, String, String, String)> = db.query_row(
        "SELECT s.id, s.name, u.email, COALESCE(s.project,'') FROM sessions s JOIN users u ON s.user_id=u.id WHERE s.share_id=?1",
        [&share_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    ).ok();
    let (sid, name, email, project) = match session {
        Some(s) => s, None => return StatusCode::NOT_FOUND.into_response(),
    };
    // Check if this project has a deployed URL
    let deploy_url: Option<String> = if !project.is_empty() {
        db.query_row(
            "SELECT slug FROM deployed_apps WHERE project=?1 AND user_id=(SELECT user_id FROM sessions WHERE id=?2)",
            rusqlite::params![project, sid], |r| {
                let slug: String = r.get(0)?;
                Ok(format!("https://{}.chatweb.ai", slug))
            }
        ).ok()
    } else { None };
    let mut st = db.prepare("SELECT role,content,timestamp FROM messages WHERE session_id=?1 ORDER BY id").unwrap();
    let msgs: Vec<serde_json::Value> = st.query_map([&sid], |r| Ok(serde_json::json!({
        "role":r.get::<_,String>(0)?,"content":r.get::<_,String>(1)?,"timestamp":r.get::<_,String>(2)?
    }))).unwrap().filter_map(|r| r.ok()).collect();
    Json(serde_json::json!({
        "name": name,
        "author": email.split('@').next().unwrap_or("user"),
        "messages": msgs,
        "deploy_url": deploy_url,
        "project": project
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
                        let name = e.file_name().to_string_lossy().to_string();
                        // Skip sensitive/hidden files and dirs
                        if name.starts_with('.') || name == "node_modules" || name == "target" { continue; }
                        let s = e.path();
                        let d = dst.join(&name);
                        if s.is_dir() {
                            std::fs::create_dir_all(&d).ok();
                            copy_recursive(&s, &d);
                        } else if !d.exists() {
                            // Skip sensitive file types
                            let lower = name.to_lowercase();
                            if lower.ends_with(".pem") || lower.ends_with(".key") || lower.ends_with(".p8")
                                || lower.ends_with(".p12") || lower.contains("secret") || lower.contains("credential")
                                || lower == "id_rsa" || lower == "id_ed25519" { continue; }
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
    let chat_id = std::env::var("TELEGRAM_CHAT_ID").unwrap_or_default();

    if !bot_token.is_empty() && !chat_id.is_empty() {
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

// ── Video generation (Veo 3) ──

#[derive(Deserialize)]
struct VideoReq {
    token: Option<String>,
    prompt: String,
    duration: Option<u32>,
    aspect_ratio: Option<String>,
    reference_image: Option<String>, // base64
}

async fn generate_video(State(s): State<Arc<AppState>>, Json(body): Json<VideoReq>) -> Response {
    let (uid, ..) = match auth_user(&s, body.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let key = match s.gemini_key.as_deref() {
        Some(k) => k.to_string(),
        None => return (StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error":"GEMINI_API_KEY not configured"}))).into_response(),
    };
    let duration = body.duration.unwrap_or(8).min(8);
    let aspect = body.aspect_ratio.as_deref().unwrap_or("16:9");

    tracing::info!("Veo 3 generation requested by user {} — {}s {}", &uid[..4], duration, aspect);

    match veo::generate_video(&key, &body.prompt, duration, aspect, body.reference_image.as_deref()).await {
        Ok(result) => {
            // Save video to user's project and return path
            let filename = format!("video_{}.mp4", chrono::Utc::now().format("%Y%m%d_%H%M%S"));
            let video_dir = format!("{}/users/{}/videos", s.workdir, uid);
            std::fs::create_dir_all(&video_dir).ok();
            let video_path = format!("{}/{}", video_dir, filename);
            std::fs::write(&video_path, &result.video_data).ok();

            use base64::{engine::general_purpose::STANDARD, Engine as _};
            let b64 = STANDARD.encode(&result.video_data);
            Json(serde_json::json!({
                "url": format!("data:video/mp4;base64,{}", b64),
                "path": format!("videos/{}", filename),
                "size_mb": result.video_data.len() as f64 / 1024.0 / 1024.0,
                "duration": result.duration
            })).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e}))).into_response(),
    }
}

// ── Nano Banana (fast image gen) ──

async fn generate_nanobanana(State(s): State<Arc<AppState>>, Json(body): Json<ImageReq>) -> Response {
    if auth_user(&s, body.token.as_deref()).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let key = match s.gemini_key.as_deref() {
        Some(k) => k.to_string(),
        None => return (StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error":"GEMINI_API_KEY not configured"}))).into_response(),
    };
    match veo::generate_image_nanobanana(&key, &body.prompt).await {
        Ok((mime, data)) => Json(serde_json::json!({
            "url": format!("data:{};base64,{}", mime, data),
            "model": "nano-banana-pro-preview"
        })).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e}))).into_response(),
    }
}

// ── Pair Programming (shared session write) ──

async fn pair_session(Path(share_id): Path<String>, Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    let session = db.query_row(
        "SELECT s.id, s.name, s.project, s.user_id FROM sessions s WHERE s.share_id=?1",
        [&share_id], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?,
            r.get::<_,String>(2)?, r.get::<_,String>(3)?))
    ).ok();
    let (src_sid, name, project, owner_uid) = match session {
        Some(s) => s, None => return StatusCode::NOT_FOUND.into_response(),
    };

    // If it's the owner, just return their session
    if owner_uid == uid {
        return Json(serde_json::json!({
            "session_id": src_sid, "name": name, "project": project, "pair_mode": true
        })).into_response();
    }

    // Create a new session for the pair user with copied messages + shared project
    let new_sid = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let pair_name = format!("{} (pair)", name);
    // Point to owner's project directory for real-time file sharing
    let collab_project = format!("../{}/{}", owner_uid, if project.is_empty() { "." } else { &project });
    db.execute("INSERT INTO sessions (id,user_id,name,created_at,project) VALUES (?1,?2,?3,?4,?5)",
        rusqlite::params![new_sid, uid, pair_name, now, collab_project]).ok();
    // Copy all existing messages so pair user sees full history
    db.execute(
        "INSERT INTO messages (session_id,role,content,timestamp) \
         SELECT ?1,role,content,timestamp FROM messages WHERE session_id=?2 ORDER BY id",
        rusqlite::params![new_sid, src_sid]).ok();

    let msg_count: i64 = db.query_row(
        "SELECT COUNT(*) FROM messages WHERE session_id=?1", [&new_sid], |r| r.get(0)
    ).unwrap_or(0);

    tracing::info!("Pair session: user {} joined {} ({} messages copied)", &uid[..4], src_sid, msg_count);
    Json(serde_json::json!({
        "session_id": new_sid, "name": pair_name, "project": collab_project, "pair_mode": true,
        "messages_copied": msg_count
    })).into_response()
}

// ── Time Machine (auto-commit + revert) ──

async fn list_snapshots(Path(project): Path<String>, Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let dir = format!("{}/users/{}/{}", s.workdir, uid, project);
    if !std::path::Path::new(&dir).join(".git").is_dir() {
        return Json(serde_json::json!({"snapshots": []})).into_response();
    }
    let output = Command::new("git")
        .args(["log", "--oneline", "--format=%H|%s|%cr", "-20"])
        .current_dir(&dir)
        .output().await;
    let snapshots: Vec<serde_json::Value> = match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).lines().filter_map(|l| {
            let parts: Vec<&str> = l.splitn(3, '|').collect();
            if parts.len() == 3 {
                Some(serde_json::json!({"hash": parts[0], "message": parts[1], "time": parts[2]}))
            } else { None }
        }).collect(),
        Err(_) => vec![],
    };
    Json(serde_json::json!({"snapshots": snapshots})).into_response()
}

#[derive(Deserialize)] struct RevertReq { hash: String }

async fn revert_snapshot(Path(project): Path<String>, Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    Json(body): Json<RevertReq>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let dir = format!("{}/users/{}/{}", s.workdir, uid, project);
    // Validate hash is alphanumeric
    if !body.hash.chars().all(|c| c.is_ascii_alphanumeric()) {
        return (StatusCode::BAD_REQUEST, "Invalid hash").into_response();
    }
    let output = Command::new("git")
        .args(["checkout", &body.hash, "--", "."])
        .current_dir(&dir)
        .output().await;
    match output {
        Ok(o) if o.status.success() => Json(serde_json::json!({"ok": true, "reverted_to": body.hash})).into_response(),
        _ => (StatusCode::BAD_REQUEST, "Revert failed").into_response(),
    }
}

// ── Widget Builder ──

async fn get_widget(Path(widget_id): Path<String>, State(s): State<Arc<AppState>>) -> Response {
    // widget_id = deploy slug
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    let deploy = db.query_row(
        "SELECT slug FROM deployed_apps WHERE slug=?1 OR id=?1", [&widget_id], |r| r.get::<_,String>(0)
    ).ok();
    let slug = match deploy {
        Some(s) => s, None => return StatusCode::NOT_FOUND.into_response(),
    };
    let script = format!(
        r#"(function(){{var d=document.createElement('div');d.id='cw-widget-{slug}';d.style.cssText='width:100%;max-width:800px;margin:0 auto';var f=document.createElement('iframe');f.src='https://{slug}.chatweb.ai';f.style.cssText='width:100%;height:600px;border:none;border-radius:12px;box-shadow:0 4px 24px rgba(0,0,0,.15)';d.appendChild(f);var s=document.currentScript;s.parentNode.insertBefore(d,s)}})();"#
    );
    (StatusCode::OK, [
        ("content-type", "application/javascript"),
        ("access-control-allow-origin", "*"),
        ("cache-control", "public, max-age=300"),
    ], script).into_response()
}

async fn widget_embed_info(Path(slug): Path<String>) -> Response {
    let embed_script = format!(r#"<script src="https://chatweb.ai/w/{}"></script>"#, slug);
    let iframe = format!(r#"<iframe src="https://{}.chatweb.ai" style="width:100%;height:600px;border:none;border-radius:12px" loading="lazy"></iframe>"#, slug);
    Json(serde_json::json!({
        "script": embed_script,
        "iframe": iframe,
        "url": format!("https://{}.chatweb.ai", slug)
    })).into_response()
}

// ── AI App Store ──

async fn app_store_page(State(s): State<Arc<AppState>>) -> Html<String> {
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = db.prepare(
        "SELECT d.slug, d.title, d.project, g.description, g.tags, g.likes, g.author \
         FROM deployed_apps d LEFT JOIN gallery g ON d.project = g.project AND d.user_id = g.user_id \
         ORDER BY g.likes DESC, d.created_at DESC LIMIT 50"
    ).unwrap();
    let apps: Vec<(String,String,String,String,String,i64,String)> = st.query_map([], |r| Ok((
        r.get(0)?, r.get::<_,String>(1).unwrap_or_default(),
        r.get::<_,String>(2).unwrap_or_default(), r.get::<_,String>(3).unwrap_or_default(),
        r.get::<_,String>(4).unwrap_or_default(), r.get::<_,i64>(5).unwrap_or(0),
        r.get::<_,String>(6).unwrap_or_else(|_| "user".to_string()),
    ))).unwrap().filter_map(|r| r.ok()).collect();

    let cards: String = apps.iter().map(|(slug, title, _proj, desc, tags, likes, author)| {
        let t = if title.is_empty() { slug } else { title };
        format!(r#"<a href="https://{slug}.chatweb.ai" target="_blank" class="card">
            <h3>{t}</h3><p>{desc}</p>
            <div class="meta"><span>@{author}</span>{}<span>{likes} &hearts;</span></div>
        </a>"#, if tags.is_empty() { String::new() } else { format!("<span>{tags}</span>") })
    }).collect();

    Html(format!(r##"<!DOCTYPE html><html lang="en"><head><meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>ChatWeb Apps — AI-Built App Store</title>
<style>*{{margin:0;padding:0;box-sizing:border-box}}body{{background:#09090b;color:#f4f4f5;font-family:system-ui;min-height:100vh}}
.hd{{padding:24px;text-align:center;border-bottom:1px solid #27272a}}.hd h1{{font-size:32px;font-weight:800;background:linear-gradient(135deg,#a78bfa,#60a5fa);-webkit-background-clip:text;-webkit-text-fill-color:transparent}}
.hd p{{color:#a1a1aa;margin-top:8px}}.grid{{display:grid;grid-template-columns:repeat(auto-fill,minmax(280px,1fr));gap:16px;padding:24px;max-width:1200px;margin:0 auto}}
.card{{background:#18181b;border:1px solid #27272a;border-radius:16px;padding:20px;text-decoration:none;color:#f4f4f5;transition:all .2s}}
.card:hover{{border-color:#a78bfa;transform:translateY(-2px);box-shadow:0 8px 32px rgba(167,139,250,.15)}}
.card h3{{font-size:18px;margin-bottom:8px}}.card p{{color:#a1a1aa;font-size:13px;margin-bottom:12px;line-height:1.6}}
.meta{{display:flex;gap:8px;font-size:11px;color:#52525b;flex-wrap:wrap}}.meta span{{background:#27272a;padding:2px 8px;border-radius:12px}}
.cta{{text-align:center;padding:32px}}.cta a{{display:inline-flex;align-items:center;gap:8px;background:linear-gradient(135deg,#a78bfa,#818cf8);color:#fff;text-decoration:none;padding:14px 28px;border-radius:12px;font-weight:700;font-size:16px}}
</style></head><body>
<div class="hd"><h1>ChatWeb Apps</h1><p>Apps built entirely by AI — browse, use, and remix</p></div>
<div class="grid">{cards}</div>
<div class="cta"><a href="https://chatweb.ai">Build your own app with AI &rarr;</a></div>
</body></html>"##))
}

// ── Deploy (subdomain hosting) ──

#[derive(Deserialize)] struct DeployReq { project: String, slug: String, title: Option<String> }

async fn create_deploy(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    Json(body): Json<DeployReq>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    // Validate slug: lowercase alphanumeric + hyphens, 3-32 chars
    let slug: String = body.slug.trim().to_lowercase()
        .chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    if slug.len() < 3 || slug.len() > 32 {
        return (StatusCode::BAD_REQUEST, "Slug must be 3-32 chars (a-z, 0-9, -)").into_response();
    }
    // Reserved slugs
    if ["www","api","app","mail","ftp","admin","dev","staging","test"].contains(&slug.as_str()) {
        return (StatusCode::BAD_REQUEST, "This subdomain is reserved").into_response();
    }
    // Check project exists
    let dir = PathBuf::from(format!("{}/users/{}/{}", s.workdir, uid, body.project));
    if !dir.is_dir() { return (StatusCode::NOT_FOUND, "Project not found").into_response(); }
    // Check index.html exists
    if !dir.join("index.html").exists() {
        return (StatusCode::BAD_REQUEST, "Project needs an index.html to deploy").into_response();
    }

    let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    match db.execute(
        "INSERT INTO deployed_apps (id,user_id,slug,project,title,created_at) VALUES (?1,?2,?3,?4,?5,?6)",
        rusqlite::params![id, uid, slug, body.project, body.title.as_deref().unwrap_or(""), now]
    ) {
        Ok(_) => {
            let url = format!("https://{}.chatweb.ai", slug);
            tracing::info!("Deployed {}.chatweb.ai → user {}/{}", slug, &uid[..4], body.project);
            Json(serde_json::json!({"id": id, "slug": slug, "url": url, "ok": true})).into_response()
        }
        Err(_) => (StatusCode::CONFLICT, "This subdomain is already taken").into_response(),
    }
}

async fn list_deploys(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = db.prepare(
        "SELECT id,slug,project,title,created_at FROM deployed_apps WHERE user_id=?1 ORDER BY created_at DESC"
    ).unwrap();
    let items: Vec<serde_json::Value> = st.query_map([&uid], |r| Ok(serde_json::json!({
        "id": r.get::<_,String>(0)?, "slug": r.get::<_,String>(1)?,
        "project": r.get::<_,String>(2)?, "title": r.get::<_,String>(3)?,
        "url": format!("https://{}.chatweb.ai", r.get::<_,String>(1)?),
        "created_at": r.get::<_,String>(4)?
    }))).unwrap().filter_map(|r| r.ok()).collect();
    Json(items).into_response()
}

async fn delete_deploy(Path(id): Path<String>, Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    db.execute("DELETE FROM deployed_apps WHERE id=?1 AND user_id=?2", rusqlite::params![id, uid]).ok();
    StatusCode::NO_CONTENT.into_response()
}

// ── Agent Marketplace ──

#[derive(Deserialize)] struct PublishAgentReq {
    name: String, description: Option<String>, command: String,
    project: Option<String>, interval_secs: i64, tags: Option<String>,
}

async fn publish_agent(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>,
    Json(body): Json<PublishAgentReq>) -> Response {
    let (uid, email, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if body.name.trim().is_empty() || body.command.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "name and command required").into_response();
    }
    let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let author = email.split('@').next().unwrap_or("user").to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let interval = body.interval_secs.max(300).min(604800);
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    db.execute(
        "INSERT INTO agent_marketplace (id,user_id,author,name,description,command,project,interval_secs,tags,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        rusqlite::params![id, uid, author, body.name.trim(), body.description.as_deref().unwrap_or(""),
            body.command.trim(), body.project.as_deref().unwrap_or(""), interval,
            body.tags.as_deref().unwrap_or(""), now]
    ).ok();
    Json(serde_json::json!({"id": id, "ok": true})).into_response()
}

async fn list_agents(State(s): State<Arc<AppState>>) -> Response {
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = db.prepare(
        "SELECT id,author,name,description,command,project,interval_secs,tags,installs,created_at FROM agent_marketplace ORDER BY installs DESC, created_at DESC LIMIT 50"
    ).unwrap();
    let items: Vec<serde_json::Value> = st.query_map([], |r| Ok(serde_json::json!({
        "id": r.get::<_,String>(0)?, "author": r.get::<_,String>(1)?,
        "name": r.get::<_,String>(2)?, "description": r.get::<_,String>(3)?,
        "command": r.get::<_,String>(4)?, "project": r.get::<_,String>(5)?,
        "interval_secs": r.get::<_,i64>(6)?, "tags": r.get::<_,String>(7)?,
        "installs": r.get::<_,i64>(8)?, "created_at": r.get::<_,String>(9)?
    }))).unwrap().filter_map(|r| r.ok()).collect();
    Json(items).into_response()
}

async fn install_agent(Path(agent_id): Path<String>, Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    // Get agent details
    let agent = db.query_row(
        "SELECT name,command,project,interval_secs FROM agent_marketplace WHERE id=?1",
        [&agent_id], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?,
            r.get::<_,String>(2)?, r.get::<_,i64>(3)?))
    );
    let (name, command, project, interval) = match agent {
        Ok(a) => a, Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    // Create cron job for user
    let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let now = chrono::Utc::now();
    let now_ts = now.timestamp();
    db.execute(
        "INSERT INTO cron_jobs (id,user_id,name,command,project,interval_secs,next_run,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        rusqlite::params![id, uid, name, command, project, interval, now_ts + interval, now.to_rfc3339()]
    ).ok();
    // Increment install count
    db.execute("UPDATE agent_marketplace SET installs = installs + 1 WHERE id=?1", [&agent_id]).ok();
    Json(serde_json::json!({"id": id, "ok": true, "name": name})).into_response()
}

// ── Live Coding ──

async fn toggle_broadcast(Path(session_id): Path<String>, Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, email, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    // Verify session belongs to user
    let session = {
        let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
        db.query_row("SELECT name, COALESCE(project,'') FROM sessions WHERE id=?1 AND user_id=?2",
            rusqlite::params![session_id, uid], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?))
        ).ok()
    };
    let (name, project) = match session {
        Some(s) => s, None => return StatusCode::NOT_FOUND.into_response(),
    };

    let mut broadcasts = s.live_broadcasts.lock().unwrap_or_else(|e| e.into_inner());
    if broadcasts.contains_key(&session_id) {
        broadcasts.remove(&session_id);
        Json(serde_json::json!({"broadcasting": false})).into_response()
    } else {
        let (tx, _) = tokio::sync::broadcast::channel(256);
        broadcasts.insert(session_id.clone(), LiveBroadcast {
            session_id: session_id.clone(),
            user_email: email,
            session_name: name,
            project,
            started_at: chrono::Utc::now().to_rfc3339(),
            tx,
        });
        Json(serde_json::json!({"broadcasting": true})).into_response()
    }
}

async fn list_live(State(s): State<Arc<AppState>>) -> Response {
    let broadcasts = s.live_broadcasts.lock().unwrap_or_else(|e| e.into_inner());
    let items: Vec<serde_json::Value> = broadcasts.values().map(|b| {
        serde_json::json!({
            "session_id": b.session_id,
            "author": b.user_email.split('@').next().unwrap_or("user"),
            "name": b.session_name,
            "project": b.project,
            "started_at": b.started_at,
            "viewers": b.tx.receiver_count(),
        })
    }).collect();
    Json(items).into_response()
}

async fn ws_watch_handler(
    ws: WebSocketUpgrade,
    Path(session_id): Path<String>,
    Query(q): Query<TokenQ>,
    State(state): State<Arc<AppState>>,
) -> Response {
    // Require valid authentication token
    if auth_user(&state, q.token.as_deref()).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let rx = {
        let broadcasts = state.live_broadcasts.lock().unwrap_or_else(|e| e.into_inner());
        broadcasts.get(&session_id).map(|b| b.tx.subscribe())
    };
    match rx {
        Some(rx) => ws.on_upgrade(move |socket| handle_ws_watch(socket, rx)),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn handle_ws_watch(mut ws: WebSocket, mut rx: tokio::sync::broadcast::Receiver<String>) {
    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(text) => {
                        if ws.send(Message::Text(text.into())).await.is_err() { break; }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
            msg = ws.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
}

async fn live_page(State(_s): State<Arc<AppState>>) -> Html<&'static str> {
    Html(HTML)
}

// ── Gallery Enhancements ──

async fn like_gallery(Path(id): Path<String>, Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    if auth_user(&s, q.token.as_deref()).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    db.execute("UPDATE gallery SET likes = likes + 1 WHERE id=?1", [&id]).ok();
    let likes: i64 = db.query_row("SELECT likes FROM gallery WHERE id=?1", [&id], |r| r.get(0)).unwrap_or(0);
    Json(serde_json::json!({"likes": likes})).into_response()
}

async fn remix_gallery(Path(gallery_id): Path<String>, Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> Response {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    // Get gallery item
    let item = {
        let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
        db.query_row(
            "SELECT user_id, project, title FROM gallery WHERE id=?1",
            [&gallery_id], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?, r.get::<_,String>(2)?))
        ).ok()
    };
    let (src_uid, src_project, title) = match item {
        Some(i) => i, None => return StatusCode::NOT_FOUND.into_response(),
    };

    // Copy project files
    let new_project = format!("{}-remix", src_project);
    let src_dir = format!("{}/users/{}/{}", s.workdir, src_uid, src_project);
    let dst_dir = format!("{}/users/{}/{}", s.workdir, uid, new_project);
    if std::path::Path::new(&src_dir).is_dir() {
        std::fs::create_dir_all(&dst_dir).ok();
        fn copy_recursive(src: &std::path::Path, dst: &std::path::Path) {
            if let Ok(rd) = std::fs::read_dir(src) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if name.starts_with('.') || name == "node_modules" || name == "target" { continue; }
                    let s = e.path(); let d = dst.join(&name);
                    if s.is_dir() { std::fs::create_dir_all(&d).ok(); copy_recursive(&s, &d); }
                    else {
                        let lower = name.to_lowercase();
                        if lower.ends_with(".pem") || lower.ends_with(".key") || lower.contains("secret") { continue; }
                        std::fs::copy(&s, &d).ok();
                    }
                }
            }
        }
        copy_recursive(std::path::Path::new(&src_dir), std::path::Path::new(&dst_dir));
    }

    // Create session for the remix
    let new_sid = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let session_name = format!("{} (remix)", title);
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    db.execute("INSERT INTO sessions (id,user_id,name,created_at,project) VALUES (?1,?2,?3,?4,?5)",
        rusqlite::params![new_sid, uid, session_name, now, new_project]).ok();

    Json(serde_json::json!({
        "session_id": new_sid, "project": new_project, "name": session_name
    })).into_response()
}

// ── WebSocket ──

async fn ws_handler(ws: WebSocketUpgrade, Query(q): Query<TokenQ>, State(state): State<Arc<AppState>>) -> Response {
    let user = auth_user(&state, q.token.as_deref());
    if user.is_none() { return StatusCode::UNAUTHORIZED.into_response(); }
    let user = auth_user(&state, q.token.as_deref());
    if user.is_none() { return StatusCode::UNAUTHORIZED.into_response(); }
    let (uid, _email, credits, api_key, plan) = user.unwrap();
    ws.on_upgrade(move |socket| handle_ws(socket, state, uid, credits, api_key, plan))
}

async fn handle_ws(mut ws: WebSocket, state: Arc<AppState>, uid: String, mut credits: f64, api_key: Option<String>, plan: String) {
    let (stop_tx, mut stop_rx) = watch::channel(false);
    // Per-connection message rate limit: max 60 messages per 60-second window
    let mut conn_msg_count = 0u32;
    let mut conn_window_start = std::time::Instant::now();

    loop {
        let msg = ws.recv().await;
        match msg {
            Some(Ok(Message::Text(text))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    if v.get("type").and_then(|t|t.as_str()) == Some("stop") {
                        let _ = stop_tx.send(true); continue;
                    }
                }

                // Per-connection rate limit: reset window every 60s, max 60 msgs/window
                if conn_window_start.elapsed().as_secs() >= 60 {
                    conn_msg_count = 0;
                    conn_window_start = std::time::Instant::now();
                }
                conn_msg_count += 1;
                if conn_msg_count > 60 {
                    let _ = ws.send(Message::Text(serde_json::json!({
                        "type":"error","text":"Too many messages. Please wait a moment."
                    }).to_string().into())).await;
                    continue;
                }

                // Global rate limit
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
                    // Resolve and verify path stays within workspaces (allows collab ../otheruser/proj)
                    let resolved = std::path::Path::new(&w).canonicalize()
                        .unwrap_or_else(|_| std::path::PathBuf::from(&w));
                    let root = std::path::Path::new(&state.workdir).canonicalize()
                        .unwrap_or_else(|_| std::path::PathBuf::from(&state.workdir));
                    if resolved.starts_with(&root) && resolved.is_dir() {
                        resolved.to_string_lossy().to_string()
                    } else { user_sandbox.clone() }
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
                let use_nou = cm.model.as_deref().map(|m| m.starts_with("nou")).unwrap_or(false);
                let (model, effort) = if use_gemini {
                    (gemini::MODEL, "")
                } else if use_nou {
                    ("nou", "")
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
                              db.execute("INSERT INTO usage_log (user_id,session_id,model,cost_usd,created_at) VALUES (?1,?2,?3,?4,?5)",
                                rusqlite::params![uid, cm.session, model, charge, now]).ok();
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

                // ── NOU path (local machine via relay) ────────────────────
                if use_nou {
                    // Resolve node ID: user's vault NOU_NODE_ID > server env > error
                    let vault_keys_nou = load_user_keys(&state, &uid).await;
                    let node_id = vault_keys_nou.iter()
                        .find(|(k, _)| k == "NOU_NODE_ID")
                        .map(|(_, v)| v.clone())
                        .or_else(|| state.nou_node_id.clone());

                    let node_id = match node_id {
                        Some(id) if !id.is_empty() => id,
                        _ => {
                            let _ = ws.send(Message::Text(serde_json::json!({
                                "type":"error","text":"No NOU node connected. Start NOU on your machine or set NOU_NODE_ID in your keys."
                            }).to_string().into())).await;
                            state.active_procs.lock().unwrap_or_else(|e| e.into_inner()).remove(&uid);
                            continue;
                        }
                    };

                    // Determine NOU model: "nou:gemma4-31b" or default to whatever node runs
                    let nou_model = cm.model.as_deref()
                        .and_then(|m| m.strip_prefix("nou:"))
                        .unwrap_or("") // empty = relay/node picks default
                        .to_string();
                    let nou_model_str = if nou_model.is_empty() {
                        // Ask node for its current model
                        let client = reqwest::Client::new();
                        let models_url = format!("{}/n/{}/v1/models", state.nou_relay_url, node_id);
                        let fetched = async {
                            let r = client.get(&models_url).timeout(std::time::Duration::from_secs(5)).send().await.ok()?;
                            let v: serde_json::Value = r.json().await.ok()?;
                            v["data"][0]["id"].as_str().map(|s| s.to_string())
                        }.await;
                        fetched.unwrap_or_else(|| "Qwen3-1.7B-Q4_K_M.gguf".to_string())
                    } else { nou_model };

                    // Load conversation history
                    let history: Vec<(String, String)> = {
                        let db = state.db.lock().unwrap_or_else(|e| e.into_inner());
                        let mut stmt = db.prepare(
                            "SELECT role, content FROM messages WHERE session_id=?1 ORDER BY id ASC LIMIT 20"
                        ).unwrap();
                        stmt.query_map(rusqlite::params![cm.session], |r| Ok((r.get(0)?, r.get(1)?)))
                            .unwrap().filter_map(|r| r.ok()).collect()
                    };

                    let result = nou::stream(&mut ws, &state.nou_relay_url, &node_id, &nou_model_str, &history, &cm.text).await;
                    state.active_procs.lock().unwrap_or_else(|e| e.into_inner()).remove(&uid);
                    match result {
                        Ok(nr) => {
                            // NOU is free (local) — deduct nothing
                            if !nr.text.is_empty() {
                                let db = state.db.lock().unwrap_or_else(|e| e.into_inner());
                                let now = chrono::Utc::now().to_rfc3339();
                                db.execute("INSERT INTO messages (session_id,role,content,timestamp) VALUES (?1,'assistant',?2,?3)",
                                    rusqlite::params![cm.session, nr.text, now]).ok();
                            }
                            let _ = ws.send(Message::Text(serde_json::json!({
                                "type":"done","credits":credits
                            }).to_string().into())).await;
                        }
                        Err(e) => {
                            let _ = ws.send(Message::Text(serde_json::json!({
                                "type":"error","text":format!("NOU error: {e}")
                            }).to_string().into())).await;
                        }
                    }
                    continue;
                }
                // ── End NOU path ──────────────────────────────────────────

                // Load user keys early (needed for deploy context + cmd env)
                let vault_keys = load_user_keys(&state, &uid).await;

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

                // Enrich message with deploy context
                let enriched_text = {
                    let db = state.db.lock().unwrap_or_else(|e| e.into_inner());
                    // Pro/Power: inject cross-session memory as context
                    let memory_prefix = if plan == "pro" || plan == "power" {
                        let mem: Option<String> = db.query_row(
                            "SELECT content FROM user_memory WHERE user_id=?1",
                            [&uid], |r| r.get(0)
                        ).ok().filter(|s: &String| !s.is_empty());
                        mem.map(|m| format!("[CONTEXT FROM PREVIOUS SESSIONS:\n{}\n]\n\n", m))
                            .unwrap_or_default()
                    } else { String::new() };
                    let deploy_info: Option<String> = if !project_name.is_empty() {
                        db.query_row(
                            "SELECT slug FROM deployed_apps WHERE project=?1 AND user_id=?2",
                            rusqlite::params![project_name, uid], |r| r.get::<_,String>(0)
                        ).ok().map(|slug| format!(
                            "\n\n[IMPORTANT: This project is live at https://{slug}.chatweb.ai — you MUST mention this exact URL at the end of your response. Say: \"サイトを更新しました！こちらで確認できます: https://{slug}.chatweb.ai\"]"
                        ))
                    } else { None };
                    // Check available deploy targets from user's keys
                    let key_exists = |name: &str| vault_keys.iter().any(|(k, v)| k == name && !v.is_empty());
                    let mut targets = vec!["ChatWeb subdomain (Deploy menu)"];
                    if key_exists("FLY_API_TOKEN") { targets.push("Fly.io (fly deploy)"); }
                    if key_exists("VERCEL_TOKEN") { targets.push("Vercel (vercel --yes)"); }
                    if key_exists("CLOUDFLARE_API_TOKEN") { targets.push("Cloudflare (wrangler deploy)"); }
                    if key_exists("NETLIFY_AUTH_TOKEN") { targets.push("Netlify (netlify deploy --prod)"); }
                    if key_exists("RAILWAY_TOKEN") { targets.push("Railway (railway up)"); }
                    if key_exists("SUPABASE_ACCESS_TOKEN") { targets.push("Supabase (supabase functions deploy)"); }
                    // KAGI integration context
                    if key_exists("KAGI_AUTH_TOKEN") { targets.push("KAGI Smart Home (curl API)"); }
                    if key_exists("SWITCHBOT_TOKEN") { targets.push("SwitchBot (smart lock control)"); }
                    if key_exists("BEDS24_API_KEY") { targets.push("Beds24 (reservation data)"); }
                    let targets_str = if targets.len() > 1 {
                        format!("\n[Deploy targets available: {}]", targets.join(", "))
                    } else { String::new() };
                    format!("{}{}{}{}", memory_prefix, cm.text, deploy_info.unwrap_or_default(), targets_str)
                };
                cmd.arg("-p").arg("--output-format").arg("stream-json")
                    .arg("--verbose").arg("--dangerously-skip-permissions")
                    .arg("--model").arg(model)
                    .arg(&enriched_text);
                if let Some(ref sid) = claude_sid { cmd.arg("--resume").arg(sid); }
                for (k, v) in &vault_keys {
                    cmd.env(k, v);
                }
                // Configure git credentials for GitHub push
                // If user has GITHUB_TOKEN, set up credential helper so `git push` works
                let has_gh_token = vault_keys.iter().any(|(k, v)| k == "GITHUB_TOKEN" && !v.is_empty());
                if has_gh_token {
                    let gh_token = vault_keys.iter().find(|(k, _)| k == "GITHUB_TOKEN").unwrap().1.clone();
                    cmd.env("GH_TOKEN", &gh_token);
                    // Write a tiny credential helper script path into the env
                    // git will call GIT_ASKPASS with the prompt; we return the token as password
                    let helper = format!("!f() {{ echo \"protocol=https\nhost=github.com\nusername=x-access-token\npassword={}\"; }}; f", gh_token);
                    cmd.env("GIT_CONFIG_COUNT", "1");
                    cmd.env("GIT_CONFIG_KEY_0", "credential.https://github.com.helper");
                    cmd.env("GIT_CONFIG_VALUE_0", &helper);
                }
                cmd.current_dir(&workdir)
                    .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
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
                // Pass server-side Gemini key for Veo/image generation
                // Write to user's .env file so Python scripts and Claude CLI subprocesses can read it
                if let Some(ref gk) = state.gemini_key {
                    if !vault_keys.iter().any(|(k, _)| k == "GEMINI_API_KEY") {
                        cmd.env("GEMINI_API_KEY", gk);
                        cmd.env("GOOGLE_API_KEY", gk);
                        // Also write to .env in project dir so Python scripts pick it up
                        let env_path = format!("{}/.env", workdir);
                        let env_content = std::fs::read_to_string(&env_path).unwrap_or_default();
                        if !env_content.contains("GEMINI_API_KEY") {
                            let line = format!("GEMINI_API_KEY={}\nGOOGLE_API_KEY={}\n", gk, gk);
                            std::fs::write(&env_path, format!("{}{}", env_content, line)).ok();
                        }
                    }
                }

                let mut child = match cmd.spawn() {
                    Ok(c)=>c, Err(e)=>{
                        tracing::error!("claude spawn failed uid={uid} err={e}");
                        let err_msg = match e.kind() {
                            std::io::ErrorKind::NotFound => "Claude CLI is not available on this server.",
                            std::io::ErrorKind::PermissionDenied => "Permission denied. Please try again.",
                            _ => "Server is busy. Please try again in a moment.",
                        };
                        let _ = ws.send(Message::Text(serde_json::json!({"type":"error","text":err_msg}).to_string().into())).await;
                        state.active_procs.lock().unwrap_or_else(|e| e.into_inner()).remove(&uid);
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
                                    // Redact known secret patterns from output
                                    let safe_line = redact_secrets(&line, &vault_keys);
                                    // Broadcast to live watchers if session is broadcasting
                                    if let Ok(broadcasts) = state.live_broadcasts.lock() {
                                        if let Some(b) = broadcasts.get(&sid_clone) {
                                            let _ = b.tx.send(safe_line.clone());
                                        }
                                    }
                                    if ws.send(Message::Text(safe_line.into())).await.is_err() {
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
                    db.execute("INSERT INTO usage_log (user_id,session_id,model,cost_usd,created_at) VALUES (?1,?2,?3,?4,?5)",
                        rusqlite::params![uid, sid_clone, model, charge, chrono::Utc::now().to_rfc3339()]).ok();
                }

                // Save assistant response
                if !assistant_text.is_empty() {
                    let db=db_ref.lock().unwrap_or_else(|e| e.into_inner()); let now=chrono::Utc::now().to_rfc3339();
                    db.execute("INSERT INTO messages (session_id,role,content,timestamp) VALUES (?1,'assistant',?2,?3)",
                        rusqlite::params![sid_clone,assistant_text,now]).ok();
                }

                // Pro/Power: update cross-session memory asynchronously
                if !assistant_text.is_empty() && (plan == "pro" || plan == "power") {
                    if let Some(akey) = state.anthropic_key.clone() {
                        let db2 = Arc::clone(&db_ref);
                        let uid2 = uid.clone();
                        let user_msg = cm.text.clone();
                        let asst_msg = assistant_text.clone();
                        tokio::spawn(async move {
                            update_user_memory(db2, akey, uid2, user_msg, asst_msg).await;
                        });
                    }
                }

                // R2: push modified files after Claude finishes
                state.storage.push(&uid, project_name).await;

                // Time Machine: auto-commit snapshot after each turn
                if !project_name.is_empty() {
                    let snap_dir = format!("{}/users/{}/{}", state.workdir, uid, project_name);
                    if std::path::Path::new(&snap_dir).is_dir() {
                        let snap_msg = assistant_text.chars().take(72).collect::<String>();
                        let snap_msg = if snap_msg.is_empty() { "auto-snapshot".to_string() } else { snap_msg };
                        // Init git if not exists, then commit
                        let _ = Command::new("git").args(["init"]).current_dir(&snap_dir).output().await;
                        let _ = Command::new("git").args(["add", "-A"]).current_dir(&snap_dir).output().await;
                        let _ = Command::new("git")
                            .args(["commit", "-m", &snap_msg, "--allow-empty-message", "--no-gpg-sign"])
                            .env("GIT_AUTHOR_NAME", "ChatWeb AI").env("GIT_AUTHOR_EMAIL", "ai@chatweb.ai")
                            .env("GIT_COMMITTER_NAME", "ChatWeb AI").env("GIT_COMMITTER_EMAIL", "ai@chatweb.ai")
                            .current_dir(&snap_dir).output().await;
                    }
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

// ── GET /api/nou/status ──────────────────────────────────────────────────────
async fn nou_status(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> impl IntoResponse {
    let user = auth_user(&s, q.token.as_deref());
    let (uid, ..) = match user { Some(u) => u, None => return (StatusCode::UNAUTHORIZED, Json(serde_json::Value::Null)) };

    // Get user's NOU_NODE_ID from vault or fall back to server default
    let vault_keys = load_user_keys(&s, &uid).await;
    let node_id = vault_keys.iter()
        .find(|(k, _)| k == "NOU_NODE_ID")
        .map(|(_, v)| v.clone())
        .or_else(|| s.nou_node_id.clone());

    // Query the relay for connected nodes
    let client = reqwest::Client::new();
    let relay_status: Option<serde_json::Value> = async {
        let r = client.get(format!("{}/api/status", s.nou_relay_url))
            .timeout(std::time::Duration::from_secs(5))
            .send().await.ok()?;
        r.json().await.ok()
    }.await;

    let nodes: Vec<serde_json::Value> = relay_status.as_ref()
        .and_then(|v| v["nodes"].as_array())
        .map(|a| a.to_vec())
        .unwrap_or_default();

    // Find matching node if user has one configured
    let matched = node_id.as_deref().and_then(|nid| {
        nodes.iter().find(|n| n["node_id"].as_str() == Some(nid)).cloned()
    });

    let connected = matched.is_some() || (!nodes.is_empty() && node_id.is_none());
    let active_node = matched.or_else(|| nodes.first().cloned());

    (StatusCode::OK, Json(serde_json::json!({
        "connected": connected,
        "node": active_node,
        "relay_url": s.nou_relay_url,
        "all_nodes": nodes.len()
    })))
}

// ── Pro Memory: extract key facts and update persistent memory ──────────────
async fn update_user_memory(db: Db, anthropic_key: String, user_id: String, user_msg: String, assistant_msg: String) {
    // Load existing memory
    let existing: String = {
        let db = db.lock().unwrap_or_else(|e| e.into_inner());
        db.query_row("SELECT content FROM user_memory WHERE user_id=?1", [&user_id], |r| r.get(0))
            .unwrap_or_default()
    };

    // Truncate inputs to keep prompt small
    let user_snippet = user_msg.chars().take(600).collect::<String>();
    let asst_snippet = assistant_msg.chars().take(600).collect::<String>();

    let prompt = format!(
        "You are a memory extractor. Given a conversation exchange and existing memory, update the memory with any new important facts about the user: their projects, tech stack, preferences, goals, key decisions, or context that would help future conversations.\n\nRules:\n- Keep it concise (max 400 chars)\n- Use bullet points like \"• \"\n- Only include facts that will be useful in future sessions\n- Merge/deduplicate with existing memory\n- If nothing new is worth remembering, return the existing memory unchanged\n- Return ONLY the memory text, no explanation\n\nExisting memory:\n{}\n\nNew exchange:\nUser: {}\nAssistant: {}",
        if existing.is_empty() { "(none)".to_string() } else { existing },
        user_snippet,
        asst_snippet
    );

    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 200,
        "messages": [{"role": "user", "content": prompt}]
    });

    let resp = client.post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", &anthropic_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send().await;

    if let Ok(r) = resp {
        if let Ok(v) = r.json::<serde_json::Value>().await {
            if let Some(new_mem) = v["content"][0]["text"].as_str() {
                let new_mem = new_mem.trim().to_string();
                if !new_mem.is_empty() {
                    let now = chrono::Utc::now().to_rfc3339();
                    let db = db.lock().unwrap_or_else(|e| e.into_inner());
                    db.execute(
                        "INSERT INTO user_memory (user_id, content, updated_at) VALUES (?1,?2,?3) ON CONFLICT(user_id) DO UPDATE SET content=excluded.content, updated_at=excluded.updated_at",
                        rusqlite::params![user_id, new_mem, now]
                    ).ok();
                }
            }
        }
    }
}

// ── GET /api/memory ──────────────────────────────────────────────────────────
async fn get_memory(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> impl IntoResponse {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return (StatusCode::UNAUTHORIZED, Json(serde_json::Value::Null))
    };
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    let content: String = db.query_row(
        "SELECT content FROM user_memory WHERE user_id=?1", [&uid], |r| r.get(0)
    ).unwrap_or_default();
    let updated_at: Option<String> = db.query_row(
        "SELECT updated_at FROM user_memory WHERE user_id=?1", [&uid], |r| r.get(0)
    ).ok();
    (StatusCode::OK, Json(serde_json::json!({"content": content, "updated_at": updated_at})))
}

// ── DELETE /api/memory ───────────────────────────────────────────────────────
async fn delete_memory(Query(q): Query<TokenQ>, State(s): State<Arc<AppState>>) -> impl IntoResponse {
    let (uid, ..) = match auth_user(&s, q.token.as_deref()) {
        Some(u) => u, None => return StatusCode::UNAUTHORIZED
    };
    let db = s.db.lock().unwrap_or_else(|e| e.into_inner());
    db.execute("DELETE FROM user_memory WHERE user_id=?1", [&uid]).ok();
    StatusCode::NO_CONTENT
}
