//! JWT token 持久化（JSON 文件，0600 权限）。
//!
//! 从 deveco-local-model-dashboard/src/token-store.ts 移植。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config;

#[derive(Debug, thiserror::Error)]
pub enum TokenStoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Persistent JWT token store. Thread-safe via interior mutability.
#[derive(Debug, Clone)]
pub struct TokenStore {
    file_path: PathBuf,
    legacy_path: PathBuf,
    // Serialize concurrent save/load; load is rare (startup + refresh) so a
    // simple mutex is fine.
    _lock: Arc<Mutex<()>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredShape {
    jwt: Option<String>,
    saved_at: Option<u64>,
}

fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("OPENCODE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let xdg = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        home.join(".config").to_string_lossy().to_string()
    });
    PathBuf::from(xdg).join("opencode")
}

fn default_token_file_path() -> PathBuf {
    config_dir().join("deveco-local-model-dashboard").join("jwt.json")
}

fn legacy_token_file_path() -> PathBuf {
    let xdg = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        home.join(".config").to_string_lossy().to_string()
    });
    PathBuf::from(xdg).join("deveco-local-model-dashboard").join("jwt.json")
}

fn valid_jwt(jwt: &str) -> bool {
    !jwt.is_empty() && jwt.split('.').count() == 3
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl TokenStore {
    pub fn new() -> Self {
        Self::with_paths(default_token_file_path(), legacy_token_file_path())
    }

    pub fn with_paths(file_path: PathBuf, legacy_path: PathBuf) -> Self {
        Self {
            file_path,
            legacy_path,
            _lock: Arc::new(Mutex::new(())),
        }
    }

    /// Save the jwtToken. Returns false on failure.
    pub async fn save(&self, jwt_token: &str) -> bool {
        if jwt_token.is_empty() {
            return false;
        }
        let _g = self._lock.lock().await;
        let dir = self.file_path.parent().unwrap_or(Path::new("."));
        if let Err(e) = tokio::fs::create_dir_all(dir).await {
            config::log_error(&format!("token-store: mkdir failed: {e}"));
            return false;
        }
        let payload = StoredShape {
            jwt: Some(jwt_token.to_string()),
            saved_at: Some(now_millis()),
        };
        let json = match serde_json::to_string_pretty(&payload) {
            Ok(s) => s,
            Err(e) => {
                config::log_error(&format!("token-store: serialize failed: {e}"));
                return false;
            }
        };
        if let Err(e) = tokio::fs::write(&self.file_path, json).await {
            config::log_error(&format!("token-store: write failed: {e}"));
            return false;
        }
        // best-effort chmod 0600 on unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(&self.file_path, std::fs::Permissions::from_mode(0o600)).await;
        }
        // Remove stale legacy copy
        if self.legacy_path != self.file_path && self.legacy_path.exists() {
            let _ = tokio::fs::remove_file(&self.legacy_path).await;
        }
        true
    }

    /// Load the jwtToken, or None if absent/corrupt.
    pub async fn load(&self) -> Option<String> {
        let _g = self._lock.lock().await;
        let canonical = self.read_token_file(&self.file_path).await;
        let legacy = if self.legacy_path != self.file_path {
            self.read_token_file(&self.legacy_path).await
        } else {
            None
        };

        match (canonical, legacy) {
            (Some(c), Some(l)) => {
                // Newest savedAt wins
                let c_time = c.saved_at.unwrap_or(0);
                let l_time = l.saved_at.unwrap_or(0);
                let l_jwt = l.jwt.unwrap_or_default();
                if l_time > c_time {
                    // Migrate legacy to canonical
                    let _ = self.save_inner(&l_jwt).await;
                    Some(l_jwt)
                } else {
                    // TS: canonical wins → removeLegacyCopy()
                    if self.legacy_path != self.file_path && self.legacy_path.exists() {
                        let _ = tokio::fs::remove_file(&self.legacy_path).await;
                    }
                    Some(c.jwt.unwrap_or_default())
                }
            }
            (Some(c), None) => {
                // TS: removeLegacyCopy() when canonical exists
                if self.legacy_path != self.file_path && self.legacy_path.exists() {
                    let _ = tokio::fs::remove_file(&self.legacy_path).await;
                }
                Some(c.jwt.unwrap_or_default())
            }
            (None, Some(l)) => {
                let jwt = l.jwt.unwrap_or_default();
                let _ = self.save_inner(&jwt).await;
                Some(jwt)
            }
            (None, None) => None,
        }
    }

    /// Remove the stored jwtToken.
    pub async fn clear(&self) {
        let _g = self._lock.lock().await;
        let _ = tokio::fs::remove_file(&self.file_path).await;
        if self.legacy_path != self.file_path {
            let _ = tokio::fs::remove_file(&self.legacy_path).await;
        }
    }

    async fn save_inner(&self, jwt_token: &str) -> Result<(), TokenStoreError> {
        let dir = self.file_path.parent().unwrap_or(Path::new("."));
        tokio::fs::create_dir_all(dir).await?;
        let payload = StoredShape {
            jwt: Some(jwt_token.to_string()),
            saved_at: Some(now_millis()),
        };
        let json = serde_json::to_string_pretty(&payload)?;
        tokio::fs::write(&self.file_path, json).await?;
        Ok(())
    }

    async fn read_token_file(&self, path: &Path) -> Option<StoredShape> {
        let raw = match tokio::fs::read_to_string(path).await {
            Ok(s) => s,
            Err(_) => return None,
        };
        let parsed: StoredShape = match serde_json::from_str(&raw) {
            Ok(p) => p,
            Err(_) => {
                let _ = tokio::fs::remove_file(path).await;
                return None;
            }
        };
        match &parsed.jwt {
            Some(jwt) if valid_jwt(jwt) => Some(parsed),
            _ => {
                let _ = tokio::fs::remove_file(path).await;
                None
            }
        }
    }
}

impl Default for TokenStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn save_and_load() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("jwt.json");
        let store = TokenStore::with_paths(path.clone(), path.clone());
        let jwt = "aaa.bbb.ccc";
        assert!(store.save(jwt).await);
        let loaded = store.load().await;
        assert_eq!(loaded.as_deref(), Some(jwt));
    }

    #[tokio::test]
    async fn load_missing_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nonexistent.json");
        let store = TokenStore::with_paths(path.clone(), path);
        assert!(store.load().await.is_none());
    }

    #[tokio::test]
    async fn clear_removes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("jwt.json");
        let store = TokenStore::with_paths(path.clone(), path.clone());
        store.save("aaa.bbb.ccc").await;
        store.clear().await;
        assert!(!path.exists());
    }
}
