//! Storage abstraction: local filesystem or S3/R2 (with proper AWS SigV4)
//!
//! Set R2_ENDPOINT, R2_BUCKET, R2_ACCESS_KEY_ID, R2_SECRET_ACCESS_KEY
//! to enable R2. Without them: pure local (zero overhead).

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
    pub region: String,
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
                Some(R2Config {
                    endpoint: e, bucket: b,
                    region: std::env::var("R2_REGION").unwrap_or_else(|_| "auto".into()),
                    access_key: a, secret_key: s,
                })
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

    #[allow(dead_code)]
    pub fn is_r2(&self) -> bool { self.r2.is_some() }

    /// Pull from R2 → local cache (before Claude runs)
    pub async fn pull(&self, uid: &str, project: &str) {
        let Some(ref r2) = self.r2 else { return };
        let prefix = r2_prefix(uid, project);
        let local = self.project_dir(uid, project);
        std::fs::create_dir_all(&local).ok();

        match r2_pull(r2, &prefix, &local).await {
            Ok(n) => { if n > 0 { tracing::debug!("R2 pull {}: {} files", prefix, n); } }
            Err(e) => tracing::error!("R2 pull {}: {}", prefix, e),
        }
    }

    /// Push local → R2 (after Claude finishes)
    pub async fn push(&self, uid: &str, project: &str) {
        let Some(ref r2) = self.r2 else { return };
        let prefix = r2_prefix(uid, project);
        let local = self.project_dir(uid, project);
        if !local.is_dir() { return; }

        match r2_push(r2, &prefix, &local).await {
            Ok(n) => { if n > 0 { tracing::debug!("R2 push {}: {} files", prefix, n); } }
            Err(e) => tracing::error!("R2 push {}: {}", prefix, e),
        }
    }
}

fn r2_prefix(uid: &str, project: &str) -> String {
    if project.is_empty() { format!("u/{}/", uid) }
    else { format!("u/{}/{}/", uid, project) }
}

fn make_bucket(r2: &R2Config) -> Result<s3::Bucket, String> {
    let region = s3::Region::Custom {
        region: r2.region.clone(),
        endpoint: r2.endpoint.clone(),
    };
    let creds = s3::creds::Credentials::new(
        Some(&r2.access_key), Some(&r2.secret_key), None, None, None
    ).map_err(|e| e.to_string())?;

    let bucket = s3::Bucket::new(&r2.bucket, region, creds)
        .map_err(|e| e.to_string())?;
    Ok(bucket.with_path_style())
}

async fn r2_pull(r2: &R2Config, prefix: &str, local: &Path) -> Result<usize, String> {
    let bucket = make_bucket(r2)?;
    let list = bucket.list(prefix.to_string(), None).await.map_err(|e| e.to_string())?;
    let mut count = 0;

    for result in &list {
        for obj in &result.contents {
            let key = &obj.key;
            if key.ends_with('/') { continue; }
            let rel = key.strip_prefix(prefix).unwrap_or(key);
            let local_path = local.join(rel);

            if let Some(parent) = local_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }

            match bucket.get_object(key).await {
                Ok(resp) => {
                    std::fs::write(&local_path, resp.bytes()).ok();
                    count += 1;
                }
                Err(e) => tracing::warn!("R2 GET {}: {}", key, e),
            }
        }
    }
    Ok(count)
}

async fn r2_push(r2: &R2Config, prefix: &str, local: &Path) -> Result<usize, String> {
    let bucket = make_bucket(r2)?;
    let mut files = Vec::new();
    walk_files(local, &mut files);
    let mut count = 0;

    for file in &files {
        let rel = file.strip_prefix(local).unwrap().to_string_lossy().replace('\\', "/");
        let key = format!("{}{}", prefix, rel);
        let body = std::fs::read(file).map_err(|e| e.to_string())?;

        match bucket.put_object(&key, &body).await {
            Ok(_) => count += 1,
            Err(e) => tracing::warn!("R2 PUT {}: {}", key, e),
        }
    }
    Ok(count)
}

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            let name = e.file_name().to_string_lossy().to_string();
            if matches!(name.as_str(), "target"|"node_modules"|".git"|".cache"|"__pycache__")
                || name.starts_with('.') { continue; }
            walk_files(&p, out);
        } else {
            if e.metadata().map(|m| m.len()).unwrap_or(0) > 10_000_000 { continue; }
            out.push(p);
        }
    }
}
