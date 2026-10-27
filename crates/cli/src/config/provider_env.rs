//! Machine-scoped provider credential env file.
//!
//! `provider_env_file` names ONE absolute, owner-only file of `KEY=VALUE`
//! lines. The daemon loads it during the boot preflight, BEFORE the provider
//! secret registry and the provider adapters are built, so
//! `providers[].api_key_env` resolves without putting key values in the
//! config, in argv or in logs. A variable already set in the process
//! environment is never overwritten: an explicit operator export stays
//! authoritative.
//!
//! The file is a credential document, not a general shell script: it is
//! bounded, must not be readable or writable by group/other on unix, and
//! only plain `KEY=VALUE` / `export KEY=VALUE` lines (with optional matching
//! quotes and full-line comments) are accepted. Error text names the path and
//! line, never a value.

use super::*;
use std::io::Read;

/// Maximum size of the provider env file (far above any real credential set,
/// far below an accidental dump of the machine environment).
pub const MAX_PROVIDER_ENV_BYTES: u64 = 64 * 1024;

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn unquote(raw: &str) -> Option<&str> {
    if let Some(rest) = raw.strip_prefix('"') {
        return rest.strip_suffix('"');
    }
    if let Some(rest) = raw.strip_prefix('\'') {
        return rest.strip_suffix('\'');
    }
    Some(raw)
}

