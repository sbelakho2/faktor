use crate::util::{self, Result};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

const API_DEFAULT: &str = "https://api.github.com";
const MAX_DESCRIPTION: usize = 140;

type HmacSha256 = Hmac<Sha256>;

fn fail(code: &str, detail: impl AsRef<str>) -> String {
    format!("github-status-{code}: {}", detail.as_ref())
}

fn arg(args: &[String], name: &str, default: &str) -> Result<String> {
    if let Some(index) = args.iter().position(|arg| arg == name) {
        return args
            .get(index + 1)
            .cloned()
            .ok_or_else(|| fail("args", format!("{name} requires a value")));
    }
    Ok(default.to_string())
}

fn platform_signing_payload(
    platform: &str,
    sha: &str,
    tree: &str,
    run: &str,
    state: &str,
    context: &str,
) -> String {
    [
        "faktor-platform-cert/v1",
        &format!("platform={platform}"),
        &format!("sha={sha}"),
        &format!("tree={tree}"),
        &format!("run={run}"),
        &format!("state={state}"),
        &format!("context={context}"),
    ]
    .join("\n")
}

fn platform_signature(
    platform: &str,
    sha: &str,
    tree: &str,
    run: &str,
    state: &str,
    context: &str,
    key: &str,
) -> String {
    let payload = platform_signing_payload(platform, sha, tree, run, state, context);
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).expect("HMAC accepts any key");
    mac.update(payload.as_bytes());
    util::hex(&mac.finalize().into_bytes())
}

fn normalize_key(value: &str) -> String {
    let mut output = String::new();
    for ch in value.to_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            output.push(ch);
        } else {
            output.push('_');
        }
    }
    output
}

fn platform_key_from_env(platform: &str, environ: Option<&HashMap<String, String>>) -> String {
    let normalized = normalize_key(platform);
    for name in [
        format!("faktor_platform_status_key_{normalized}"),
        format!("FAKTOR_PLATFORM_STATUS_KEY_{}", normalized.to_uppercase()),
    ] {
        let value = match environ {
            Some(environ) => environ.get(&name).cloned().unwrap_or_default(),
            None => std::env::var(&name).unwrap_or_default(),
        };
        if !value.is_empty() {
            return value;
        }
    }
    String::new()
}

fn is_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .chars()
            .all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch))
}

fn is_repo(value: &str) -> bool {
    let mut parts = value.split('/');
    let Some(owner) = parts.next() else {
        return false;
    };
    let Some(repo) = parts.next() else {
        return false;
    };
    if parts.next().is_some() || owner.is_empty() || repo.is_empty() {
        return false;
    }
    owner
        .chars()
        .chain(repo.chars())
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
}

fn validate(
    repo: &str,
    sha: &str,
    state: &str,
    context: &str,
    description: &str,
    target_url: &str,
) -> Result<()> {
    if !is_repo(repo) {
        return Err(fail("repo", format!("invalid repository {repo:?}")));
    }
    if !is_sha(sha) {
        return Err(fail(
            "sha",
            "--sha must be the exact 40-lowercase-hex commit",
        ));
    }
    if !matches!(state, "success" | "failure" | "error" | "pending") {
        return Err(fail(
            "state",
            "--state must be one of [\"error\", \"failure\", \"pending\", \"success\"]",
        ));
    }
    if context.is_empty() || context.chars().any(char::is_whitespace) {
        return Err(fail(
            "context",
            "--context must be a non-empty status context",
        ));
    }
    if description.is_empty() || description.len() > MAX_DESCRIPTION {
        return Err(fail(
            "description",
            format!("--description must be 1..{MAX_DESCRIPTION} characters"),
        ));
    }
    if !target_url.is_empty() && !target_url.starts_with("https://") {
        return Err(fail(
            "target-url",
            "--target-url must be an https URL when set",
        ));
    }
    Ok(())
}

fn token_from_env() -> String {
    for name in ["GITHUB_STATUS_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(value) = std::env::var(name) {
            if !value.is_empty() {
                return value;
            }
        }
    }
    String::new()
}

