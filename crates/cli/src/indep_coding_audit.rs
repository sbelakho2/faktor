//! TEMPORARY independent coding-task audit (real daemon graph + real Ollama
//! through a logging proxy). Removed after measurement.

use super::*;

struct AuditAllow;

impl faktor_agent::PermissionRequester for AuditAllow {
    fn request(
        &self,
        _session: SessionId,
        _permission: &faktor_session::PermissionRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = faktor_core::Result<faktor_core::capability::PermissionDecision>,
                > + Send,
        >,
    > {
        Box::pin(async { Ok(faktor_core::capability::PermissionDecision::Allow) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "[live] live Ollama coding-task audit"]
async fn indep_coding_task_audit() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info,faktor_agent=debug,faktor_openai=debug")
        .with_test_writer()
        .try_init();
    let port: u16 = std::env::var("AUDIT_PROXY_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(19434);
    let model = std::env::var("AUDIT_MODEL").unwrap_or_else(|_| "qwen3.8:latest".into());
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"service-rs\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src/lib.rs"),
        "pub mod retry;\n\npub fn add(a: i64, b: i64) -> i64 { a + b }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src/retry.rs"),
        "pub fn backoff_ms(attempt: u32, base_ms: u64) -> u64 {\n    base_ms * (1u64 << attempt.saturating_sub(1))\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn backoff_doubles_each_attempt() {\n        assert_eq!(backoff_ms(0, 100), 100);\n        assert_eq!(backoff_ms(1, 100), 200);\n        assert_eq!(backoff_ms(2, 100), 400);\n    }\n}\n",
    )
    .unwrap();
    let data = tempfile::tempdir().unwrap();
    let cfg = config::Config {
        model: model.clone(),
        // Economy: the only registered local runtime advertises the real
        // installed model (live discovery); the synthetic `default` alias
        // resolves to it in the adapter. Pinning is deliberately avoided
        // here because the boot preflight validates pins before live
        // Ollama discovery has run.
        routing_mode: None,
        providers: vec![config::ProviderCfg::Ollama {
            id: "local".into(),
            base_url: Some(format!("http://127.0.0.1:{port}")),
            allow_loopback: true,
            pricing: None,
            quality: Some(config::ProviderQualityCfg {
                coding_reliability: Some(80),
                context_reliability: Some(80),
                tool_reliability: Some(80),
                reasoning_reliability: Some(80),
            }),
            // Local cold-start bound: a 27B model is loaded lazily on the
            // first request; the generic 60 s header default is premature.
            first_byte_timeout_ms: Some(300_000),
            idle_timeout_ms: Some(120_000),
        }],
        instructions: "You are Faktor, a careful senior engineer. Read each file at most once. \
After identifying the bug, apply the fix with edit_file immediately, then run the tests."
            .into(),
        sandbox: config::SandboxCfg {
            network: Some(vec![format!("http://127.0.0.1:{port}")]),
            network_guarantee: faktor_sandbox::SandboxGuarantee::BestEffort,
            shell: Some(faktor_sandbox::ShellExecutionMode::NetworkCapableUserGranted),
            ..config::SandboxCfg::default()
        },
        ..config::Config::default()
    };
    let graph = build_daemon_with_permission(data.path(), Some(cfg), Arc::new(AuditAllow))
        .expect("daemon graph");
    // Warm the model OUTSIDE the measured window. A cold Ollama loads the
    // model on the first request; doing that inside the task makes a correct
    // turn look like a provider timeout.
    let warm = faktor_ollama::OllamaProvider::new(
        faktor_ollama::OllamaConfig::new(Some(format!("http://127.0.0.1:{port}")))
            .with_timeouts(Some(300_000), Some(120_000)),
        Arc::new(faktor_provider::egress::PolicyCheckedHttpTransport::permissive()),
    );
    let warm_started = std::time::Instant::now();
    warm.warm_up(&model).await.expect("Ollama warm-up");
    let warmup_ms = warm_started.elapsed().as_millis() as u64;
    let ws = graph
        .session
        .create_workspace(tmp.path().to_str().unwrap())
        .unwrap();
    let session = graph
        .session
        .create_session(ws, "indep coding audit", "local", &model)
        .unwrap();
    let prompt = "The retry backoff test in src/retry.rs fails: backoff_ms returns the wrong delay after the first attempt. Fix src/retry.rs, then run cargo test to verify.";
    let started = std::time::Instant::now();
    let outcome = graph
        .agent
        .run_turn(session.id(), prompt.trim(), &[])
        .await
        .expect("turn");
    let elapsed = started.elapsed();
    let usage = graph
        .session
        .store()
        .session_usage_tokens(session.id())
        .unwrap();
    let tool_rows = session.completed_tool_runs(1000).unwrap();
    let events = session.events_range(1, None).unwrap();
    let tool_calls = events
        .iter()
        .filter(|event| format!("{:?}", event.kind).contains("ToolCall"))
        .count();
    let tool_results = events
        .iter()
        .filter(|event| format!("{:?}", event.kind).contains("ToolResult"))
        .count();
    let source = std::fs::read_to_string(tmp.path().join("src/retry.rs")).unwrap();
    let lib = std::fs::read_to_string(tmp.path().join("src/lib.rs")).unwrap();
    let verify = std::process::Command::new("cargo")
        .arg("test")
        .arg("--quiet")
        .current_dir(tmp.path())
        .env("CARGO_TARGET_DIR", tmp.path().join("target"))
        .output()
        .unwrap();
    let verify_stdout = String::from_utf8_lossy(&verify.stdout).into_owned();
    let verify_stderr = String::from_utf8_lossy(&verify.stderr).into_owned();
    let test_module = source
        .split("#[cfg(test)]")
        .nth(1)
        .unwrap_or_default()
        .to_string();
    let tests_intact = test_module.contains("assert_eq!(backoff_ms(0, 100), 100)")
        && test_module.contains("assert_eq!(backoff_ms(1, 100), 200)")
        && test_module.contains("assert_eq!(backoff_ms(2, 100), 400)");
    eprintln!(
        "{}",
        serde_json::json!({
            "model": model,
            "warmup_ms": warmup_ms,
            "elapsed_ms": elapsed.as_millis() as u64,
            "final_state": format!("{:?}", outcome.final_state),
            "stop_reason": format!("{:?}", outcome.stop_reason),
            "attempts": outcome.turns,
            "compacted": outcome.compacted,
            "verification": format!("{:?}", outcome.verification),
            "acceptance": format!("{:?}", outcome.acceptance),
            "completion": format!("{:?}", outcome.completion),
            "usage_tokens": usage,
            "tool_rows": tool_rows.iter().map(|r| format!("{}:{:?}", r.tool, r.status)).collect::<Vec<_>>(),
            "event_tool_calls": tool_calls,
            "event_tool_results": tool_results,
            "verify_status": verify.status.code(),
            "verify_stdout": verify_stdout,
            "verify_stderr": verify_stderr,
            "source": source,
            "lib_unchanged": lib == "pub mod retry;\n\npub fn add(a: i64, b: i64) -> i64 { a + b }\n",
            "tests_intact": tests_intact,
            "workspace": tmp.path().to_string_lossy(),
        })
    );
}
