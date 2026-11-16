//! OAuth Web API tokens are distinct from librespot's reusable AP credentials.
//! Keep the granted access/refresh token and scope set; never replace it with a
//! newly requested streaming-only token when calling /v1/me/*.

use librespot_oauth::{DeviceAuthClientBuilder, OAuthToken};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use crate::engine::{KEYMASTER_CLIENT_ID, LOGIN_SCOPES};

#[derive(Clone, Serialize, Deserialize)]
struct SavedToken {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
    scopes: Vec<String>,
}

impl SavedToken {
    fn from_grant(token: OAuthToken) -> Self {
        Self {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_at: now().saturating_add(
                token
                    .expires_at
                    .saturating_duration_since(std::time::Instant::now())
                    .as_secs(),
            ),
            scopes: token.scopes,
        }
    }

    fn permits(&self, scope: Option<&str>) -> bool {
        scope.is_none_or(|s| self.scopes.iter().any(|granted| granted == s))
    }

    fn is_fresh(&self) -> bool {
        !self.access_token.is_empty() && self.expires_at > now().saturating_add(60)
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub struct OAuthStore {
    path: PathBuf,
    upgrade_path: PathBuf,
    token: Mutex<Option<SavedToken>>,
}

impl OAuthStore {
    pub fn new(dir: &Path) -> Self {
        let path = dir.join("oauth-token.json");
        // No raw JSON/serde error is logged: malformed files may contain secrets.
        let saved = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let upgrade_path = dir.join("oauth-scope-upgrade-required");
        let has_ap_credentials =
            dir.join("credentials").exists() || dir.join("credentials.json").exists();
        let legacy_grant = saved.as_ref().is_some_and(|s: &SavedToken| {
            !s.scopes.iter().any(|scope| scope == "user-library-read")
                || !s
                    .scopes
                    .iter()
                    .any(|scope| scope == "playlist-read-private")
        });
        if (legacy_grant || (saved.is_none() && has_ap_credentials)) && !upgrade_path.exists() {
            // 0.3.0 stored AP credentials but not the device-flow Web API grant.
            // Persist a non-secret marker so old installs get an explicit re-login
            // prompt rather than silently treating a missing OAuth grant as logged out.
            let _ = std::fs::write(&upgrade_path, b"scope upgrade required");
        }
        Self {
            path,
            upgrade_path,
            token: Mutex::new(saved),
        }
    }

    pub async fn save_grant(&self, token: OAuthToken) -> Result<(), String> {
        let saved = SavedToken::from_grant(token);
        if saved.access_token.is_empty() {
            return Err("Spotify returned an empty OAuth token".into());
        }
        self.persist(&saved)?;
        if saved.scopes.iter().any(|s| s == "user-library-read")
            && saved.scopes.iter().any(|s| s == "playlist-read-private")
        {
            let _ = std::fs::remove_file(&self.upgrade_path);
        }
        *self.token.lock().await = Some(saved);
        Ok(())
    }

    /// None means this is a pre-0.3.1 install; the caller may acquire an explicit
    /// scope token through librespot once, but should ask the user to re-authorize.
    pub fn requires_scope_upgrade(&self) -> bool {
        self.upgrade_path.exists()
    }

    pub async fn has_saved_grant(&self) -> bool {
        self.token.lock().await.is_some()
    }

    pub async fn access_token(
        &self,
        required_scope: Option<&str>,
    ) -> Result<Option<String>, String> {
        let mut guard = self.token.lock().await;
        let Some(saved) = guard.as_mut() else {
            if self.upgrade_path.exists() && required_scope.is_some() {
                return Err("Spotify 旧授权缺少读取个人曲库权限，请重新进行浏览器登录".into());
            }
            return Ok(None);
        };
        if !saved.permits(required_scope) {
            return Err("Spotify 授权缺少个人曲库权限，请重新进行浏览器登录".into());
        }
        if !saved.is_fresh() {
            if saved.refresh_token.is_empty() {
                return Err("Spotify 授权已过期，请重新登录".into());
            }
            let client = DeviceAuthClientBuilder::new(KEYMASTER_CLIENT_ID, LOGIN_SCOPES.to_vec())
                .build()
                .map_err(|_| "Spotify OAuth client initialization failed".to_string())?;
            let fresh = tokio::time::timeout(
                Duration::from_secs(10),
                client.refresh_token_async(&saved.refresh_token),
            )
            .await
            .map_err(|_| "Spotify token refresh timed out".to_string())?
            .map_err(|_| "Spotify 授权刷新失败，请重新登录".to_string())?;
            let mut next = SavedToken::from_grant(fresh);
            // RFC 6749 allows a refresh response to omit refresh_token. Scope
            // omission means unchanged grants, not new authority.
            if next.refresh_token.is_empty() {
                next.refresh_token.clone_from(&saved.refresh_token);
            }
            next.scopes.clone_from(&saved.scopes);
            self.persist(&next)?;
            *saved = next;
        }
        Ok(Some(saved.access_token.clone()))
    }

    pub async fn mark_upgrade_required(&self) {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        if let Ok(mut f) = options.open(&self.upgrade_path) {
            let _ = f.write_all(b"scope upgrade required");
        }
    }

    pub async fn clear(&self) {
        *self.token.lock().await = None;
        let _ = std::fs::remove_file(&self.upgrade_path);
        // This exact private credential file belongs to the store. No computed
        // directory deletion and no provider-supplied paths are accepted.
        if self
            .path
            .file_name()
            .is_some_and(|n| n == "oauth-token.json")
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn persist(&self, token: &SavedToken) -> Result<(), String> {
        use std::io::Write;
        let bytes = serde_json::to_vec(token)
            .map_err(|_| "OAuth credential encoding failed".to_string())?;
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&self.path)
            .map_err(|_| "OAuth credential storage unavailable".to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|_| "OAuth credential permissions unavailable".to_string())?;
        }
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| "OAuth credential storage failed".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(scopes: &[&str]) -> OAuthToken {
        OAuthToken {
            access_token: "test-access-not-real".into(),
            refresh_token: "test-refresh-not-real".into(),
            expires_at: std::time::Instant::now() + Duration::from_secs(3600),
            token_type: "Bearer".into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[tokio::test]
    async fn persists_and_restores_scoped_grant() {
        let dir = tempfile::tempdir().unwrap();
        let first = OAuthStore::new(dir.path());
        first
            .save_grant(grant(&["streaming", "user-library-read"]))
            .await
            .unwrap();
        let second = OAuthStore::new(dir.path());
        assert_eq!(
            second
                .access_token(Some("user-library-read"))
                .await
                .unwrap()
                .as_deref(),
            Some("test-access-not-real")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.path().join("oauth-token.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        second.clear().await;
        assert!(!dir.path().join("oauth-token.json").exists());
        assert!(second.access_token(None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn missing_scope_is_not_upgraded_or_silently_used() {
        let dir = tempfile::tempdir().unwrap();
        let store = OAuthStore::new(dir.path());
        store.save_grant(grant(&["streaming"])).await.unwrap();
        let error = store
            .access_token(Some("playlist-read-private"))
            .await
            .unwrap_err();
        assert!(error.contains("重新"));
        assert!(!error.contains("test-access"));
    }

    #[tokio::test]
    async fn old_ap_credentials_require_a_browser_scope_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("credentials"), b"not a real credential").unwrap();
        let store = OAuthStore::new(dir.path());
        assert!(store.requires_scope_upgrade());
        let error = store
            .access_token(Some("user-library-read"))
            .await
            .unwrap_err();
        assert!(error.contains("重新"));
        store
            .save_grant(grant(&[
                "streaming",
                "user-library-read",
                "playlist-read-private",
            ]))
            .await
            .unwrap();
        assert!(!store.requires_scope_upgrade());
        assert!(
            store
                .access_token(Some("user-library-read"))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn legacy_install_has_no_oauth_grant() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            OAuthStore::new(dir.path())
                .access_token(None)
                .await
                .unwrap()
                .is_none()
        );
    }
}
