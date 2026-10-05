use crate::api::tests::*;
use crate::api::*;

/// Residual: `ServerDeps::new` refuses TYPED on a degenerate empty store
/// root. `Store::root()` is the directory the store was opened at, so an
/// empty root derives the bare relative `shadows` shadow root; the
/// constructor must propagate [`ShadowRootError`] instead of letting a
/// later shadow write under the process working directory. There is NO
/// silent fallback.
#[test]
fn server_deps_new_refuses_a_degenerate_empty_store_root_typed() {
    let dir = tempfile::tempdir().unwrap();
    let cleanup = CwdStoreFileGuard;
    let session = SessionManager::open_quick(std::path::PathBuf::new(), dir.path().join("cas"))
        .expect("the raw store opens at the degenerate root; the refusal is ServerDeps'");
    assert!(
        session.store().root().as_os_str().is_empty(),
        "fixture must carry the empty data root"
    );
    let permissions = ChannelPermissionRequester::new(Duration::from_secs(5));
    let agent = test_agent_over(
        session.clone(),
        permissions.clone(),
        faktor_provider::ProviderRegistry::new(),
        None,
    );
    match ServerDeps::new(session, agent, permissions) {
        Err(ServerDepsError::DegenerateShadowRoot(
            faktor_orchestrator::runtime::shadow::ShadowRootError::DegenerateRoot { root, reason },
        )) => {
            assert_eq!(
                root,
                std::path::PathBuf::from("shadows"),
                "the refused root is the derived relative `shadows`"
            );
            assert!(reason.contains("unanchored"), "{reason}");
        }
        Ok(_) => panic!("a degenerate store root must refuse typed, never fall back"),
        Err(other) => {
            panic!("a degenerate store root must refuse typed, never fall back; got {other:?}")
        }
    }
    drop(cleanup);
}

// ------------------------------------------------------------------
// P0 round: the added operations (status aliases, fork,
// summarize, delete, deleteMessage, question/network over the permission
// machinery, config update/warnings/overlay, pty rejection, dispose,
// auth rotation) each do real work and refuse loudly where the runtime
// cannot honor them.

/// Drop the ids that differ by construction between a session and its
/// fork (info.sessionID and the row createdMs) for equality checks.

// ---------------------------------------------------------------- native v1

#[test]
fn server_reaches_task_start_only_through_the_executor() {
    // The single-authority source scan: in the NON-TEST server code the
    // ONLY TaskExecutor start edge is the native start handler, the
    // ONLY TaskRunRequest construction lives in that same handler. The
    // scan spans every server source file of the audit 81-83/94 split
    // (api router assembly + native/*).
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    // The api surface was decomposed into cohesive modules (audit 9/23/24);
    // the scan spans the production files only (tests live out-of-line).
    let mut src = String::new();
    for module in [
        "api.rs",
        "api/deps.rs",
        "api/lifecycle.rs",
        "api/router.rs",
        "api/health.rs",
    ] {
        src.push_str(&std::fs::read_to_string(root.join(module)).expect("api module source"));
        src.push('\n');
    }
    for module in [
        "native/mod.rs",
        "native/session.rs",
        "native/task.rs",
        "native/agents.rs",
        "native/evidence.rs",
        "native/verification.rs",
        "native/terminal.rs",
        "native/usage.rs",
        "native/semantic.rs",
        "native/models.rs",
    ] {
        src.push_str(&std::fs::read_to_string(root.join(module)).expect("server module source"));
        src.push('\n');
    }
    let lines: Vec<&str> = src.lines().collect();
    let is_decl_start = |l: &str| {
        l.starts_with("async fn ")
            || l.starts_with("fn ")
            || l.starts_with("pub(crate) async fn ")
            || l.starts_with("pub(crate) fn ")
    };
    let in_handler = |i: usize| {
        let Some(start) = lines
            .iter()
            .position(|l| l.contains("async fn native_task_run_start("))
        else {
            return false;
        };
        let end = lines
            .iter()
            .enumerate()
            .skip(start + 1)
            .find(|(_, l)| is_decl_start(l))
            .map(|(j, _)| j)
            .expect("a declaration follows the start handler");
        i > start && i < end
    };
    // Exactly one `.start_task(` call, inside the native start handler.
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.contains(".start_task("))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(starts.len(), 1, "one task-start call site: {starts:?}");
    assert!(
        in_handler(starts[0]),
        "the start call must live in native_task_run_start, at line {}",
        starts[0]
    );
    assert!(
        lines[starts[0]].contains("prompts.start_task"),
        "the native handler reaches the executor ONLY through the \
             PromptExecutionService: {}",
        lines[starts[0]]
    );
    // Exactly one TaskRunRequest construction, in the same handler.
    let requests: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.contains("task_executor::TaskRunRequest {"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(requests.len(), 1, "one TaskRunRequest site: {requests:?}");
    assert!(in_handler(requests[0]), "line {}", requests[0]);
    // (work-entry unification) NO non-test server code drives the agent
    // directly: every ordinary prompt and every explicit task start goes
    // through the PromptExecutionService.
    let drives: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            l.contains(".run_session_queue(")
                || l.contains(".drive_receipt(")
                || l.contains("agent.submit(")
        })
        .map(|(i, _)| i)
        .collect();
    assert!(
        drives.is_empty(),
        "no direct AgentRuntime drive may remain in server production code: {drives:?}"
    );
    // ... and the ONE start edge is the prompt service, which itself
    // wraps the executor (never the agent).
    let service_src =
        std::fs::read_to_string(root.join("native/prompt.rs")).expect("native/prompt.rs source");
    assert!(
        service_src.contains("self.tasks.start_task("),
        "PromptExecutionService must start runs through the TaskExecutor"
    );
    for (i, l) in service_src.lines().enumerate() {
        assert!(
            !l.contains(".run_session_queue(")
                && !l.contains(".drive_receipt(")
                && !l.contains("agent.submit("),
            "prompt service line {} drives the agent directly: {l}",
            i + 1
        );
    }
}

// ------------------------------------------------ native evidence (audit 82)
