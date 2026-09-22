//! Downloads (spec §9): **disabled by default**. The browser authority
//! never writes a file from a page unless an operator explicitly enables
//! downloads for the profile, and even then every download stays inside a
//! profile-scoped directory under a byte bound (an over-bound download is
//! cancelled, never silently completed).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::error::BrowserError;

/// Download policy for one browser profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadPolicy {
    /// Disabled by default; enabling is an explicit operator decision.
    pub enabled: bool,
    /// Required when enabled: a directory inside the profile.
    pub directory: Option<PathBuf>,
    /// Best-effort byte bound enforced on `Browser.downloadProgress`
    /// (0 = refuse to enable).
    pub max_bytes: u64,
}

impl Default for DownloadPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            directory: None,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

/// The decision for one `Browser.downloadWillBegin`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadDecision {
    Denied {
        url: String,
    },
    Allowed {
        guid: String,
        url: String,
        suggested_filename: String,
    },
}

impl DownloadPolicy {
    /// Validate the policy against the profile directory it would write to:
    /// an enabled policy must name a directory inside the profile, and the
    /// byte bound must be positive.
    pub fn validate(&self, profile_dir: &Path) -> Result<(), BrowserError> {
        if !self.enabled {
            return Ok(());
        }
        let Some(directory) = &self.directory else {
            return Err(BrowserError::invalid_config(
                "downloads enabled without a profile-scoped directory",
            ));
        };
        if !directory.starts_with(profile_dir) || directory == profile_dir {
            return Err(BrowserError::invalid_config(format!(
                "download directory {directory:?} must live inside the profile directory"
            )));
        }
        if self.max_bytes == 0 {
            return Err(BrowserError::invalid_config(
                "download max_bytes must be > 0",
            ));
        }
        Ok(())
    }

    /// Create the download directory (0700) when enabled.
    pub fn prepare(&self) -> Result<(), BrowserError> {
        if !self.enabled {
            return Ok(());
        }
        let Some(directory) = &self.directory else {
            return Err(BrowserError::invalid_config(
                "downloads enabled without a directory",
            ));
        };
        std::fs::create_dir_all(directory).map_err(|e| {
            BrowserError::profile(format!(
                "cannot create download directory {directory:?}: {e}"
            ))
        })?;
        crate::profile::restrict_dir(directory)
    }

    /// The CDP command that installs this policy for the browser.
    pub fn set_behavior_command(&self) -> (&'static str, Value) {
        if self.enabled {
            let path = self
                .directory
                .as_ref()
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or_default();
            (
                "Browser.setDownloadBehavior",
                json!({ "behavior": "allow", "downloadPath": path, "eventsEnabled": true }),
            )
        } else {
            (
                "Browser.setDownloadBehavior",
                json!({ "behavior": "deny", "eventsEnabled": true }),
            )
        }
    }

    /// Decide one `Browser.downloadWillBegin` event. Denied downloads are
    /// cancelled through CDP and surface as a typed error to the caller.
    pub fn decide(&self, params: &Value) -> DownloadDecision {
        let url = params
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !self.enabled {
            return DownloadDecision::Denied { url };
        }
        DownloadDecision::Allowed {
            guid: params
                .get("guid")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            url,
            suggested_filename: params
                .get("suggestedFilename")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }
    }

    /// Should an in-flight download be cancelled for exceeding the byte
    /// bound? Returns the guid to cancel.
    pub fn over_bound_guid(&self, params: &Value) -> Option<String> {
        if !self.enabled {
            return params
                .get("guid")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        let received = params
            .get("receivedBytes")
            .and_then(Value::as_f64)
            .map(|n| n.max(0.0) as u64)
            .unwrap_or(0);
        let total = params
            .get("totalBytes")
            .and_then(Value::as_f64)
            .map(|n| n.max(0.0) as u64)
            .unwrap_or(0);
        if received > self.max_bytes || total > self.max_bytes {
            return params
                .get("guid")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        None
    }

    /// `Browser.downloadWillBegin` handling helper: the typed error a caller
    /// observes for a denied download.
    pub fn denied_error(&self, url: &str) -> BrowserError {
        BrowserError::DownloadBlocked {
            url: url.to_string(),
        }
    }
}

/// Bounded download-observation counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DownloadStats {
    pub blocked_total: u64,
    pub allowed_total: u64,
    pub cancelled_over_bound: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downloads_are_denied_by_default_and_never_write() {
        let policy = DownloadPolicy::default();
        assert!(!policy.enabled);
        let decision = policy.decide(&json!({
            "guid": "g1",
            "url": "https://first.test/file.pdf",
            "suggestedFilename": "file.pdf"
        }));
        assert_eq!(
            decision,
            DownloadDecision::Denied {
                url: "https://first.test/file.pdf".to_string()
            }
        );
        let (method, params) = policy.set_behavior_command();
        assert_eq!(method, "Browser.setDownloadBehavior");
        assert_eq!(params["behavior"], "deny");
        assert!(policy.prepare().is_ok());
    }

    #[test]
    fn enabled_downloads_must_stay_inside_the_profile() {
        let profile = PathBuf::from("/data/commerce/profiles/p1");
        let mut policy = DownloadPolicy {
            enabled: true,
            directory: Some(PathBuf::from("/tmp/outside")),
            max_bytes: 1024,
        };
        assert!(policy.validate(&profile).is_err());
        policy.directory = Some(profile.join("downloads"));
        assert!(policy.validate(&profile).is_ok());
        assert_eq!(
            policy.over_bound_guid(&json!({"guid": "g1", "receivedBytes": 2048})),
            Some("g1".to_string())
        );
        assert_eq!(
            policy.over_bound_guid(&json!({"guid": "g1", "receivedBytes": 12})),
            None
        );
        let (_, params) = policy.set_behavior_command();
        assert_eq!(params["behavior"], "allow");
        assert_eq!(
            params["downloadPath"],
            "/data/commerce/profiles/p1/downloads"
        );
    }

    #[test]
    fn enabled_without_directory_is_refused() {
        let policy = DownloadPolicy {
            enabled: true,
            directory: None,
            max_bytes: 1,
        };
        assert!(policy
            .validate(Path::new("/data/commerce/profiles/p1"))
            .is_err());
    }
}