#[allow(clippy::too_many_arguments)]
fn publish(
    api: &str,
    repo: &str,
    sha: &str,
    state: &str,
    context: &str,
    description: &str,
    target_url: &str,
    token: &str,
    platform: &str,
    tree: &str,
    run_prefix: &str,
) -> Result<()> {
    let mut description = description.to_string();
    if !platform.is_empty() {
        if !is_sha(tree) {
            return Err(fail(
                "tree",
                format!("--platform {platform} requires --tree as the exact 40-lowercase-hex tree"),
            ));
        }
        if run_prefix.is_empty() {
            return Err(fail(
                "run-prefix",
                format!("--platform {platform} requires --run-prefix (or CI_PIPELINE_NUMBER)"),
            ));
        }
        let run = format!("{run_prefix}:{platform}");
        if state == "success" {
            let key = platform_key_from_env(platform, None);
            if key.is_empty() {
                return Err(fail(
                    "platform-key-missing",
                    format!(
                        "no faktor_platform_status_key_{platform} secret is configured; an unsigned platform certificate can never be published"
                    ),
                ));
            }
            let signature = platform_signature(platform, sha, tree, &run, state, context, &key);
            description = format!("tree={tree} run={run} sig={signature}");
        } else {
            description = format!("tree={tree} run={run} state={state}");
        }
    }
    validate(repo, sha, state, context, &description, target_url)?;
    if token.is_empty() {
        return Err(fail(
            "token-missing",
            "GITHUB_STATUS_TOKEN/GH_TOKEN/GITHUB_TOKEN is required (fail closed)",
        ));
    }
    let mut body = serde_json::Map::new();
    body.insert("state".into(), json!(state));
    body.insert("context".into(), json!(context));
    body.insert("description".into(), json!(description));
    if !target_url.is_empty() {
        body.insert("target_url".into(), json!(target_url));
    }
    let body_text =
        serde_json::to_string(&Value::Object(body)).map_err(|error| error.to_string())?;
    let body_path = std::env::temp_dir().join(format!(
        "faktor-github-status-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&body_path, body_text.as_bytes()).map_err(|error| error.to_string())?;
    let url = format!("{}/repos/{repo}/statuses/{sha}", api.trim_end_matches('/'));
    let output = Command::new("curl")
        .args([
            "-sS",
            "-X",
            "POST",
            "--max-time",
            "30",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-H",
            &format!("Authorization: Bearer {token}"),
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "Content-Type: application/json",
            "-H",
            "User-Agent: faktor-certification",
            "--data-binary",
            &format!("@{}", body_path.display()),
            &url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    let _ = std::fs::remove_file(&body_path);
    let output = output.map_err(|error| fail("publish-failed", format!("transport: {error}")))?;
    let code = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if code != "200" && code != "201" {
        let detail = if code.is_empty() {
            String::from_utf8_lossy(&output.stderr).trim().to_string()
        } else {
            format!("HTTP {code} from the statuses API")
        };
        return Err(fail("publish-failed", detail));
    }
    println!("github-status-published context={context} state={state} sha={sha}");
    Ok(())
}

#[derive(Default, Clone)]
struct Seen {
    url: String,
    auth: String,
    body: Value,
}

fn handle_connection(mut stream: TcpStream, seen: Arc<Mutex<Vec<Seen>>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut content_length = 0usize;
    let mut authorization = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("authorization") {
                authorization = value.trim().to_string();
            }
        }
    }
    let mut body = vec![0u8; content_length];
    let _ = reader.read_exact(&mut body);
    let value: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("")
        .to_string();
    seen.lock().unwrap().push(Seen {
        url: path,
        auth: authorization,
        body: value,
    });
    let response =
        b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
    let _ = stream.write_all(response);
}

fn start_server(seen: Arc<Mutex<Vec<Seen>>>) -> Result<(String, u16)> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let seen = seen.clone();
            std::thread::spawn(move || handle_connection(stream, seen));
        }
    });
    Ok((format!("http://127.0.0.1:{port}"), port))
}