/// Load the configured provider env file into the process environment.
///
/// Returns the number of variables actually installed (variables already
/// present with a non-empty value are left untouched). The caller must run
/// this BEFORE [`super::ProviderCfg::key`] is consulted for the secret
/// registry or provider construction.
pub fn load_provider_env_file(config: &Config) -> Result<usize, String> {
    let Some(raw) = config.provider_env_file.as_deref() else {
        return Ok(0);
    };
    let path = Path::new(raw);
    if raw.is_empty() || !path.is_absolute() {
        return Err(
            "provider_env_file must be a non-empty absolute path; relative paths would \
             depend on an uncontrolled working directory"
                .into(),
        );
    }
    let file = std::fs::File::open(path)
        .map_err(|e| format!("provider_env_file {}: {e}", path.display()))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("provider_env_file {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!(
            "provider_env_file {} is not a regular file",
            path.display()
        ));
    }
    if meta.len() > MAX_PROVIDER_ENV_BYTES {
        return Err(format!(
            "provider_env_file {} is {} bytes; the maximum is {}",
            path.display(),
            meta.len(),
            MAX_PROVIDER_ENV_BYTES
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "provider_env_file {} must not be readable or writable by group/other \
                 (chmod 600)",
                path.display()
            ));
        }
    }
    let mut bytes = Vec::new();
    file.take(MAX_PROVIDER_ENV_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("provider_env_file {}: {e}", path.display()))?;
    if bytes.len() as u64 > MAX_PROVIDER_ENV_BYTES {
        return Err(format!(
            "provider_env_file {} grew past the {} byte maximum while being read",
            path.display(),
            MAX_PROVIDER_ENV_BYTES
        ));
    }
    let text = String::from_utf8(bytes).map_err(|e| {
        format!(
            "provider_env_file {} is not UTF-8 (invalid byte at offset {})",
            path.display(),
            e.utf8_error().valid_up_to()
        )
    })?;
    let mut installed = 0usize;
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let body = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((name, value)) = body.split_once('=') else {
            return Err(format!(
                "provider_env_file {} line {}: expected KEY=VALUE",
                path.display(),
                index + 1
            ));
        };
        let name = name.trim();
        if !valid_env_name(name) {
            return Err(format!(
                "provider_env_file {} line {}: invalid environment variable name",
                path.display(),
                index + 1
            ));
        }
        let value = unquote(value.trim()).ok_or_else(|| {
            format!(
                "provider_env_file {} line {}: unmatched quote",
                path.display(),
                index + 1
            )
        })?;
        if value.contains('\0') {
            return Err(format!(
                "provider_env_file {} line {}: NUL is not allowed in a value",
                path.display(),
                index + 1
            ));
        }
        // An explicit process export wins; an absent or empty variable is
        // filled from the file.
        if std::env::var_os(name).is_none_or(|existing| existing.is_empty()) {
            std::env::set_var(name, value);
            installed += 1;
        }
    }
    Ok(installed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    fn write_env(text: &str, mode: u32) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("provider.env");
        std::fs::write(&path, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        #[cfg(not(unix))]
        let _ = mode;
        (dir, path)
    }

    fn config_for(path: &std::path::Path) -> Config {
        Config {
            provider_env_file: Some(path.to_string_lossy().into_owned()),
            ..Config::default()
        }
    }

    #[test]
    fn loads_plain_export_quoted_and_commented_lines() {
        let _guard = guard();
        let key_a = "FAKTOR_TEST_PROVIDER_ENV_PLAIN";
        let key_b = "FAKTOR_TEST_PROVIDER_ENV_EXPORTED";
        let key_c = "FAKTOR_TEST_PROVIDER_ENV_QUOTED";
        let key_d = "FAKTOR_TEST_PROVIDER_ENV_COMMENTED";
        for key in [key_a, key_b, key_c, key_d] {
            std::env::remove_var(key);
        }
        let (_dir, path) = write_env(
            &format!(
                "# machine-scoped provider credentials\n\
                 \n\
                 {key_a}=alpha\n\
                 export {key_b} = beta\n\
                 {key_c}=\"value=with=equals # not a comment\"\n\
                 {key_d}='quoted'\n"
            ),
            0o600,
        );
        let installed = load_provider_env_file(&config_for(&path)).unwrap();
        assert_eq!(installed, 4);
        assert_eq!(std::env::var(key_a).unwrap(), "alpha");
        assert_eq!(std::env::var(key_b).unwrap(), "beta");
        assert_eq!(
            std::env::var(key_c).unwrap(),
            "value=with=equals # not a comment"
        );
        assert_eq!(std::env::var(key_d).unwrap(), "quoted");
        for key in [key_a, key_b, key_c, key_d] {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn process_environment_wins_and_empty_is_filled() {
        let _guard = guard();
        let key_present = "FAKTOR_TEST_PROVIDER_ENV_PRESENT";
        let key_empty = "FAKTOR_TEST_PROVIDER_ENV_EMPTY";
        std::env::set_var(key_present, "from-process");
        std::env::set_var(key_empty, "");
        let (_dir, path) = write_env(
            &format!("{key_present}=from-file\n{key_empty}=from-file\n"),
            0o600,
        );
        let installed = load_provider_env_file(&config_for(&path)).unwrap();
        assert_eq!(installed, 1, "only the empty variable is filled");
        assert_eq!(std::env::var(key_present).unwrap(), "from-process");
        assert_eq!(std::env::var(key_empty).unwrap(), "from-file");
        std::env::remove_var(key_present);
        std::env::remove_var(key_empty);
    }

    #[test]
    fn refuses_insecure_malformed_and_oversized_documents() {
        let _guard = guard();
        let cases = [
            ("NOT A LINE", 0o600, "expected KEY=VALUE", ""),
            (
                "1BAD=PLANTED",
                0o600,
                "invalid environment variable name",
                "PLANTED",
            ),
            ("GOOD=\"PLANTED", 0o600, "unmatched quote", "PLANTED"),
            ("GOOD=PLANTED\0bad", 0o600, "NUL is not allowed", "PLANTED"),
        ];
        for (text, mode, needle, planted) in cases {
            let (_dir, path) = write_env(text, mode);
            let error = load_provider_env_file(&config_for(&path)).unwrap_err();
            assert!(error.contains(needle), "expected {needle:?} in {error:?}");
            if !planted.is_empty() {
                assert!(
                    !error.contains(planted),
                    "a refusal must not echo the value: {error:?}"
                );
            }
        }
        // Group/other access is refused before any line is read.
        let (_dir, path) = write_env("GOOD=value\n", 0o644);
        let error = load_provider_env_file(&config_for(&path)).unwrap_err();
        assert!(error.contains("chmod 600"), "{error}");
        // Oversized documents are refused by the bounded read.
        let (_dir, path) = write_env(&format!("GOOD={}\n", "x".repeat(70 * 1024)), 0o600);
        let error = load_provider_env_file(&config_for(&path)).unwrap_err();
        assert!(error.contains("maximum"), "{error}");
        // Relative and missing paths are typed refusals.
        let relative = Config {
            provider_env_file: Some("relative.env".into()),
            ..Config::default()
        };
        let error = load_provider_env_file(&relative).unwrap_err();
        assert!(error.contains("absolute"), "{error}");
        let missing = Config {
            provider_env_file: Some("/nonexistent/provider.env".into()),
            ..Config::default()
        };
        assert!(load_provider_env_file(&missing).is_err());
    }

    #[test]
    fn preflight_loads_the_file_before_provider_construction() {
        let _guard = guard();
        let key = "FAKTOR_TEST_PROVIDER_ENV_PREFLIGHT";
        std::env::remove_var(key);
        let (_dir, path) = write_env(&format!("{key}=sk-test-not-a-real-key\n"), 0o600);
        let config = Config {
            provider_env_file: Some(path.to_string_lossy().into_owned()),
            model: "test-model".into(),
            providers: vec![ProviderCfg::OpenAi {
                id: "envtest".into(),
                base_url: "http://127.0.0.1:9/v1".into(),
                api_key_env: Some(key.into()),
                api: None,
                allow_loopback: true,
                models: vec!["test-model".into()],
                pricing: None,
                quality: None,
            }],
            routing_mode: Some(RoutingMode::Pinned {
                provider: "envtest".into(),
                model: "test-model".into(),
            }),
            ..Config::default()
        };
        crate::daemon::builder::preflight_daemon_config(&config).expect("preflight");
        assert_eq!(
            std::env::var(key).ok().as_deref(),
            Some("sk-test-not-a-real-key"),
            "the preflight must load the declared env file"
        );
        assert!(
            config.providers[0].key().is_some(),
            "the provider key must resolve before the secret registry/adapters are built"
        );
        std::env::remove_var(key);
    }
}
