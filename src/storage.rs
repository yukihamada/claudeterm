//! Storage abstraction: local filesystem or S3/R2
//!
//! Configure R2 mode: set R2_ENDPOINT, R2_BUCKET, R2_ACCESS_KEY_ID, R2_SECRET_ACCESS_KEY
//! Without them: pure local filesystem (same as before).
//!
//! Architecture:
//! - Local cache always used (Claude CLI needs local files)
//! - R2 mode: sync local ↔ R2 before/after operations
//! - This lets multiple VMs work on the same user data

use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Storage {
    pub base: PathBuf,
    pub r2: Option<R2Config>,
}

#[derive(Clone)]
pub struct R2Config {
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
}

impl Storage {
    pub fn from_env(workdir: &str) -> Self {
        let r2 = match (
            std::env::var("R2_ENDPOINT"),
            std::env::var("R2_BUCKET"),
            std::env::var("R2_ACCESS_KEY_ID"),
            std::env::var("R2_SECRET_ACCESS_KEY"),
        ) {
            (Ok(e), Ok(b), Ok(a), Ok(s)) if !e.is_empty() => {
                tracing::info!("Storage: R2 ({}/{}) + local cache", e, b);
                Some(R2Config { endpoint: e, bucket: b, access_key: a, secret_key: s })
            }
            _ => {
                tracing::info!("Storage: local only (set R2_* env vars for cloud storage)");
                None
            }
        };
        Storage { base: PathBuf::from(workdir), r2 }
    }

    pub fn user_dir(&self, uid: &str) -> PathBuf {
        self.base.join("users").join(uid)
    }

    pub fn project_dir(&self, uid: &str, project: &str) -> PathBuf {
        if project.is_empty() { self.user_dir(uid) }
        else { self.user_dir(uid).join(project) }
    }

    pub fn is_r2(&self) -> bool { self.r2.is_some() }

    /// Sync user's project FROM R2 → local cache (before Claude runs)
    pub async fn pull(&self, uid: &str, project: &str) {
        let Some(ref r2) = self.r2 else { return };
        let prefix = r2_prefix(uid, project);
        let local = self.project_dir(uid, project);
        std::fs::create_dir_all(&local).ok();

        match r2_list_and_download(r2, &prefix, &local).await {
            Ok(n) => { if n > 0 { tracing::debug!("R2 pull {}: {} files", prefix, n); } }
            Err(e) => tracing::error!("R2 pull {}: {}", prefix, e),
        }
    }

    /// Sync user's project TO R2 (after Claude finishes / file changes)
    pub async fn push(&self, uid: &str, project: &str) {
        let Some(ref r2) = self.r2 else { return };
        let prefix = r2_prefix(uid, project);
        let local = self.project_dir(uid, project);
        if !local.is_dir() { return; }

        match r2_upload_dir(r2, &prefix, &local).await {
            Ok(n) => { if n > 0 { tracing::debug!("R2 push {}: {} files", prefix, n); } }
            Err(e) => tracing::error!("R2 push {}: {}", prefix, e),
        }
    }
}

fn r2_prefix(uid: &str, project: &str) -> String {
    if project.is_empty() { format!("u/{}/", uid) }
    else { format!("u/{}/{}/", uid, project) }
}

/// List objects in R2 and download to local cache
async fn r2_list_and_download(r2: &R2Config, prefix: &str, local: &Path) -> Result<usize, String> {
    let client = reqwest::Client::new();
    // Use S3 ListObjectsV2 (R2 is S3-compatible)
    let url = format!("{}/{}?list-type=2&prefix={}", r2.endpoint, r2.bucket, urlencoding::encode(prefix));
    let resp = client.get(&url)
        .basic_auth(&r2.access_key, Some(&r2.secret_key))
        .send().await.map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        return Err(format!("LIST {} → {}", prefix, resp.status()));
    }
    let body = resp.text().await.map_err(|e| e.to_string())?;
    let mut count = 0;

    for chunk in body.split("<Key>").skip(1) {
        let key = chunk.split("</Key>").next().unwrap_or("");
        if key.is_empty() || key.ends_with('/') { continue; }
        let rel = key.strip_prefix(prefix).unwrap_or(key);
        let local_path = local.join(rel);

        if let Some(parent) = local_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        let obj_url = format!("{}/{}/{}", r2.endpoint, r2.bucket, key);
        let obj = client.get(&obj_url)
            .basic_auth(&r2.access_key, Some(&r2.secret_key))
            .send().await.map_err(|e| e.to_string())?;
        if obj.status().is_success() {
            let bytes = obj.bytes().await.map_err(|e| e.to_string())?;
            std::fs::write(&local_path, &bytes).ok();
            count += 1;
        }
    }
    Ok(count)
}

/// Upload local directory to R2
async fn r2_upload_dir(r2: &R2Config, prefix: &str, local: &Path) -> Result<usize, String> {
    let client = reqwest::Client::new();
    let mut files = Vec::new();
    walk_files(local, &mut files);
    let mut count = 0;

    for file in &files {
        let rel = file.strip_prefix(local).unwrap().to_string_lossy().replace('\\', "/");
        let key = format!("{}{}", prefix, rel);
        let body = std::fs::read(file).map_err(|e| e.to_string())?;
        let url = format!("{}/{}/{}", r2.endpoint, r2.bucket, key);

        let resp = client.put(&url)
            .basic_auth(&r2.access_key, Some(&r2.secret_key))
            .header("content-type", "application/octet-stream")
            .body(body)
            .send().await.map_err(|e| e.to_string())?;

        if resp.status().is_success() { count += 1; }
        else { tracing::warn!("R2 PUT {} → {}", key, resp.status()); }
    }
    Ok(count)
}

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            let name = e.file_name().to_string_lossy().to_string();
            // Skip build artifacts and hidden dirs
            if matches!(name.as_str(), "target"|"node_modules"|".git"|".cache"|"__pycache__")
                || name.starts_with('.') { continue; }
            walk_files(&p, out);
        } else {
            // Skip large files (>10MB)
            if e.metadata().map(|m| m.len()).unwrap_or(0) > 10_000_000 { continue; }
            out.push(p);
        }
    }
}