fn selftest() -> i32 {
    let mut failures = 0;
    let mut check = |name: &str, ok: bool| {
        if ok {
            println!("selftest ok: {name}");
        } else {
            eprintln!("selftest FAIL: {name}");
            failures += 1;
        }
    };
    let bad_cases: Vec<(&str, String, &str, &str, String)> = vec![
        ("", "a".repeat(40), "success", "ci/x", "d".into()),
        ("acme/widgets", "main".into(), "success", "ci/x", "d".into()),
        ("acme/widgets", "a".repeat(40), "green", "ci/x", "d".into()),
        ("acme/widgets", "a".repeat(40), "success", "", "d".into()),
        (
            "acme/widgets",
            "a".repeat(40),
            "success",
            "ci/x",
            "x".repeat(141),
        ),
    ];
    for bad in bad_cases {
        check(
            &format!("refuses {bad:?}"),
            validate(bad.0, &bad.1, bad.2, bad.3, &bad.4, "").is_err(),
        );
    }
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
    let (api, _port) = match start_server(seen.clone()) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("publish-status selftest: FAIL ({error})");
            return 1;
        }
    };
    let token = "selftest-token";
    check(
        "posts to the exact-sha statuses path",
        publish(
            &api,
            "acme/widgets",
            &"a".repeat(40),
            "success",
            "ci/faktor/trusted-certified-linux",
            &format!("tree={} run=7:linux", "b".repeat(40)),
            "",
            token,
            "",
            "",
            "",
        )
        .is_ok(),
    );
    let snapshot = seen.lock().unwrap().clone();
    let posted = snapshot.last().cloned().unwrap_or_default();
    check(
        "posts to the exact-sha statuses path",
        posted.url == format!("/repos/acme/widgets/statuses/{}", "a".repeat(40)),
    );
    check(
        "posts with bearer auth",
        posted.auth == "Bearer selftest-token",
    );
    check(
        "body binds state/context/description",
        posted.body.get("state").and_then(Value::as_str) == Some("success")
            && posted.body.get("context").and_then(Value::as_str)
                == Some("ci/faktor/trusted-certified-linux")
            && posted.body.get("description").and_then(Value::as_str)
                == Some(format!("tree={} run=7:linux", "b".repeat(40)).as_str()),
    );
    let expected = "5fe2c03b7870f4b45a9f5ae83392cae87eb7a3effe2d76d20551a88034d08aab";
    let actual = platform_signature(
        "darwin",
        &"a".repeat(40),
        &"b".repeat(40),
        "700:darwin",
        "success",
        "ci/faktor/trusted-certified-darwin",
        "darwin-secret",
    );
    check(
        "platform signature matches the pinned canonical vector",
        actual == expected,
    );
    let uppercase = HashMap::from([(
        "FAKTOR_PLATFORM_STATUS_KEY_DARWIN".to_string(),
        "k".to_string(),
    )]);
    check(
        "platform key lookup accepts the uppercase env form",
        platform_key_from_env("darwin", Some(&uppercase)) == "k",
    );
    let both = HashMap::from([
        (
            "faktor_platform_status_key_darwin".to_string(),
            "lower".to_string(),
        ),
        (
            "FAKTOR_PLATFORM_STATUS_KEY_DARWIN".to_string(),
            "upper".to_string(),
        ),
    ]);
    check(
        "platform key lookup prefers the lowercase spelling when both exist",
        platform_key_from_env("darwin", Some(&both)) == "lower",
    );
    check(
        "platform key lookup never leaks ambient os.environ into an explicit mapping",
        platform_key_from_env("darwin", Some(&HashMap::new())).is_empty(),
    );
    let env_with = |extra: &[(&str, &str)]| {
        let mut env: HashMap<String, String> = std::env::vars()
            .filter(|(name, _)| {
                !name
                    .to_uppercase()
                    .starts_with("FAKTOR_PLATFORM_STATUS_KEY_")
            })
            .collect();
        for (name, value) in extra {
            env.insert((*name).to_string(), (*value).to_string());
        }
        env
    };
    let exe = std::env::current_exe().unwrap();
    let child = Command::new(&exe)
        .args([
            "publish-status",
            "publish",
            "--api",
            &api,
            "--repo",
            "acme/widgets",
            "--sha",
            &"a".repeat(40),
            "--state",
            "success",
            "--context",
            "ci/faktor/trusted-certified-darwin",
            "--platform",
            "darwin",
            "--tree",
            &"b".repeat(40),
            "--run-prefix",
            "700",
        ])
        .env_clear()
        .envs(env_with(&[
            ("GITHUB_STATUS_TOKEN", "selftest-token"),
            ("FAKTOR_PLATFORM_STATUS_KEY_DARWIN", "darwin-secret"),
        ]))
        .output()
        .unwrap();
    let before = seen.lock().unwrap().len();
    let snapshot = seen.lock().unwrap().clone();
    let posted = snapshot.last().cloned().unwrap_or_default();
    check(
        "platform publish signs the constructed description",
        child.status.success()
            && posted.body.get("state").and_then(Value::as_str) == Some("success")
            && posted.body.get("context").and_then(Value::as_str)
                == Some("ci/faktor/trusted-certified-darwin")
            && posted.body.get("description").and_then(Value::as_str)
                == Some(format!("tree={} run=700:darwin sig={expected}", "b".repeat(40)).as_str()),
    );
    let refused = Command::new(&exe)
        .args([
            "publish-status",
            "publish",
            "--api",
            &api,
            "--repo",
            "acme/widgets",
            "--sha",
            &"a".repeat(40),
            "--state",
            "success",
            "--context",
            "ci/faktor/trusted-certified-darwin",
            "--platform",
            "darwin",
            "--tree",
            &"b".repeat(40),
            "--run-prefix",
            "700",
        ])
        .env_clear()
        .envs(env_with(&[
            ("GITHUB_STATUS_TOKEN", "selftest-token"),
            ("faktor_platform_status_key_linux", "selftest-linux-key"),
            ("FAKTOR_PLATFORM_STATUS_KEY_WINDOWS", "selftest-windows-key"),
        ]))
        .output()
        .unwrap();
    check(
        "platform publish without its own key refuses (fail closed)",
        !refused.status.success()
            && String::from_utf8_lossy(&refused.stdout)
                .contains("github-status-platform-key-missing")
            || String::from_utf8_lossy(&refused.stderr)
                .contains("github-status-platform-key-missing"),
    );
    check(
        "platform key refusal makes no request",
        seen.lock().unwrap().len() == before,
    );
    if failures > 0 {
        eprintln!("publish-status selftest: FAIL ({failures})");
        return 1;
    }
    println!("publish-status selftest: PASS");
    0
}

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("selftest") => return selftest(),
        Some("publish") => {}
        _ => {
            eprintln!("github-status-args: usage: faktor-ci-tools publish-status publish|selftest");
            return 1;
        }
    }
    let result = (|| -> Result<()> {
        let api = arg(
            args,
            "--api",
            &std::env::var("GITHUB_STATUS_API").unwrap_or_else(|_| API_DEFAULT.into()),
        )?;
        let repo = arg(
            args,
            "--repo",
            &std::env::var("CI_REPO").unwrap_or_default(),
        )?;
        let sha = arg(
            args,
            "--sha",
            &std::env::var("CI_COMMIT_SHA").unwrap_or_default(),
        )?;
        let state = arg(args, "--state", "")?;
        let context = arg(args, "--context", "")?;
        let description = arg(args, "--description", "")?;
        let target_url = arg(
            args,
            "--target-url",
            &std::env::var("CI_PIPELINE_URL").unwrap_or_default(),
        )?;
        let platform = arg(args, "--platform", "")?;
        let tree = arg(args, "--tree", "")?;
        let run_prefix = arg(
            args,
            "--run-prefix",
            &std::env::var("CI_PIPELINE_NUMBER").unwrap_or_default(),
        )?;
        publish(
            &api,
            &repo,
            &sha,
            &state,
            &context,
            &description,
            &target_url,
            &token_from_env(),
            &platform,
            &tree,
            &run_prefix,
        )
    })();
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}
