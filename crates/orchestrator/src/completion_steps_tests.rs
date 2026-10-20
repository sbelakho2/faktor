//! Adversarial covers of the completion-step runner: no-contract parity,
//! truthful commit outcomes with a bounded message, Skipped/retryable
//! gate interplay, egress-denied and remote-less pushes, unconfigured /
//! configured PR commands (URL parse + already-exists idempotency),
//! ordering + terminal stop, and reopen idempotency across a simulated
//! crash between steps.
use super::*;
use faktor_core::completion::CompletionContract;
use faktor_core::id::VerificationRecordId;
use faktor_core::state::{TaskState, TaskTransition, VerificationStatus};
use faktor_session::{CompletionContractGate, SessionManager, Task, TaskError};
use std::path::Path;

fn allow_all() -> Arc<dyn EgressPolicy> {
    Arc::new(|_url: &str| Ok(()))
}

fn deny_all() -> Arc<dyn EgressPolicy> {
    Arc::new(|url: &str| Err(format!("network denied by test policy for {url}")))
}

fn manager() -> (tempfile::TempDir, Arc<SessionManager>) {
    let dir = tempfile::tempdir().unwrap();
    let m = SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    (dir, m)
}

fn session_with_task(
    m: &Arc<SessionManager>,
    goal: &str,
    contract: Option<CompletionContract>,
) -> (SessionHandle, TaskId) {
    let ws = m.create_workspace("/w").unwrap();
    let h = m
        .create_session(ws, "completion-steps", "fake", "fake")
        .unwrap();
    let task_id = h.task_id().unwrap();
    let now = h.now_ms();
    h.create_task(Task {
        task_id,
        session_id: h.id(),
        goal: goal.into(),
        created_ms: now,
        updated_ms: now,
        ..Default::default()
    })
    .unwrap();
    if let Some(contract) = contract {
        let rev = h.task_revision(task_id).unwrap();
        h.set_completion_contract(task_id, rev, contract).unwrap();
    }
    (h, task_id)
}

fn runner(
    dir: &Path,
    config: CompletionStepsConfig,
    egress: Arc<dyn EgressPolicy>,
) -> CompletionStepRunner {
    let cas = Arc::new(faktor_cas::Cas::open(dir.join("exec-cas")).unwrap());
    let supervisor = ProcessSupervisor::new(cas);
    CompletionStepRunner::new(supervisor, egress, config).unwrap()
}

fn git(cwd: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        cwd.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_output(cwd: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo(root: &Path) -> std::path::PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@faktor.local"]);
    git(&repo, &["config", "user.name", "Faktor Test"]);
    std::fs::write(repo.join("README.md"), "base\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

fn add_bare_remote(root: &Path, repo: &Path) -> std::path::PathBuf {
    let bare = root.join("remote.git");
    git(root, &["init", "--bare", "-q", bare.to_str().unwrap()]);
    git(repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
    bare
}

fn contract(commit: bool, push: bool, pr: bool) -> CompletionContract {
    CompletionContract {
        include_commit: commit,
        include_push: push,
        include_pr: pr,
    }
}

fn ctx(root: &Path, goal: &str) -> CompletionStepContext {
    CompletionStepContext {
        root: root.to_path_buf(),
        goal: goal.to_string(),
    }
}

fn step_rows(h: &SessionHandle, task_id: TaskId) -> Vec<faktor_session::CompletionStepStatusRow> {
    // Statuses are keyed by the CONTRACT revision, not the task row's
    // current revision (machine transitions bump the latter).
    let (revision, _) = h.completion_contract(task_id).unwrap().expect("contract");
    h.ledger_completion_step_statuses(task_id.raw(), revision.raw())
        .unwrap()
}

fn drive_to_verifying(h: &SessionHandle, task_id: TaskId) {
    for transition in [
        TaskTransition::StartRunning,
        TaskTransition::RequestVerification,
        TaskTransition::StartVerification,
    ] {
        let rev = h.task_revision(task_id).unwrap();
        h.transition_task(task_id, rev, transition, None).unwrap();
    }
}

fn passing_record(h: &SessionHandle, task_id: TaskId) -> VerificationRecordId {
    h.create_verification_record(
        task_id,
        None,
        vec![],
        vec![],
        vec![],
        vec![],
        None,
        VerificationStatus::Passed,
        h.now_ms(),
    )
    .unwrap()
}

#[tokio::test]
async fn no_contract_is_a_pure_no_op_parity_path() {
    let (dir, m) = manager();
    let (h, task_id) = session_with_task(&m, "no contract", None);
    let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
    // The root does not even exist: if the runner touched git the test
    // would fail — parity means the runner is never invoked.
    let report = runner
        .run(&h, task_id, &ctx(&dir.path().join("nope"), "no contract"))
        .await
        .unwrap();
    assert!(report.records.is_empty());
    assert!(report.pr_url.is_none());
    assert!(h
        .ledger_completion_contract(task_id.raw())
        .unwrap()
        .is_none());
    let rev = h.task_revision(task_id).unwrap();
    assert!(h
        .ledger_completion_step_statuses(task_id.raw(), rev.raw())
        .unwrap()
        .is_empty());
    assert_eq!(
        h.completion_contract_gate(task_id).unwrap(),
        CompletionContractGate::Satisfied
    );
}

#[tokio::test]
async fn commit_step_commits_real_changes_with_a_bounded_goal_message() {
    let (dir, m) = manager();
    let repo = init_repo(dir.path());
    std::fs::write(repo.join("feature.txt"), "new content\n").unwrap();
    let long_goal = format!("implement the feature {}", "x".repeat(500));
    let (h, task_id) = session_with_task(&m, &long_goal, Some(contract(true, false, false)));
    let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
    let report = runner
        .run(&h, task_id, &ctx(&repo, &long_goal))
        .await
        .unwrap();
    assert!(report.all_succeeded(), "{report:?}");
    assert!(dir_clean(&repo));
    let message = git_output(&repo, &["log", "-1", "--pretty=%B"]);
    assert!(
        message.starts_with("faktor: implement the feature"),
        "{message}"
    );
    assert!(
        message.len() <= MAX_COMMIT_MESSAGE_BYTES,
        "{}",
        message.len()
    );
    let rows = step_rows(&h, task_id);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].step, CompletionStep::Commit);
    assert_eq!(rows[0].status, CompletionStepOutcome::Succeeded);
    // Idempotent replay: the status row is not duplicated.
    let replay = runner
        .run(&h, task_id, &ctx(&repo, &long_goal))
        .await
        .unwrap();
    assert_eq!(
        replay.outcome_of(CompletionStep::Commit),
        Some(CompletionStepOutcome::Succeeded)
    );
    assert_eq!(
        step_rows(&h, task_id).len(),
        1,
        "no duplicate row on replay"
    );
    assert!(dir_clean(&repo));
}

fn dir_clean(repo: &Path) -> bool {
    git_output(repo, &["status", "--porcelain"]).is_empty()
}

#[tokio::test]
async fn nothing_to_commit_is_skipped_and_the_gate_treats_it_as_unmet() {
    let (dir, m) = manager();
    let repo = init_repo(dir.path());
    let (h, task_id) = session_with_task(&m, "clean tree", Some(contract(true, false, false)));
    let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
    let report = runner
        .run(&h, task_id, &ctx(&repo, "clean tree"))
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Commit),
        Some(CompletionStepOutcome::Skipped),
        "{report:?}"
    );
    let rows = step_rows(&h, task_id);
    assert_eq!(rows[0].status, CompletionStepOutcome::Skipped);
    // The documented gate rule: Skipped is NOT succeeded, so a passing
    // proof still refuses — retryably — and the task stays Verifying.
    drive_to_verifying(&h, task_id);
    let record = passing_record(&h, task_id);
    let rev = h.task_revision(task_id).unwrap();
    let err = h.complete_verified_task(task_id, rev, record).unwrap_err();
    match err {
        TaskError::CompletionStepNotSucceeded { status, step, .. } => {
            assert_eq!(step, CompletionStep::Commit);
            assert_eq!(status, CompletionStepOutcome::Skipped);
        }
        other => panic!("expected a retryable Skipped refusal, got {other}"),
    }
    assert_eq!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::Verifying
    );
    // A later real change + re-run flips the SAME contract revision to
    // Succeeded (Skipped stays retryable) and the gate then certifies.
    std::fs::write(repo.join("later.txt"), "later\n").unwrap();
    let report = runner
        .run(&h, task_id, &ctx(&repo, "clean tree"))
        .await
        .unwrap();
    assert!(report.all_succeeded(), "{report:?}");
    let rev = h.task_revision(task_id).unwrap();
    let record = passing_record(&h, task_id);
    let done = h.complete_verified_task(task_id, rev, record).unwrap();
    assert_eq!(done.state, TaskState::VerifiedComplete);
}

#[tokio::test]
async fn push_to_a_local_bare_remote_succeeds_and_bypasses_egress() {
    let (dir, m) = manager();
    let repo = init_repo(dir.path());
    let bare = add_bare_remote(dir.path(), &repo);
    let (h, task_id) = session_with_task(&m, "push it", Some(contract(false, true, false)));
    // A DENY-ALL policy: local path remotes are not egress, so the push
    // still succeeds (the policy is never consulted).
    let runner = runner(dir.path(), CompletionStepsConfig::default(), deny_all());
    let report = runner
        .run(&h, task_id, &ctx(&repo, "push it"))
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Push),
        Some(CompletionStepOutcome::Succeeded),
        "{report:?}"
    );
    let head = git_output(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(
        git_output(&bare, &["rev-parse", "main"]),
        head,
        "the bare remote must hold the pushed commit"
    );
    let rows = step_rows(&h, task_id);
    assert_eq!(rows[0].status, CompletionStepOutcome::Succeeded);
    // Replay: latest row is Succeeded, so no second row and no push.
    let replay = runner
        .run(&h, task_id, &ctx(&repo, "push it"))
        .await
        .unwrap();
    assert!(replay.all_succeeded());
    assert_eq!(step_rows(&h, task_id).len(), 1);
}

#[tokio::test]
async fn push_denied_by_egress_policy_is_a_typed_failure() {
    let (dir, m) = manager();
    let repo = init_repo(dir.path());
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://git.example.invalid/team/repo.git",
        ],
    );
    let (h, task_id) = session_with_task(&m, "push denied", Some(contract(false, true, false)));
    let runner = runner(dir.path(), CompletionStepsConfig::default(), deny_all());
    let report = runner
        .run(&h, task_id, &ctx(&repo, "push denied"))
        .await
        .unwrap();
    let failed = report.failed_step();
    assert_eq!(failed, Some(CompletionStep::Push), "{report:?}");
    let rows = step_rows(&h, task_id);
    assert_eq!(rows[0].status, CompletionStepOutcome::Failed);
    assert!(
        rows[0].detail.contains("egress policy denied"),
        "{}",
        rows[0].detail
    );
    // The gate refuses TERMINALLY: even a passing proof cannot certify.
    drive_to_verifying(&h, task_id);
    let record = passing_record(&h, task_id);
    let rev = h.task_revision(task_id).unwrap();
    let err = h.complete_verified_task(task_id, rev, record).unwrap_err();
    assert!(
        matches!(err, TaskError::CompletionStepFailed { .. }),
        "{err}"
    );
    // A re-run never retries a terminally failed step and writes nothing.
    let replay = runner
        .run(&h, task_id, &ctx(&repo, "push denied"))
        .await
        .unwrap();
    assert_eq!(replay.failed_step(), Some(CompletionStep::Push));
    assert_eq!(step_rows(&h, task_id).len(), 1, "terminal rows are frozen");
}

#[tokio::test]
async fn push_without_a_remote_is_skipped() {
    let (dir, m) = manager();
    let repo = init_repo(dir.path());
    let (h, task_id) = session_with_task(&m, "no remote", Some(contract(false, true, false)));
    let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
    let report = runner
        .run(&h, task_id, &ctx(&repo, "no remote"))
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Push),
        Some(CompletionStepOutcome::Skipped),
        "{report:?}"
    );
    let rows = step_rows(&h, task_id);
    assert!(rows[0].detail.contains("no git remote configured"));
}

#[tokio::test]
async fn detached_head_push_is_failed_typed() {
    let (dir, m) = manager();
    let repo = init_repo(dir.path());
    add_bare_remote(dir.path(), &repo);
    git(&repo, &["checkout", "-q", "--detach"]);
    let (h, task_id) = session_with_task(&m, "detached", Some(contract(false, true, false)));
    let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
    let report = runner
        .run(&h, task_id, &ctx(&repo, "detached"))
        .await
        .unwrap();
    assert_eq!(
        report.failed_step(),
        Some(CompletionStep::Push),
        "{report:?}"
    );
    assert!(step_rows(&h, task_id)[0].detail.contains("detached HEAD"));
}

#[tokio::test]
async fn unconfigured_pr_command_is_skipped() {
    let (dir, m) = manager();
    let (h, task_id) = session_with_task(&m, "no pr command", Some(contract(false, false, true)));
    let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
    let report = runner
        .run(&h, task_id, &ctx(&dir.path().join("nope"), "no pr command"))
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Pr),
        Some(CompletionStepOutcome::Skipped),
        "{report:?}"
    );
    assert!(step_rows(&h, task_id)[0]
        .detail
        .contains("pr_command is not configured"));
}

#[cfg(unix)]
fn fake_pr_script(dir: &Path, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("fake-pr.sh");
    std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    script
}

#[cfg(unix)]
#[tokio::test]
async fn configured_pr_command_succeeds_and_parses_the_url() {
    let (dir, m) = manager();
    let repo = init_repo(dir.path());
    let script = fake_pr_script(dir.path(), "echo \"https://example.test/pr/42\"");
    let config = CompletionStepsConfig {
        pr_command: Some(format!("{} {{branch}} {{base}}", script.display())),
        ..Default::default()
    };
    let (h, task_id) = session_with_task(&m, "open a pr", Some(contract(false, false, true)));
    let runner = runner(dir.path(), config, allow_all());
    let report = runner
        .run(&h, task_id, &ctx(&repo, "open a pr"))
        .await
        .unwrap();
    assert!(report.all_succeeded(), "{report:?}");
    assert_eq!(report.pr_url.as_deref(), Some("https://example.test/pr/42"));
    assert!(step_rows(&h, task_id)[0]
        .detail
        .contains("https://example.test/pr/42"));
}

#[cfg(unix)]
#[tokio::test]
async fn typed_argv_pr_program_executes_with_spaces_preserved() {
    let (dir, m) = manager();
    let repo = init_repo(dir.path());
    // A program path WITH SPACES and an argument WITH SPACES: the typed
    // argv is executed directly (no shell, no whitespace splitting), so
    // each element must arrive verbatim. The spy writes its argv to
    // `args.txt` in the step's cwd (the repo root).
    use std::os::unix::fs::PermissionsExt;
    let tools = dir.path().join("my tools");
    std::fs::create_dir_all(&tools).unwrap();
    let script = tools.join("fake pr.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > argv-spy.txt\necho \"https://example.test/pr/11\"\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let config = CompletionStepsConfig {
        pr_program: Some(script.display().to_string()),
        pr_args: vec![
            "pr".into(),
            "create".into(),
            "--head".into(),
            "{branch}".into(),
            "--title".into(),
            "spaces stay one arg".into(),
            "--remote".into(),
            "{remote}".into(),
        ],
        base_branch: "main".into(),
        ..Default::default()
    };
    let (h, task_id) = session_with_task(&m, "typed argv pr", Some(contract(false, false, true)));
    let runner = runner(dir.path(), config, allow_all());
    let report = runner
        .run(&h, task_id, &ctx(&repo, "typed argv pr"))
        .await
        .unwrap();
    assert!(report.all_succeeded(), "{report:?}");
    assert_eq!(report.pr_url.as_deref(), Some("https://example.test/pr/11"));
    let observed = std::fs::read_to_string(repo.join("argv-spy.txt")).unwrap();
    assert_eq!(
        observed, "pr\ncreate\n--head\nmain\n--title\nspaces stay one arg\n--remote\norigin\n",
        "every argv element must arrive verbatim, spaces included"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn existing_pr_is_an_idempotent_success_with_the_url() {
    let (dir, m) = manager();
    let repo = init_repo(dir.path());
    let script = fake_pr_script(
        dir.path(),
        "echo \"PR already exists: https://example.test/pr/7\"\nexit 1",
    );
    let config = CompletionStepsConfig {
        pr_command: Some(format!("{} {{branch}}", script.display())),
        ..Default::default()
    };
    let (h, task_id) = session_with_task(&m, "existing pr", Some(contract(false, false, true)));
    let runner = runner(dir.path(), config, allow_all());
    let report = runner
        .run(&h, task_id, &ctx(&repo, "existing pr"))
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Pr),
        Some(CompletionStepOutcome::Succeeded),
        "{report:?}"
    );
    assert_eq!(report.pr_url.as_deref(), Some("https://example.test/pr/7"));
}

#[cfg(unix)]
#[tokio::test]
async fn failed_commit_stops_push_and_pr_and_records_them_skipped() {
    let (dir, m) = manager();
    // A repo whose pre-commit hook REJECTS: the commit fails
    // deterministically on every machine (never reliant on a missing
    // global git identity).
    let repo = init_repo(dir.path());
    std::fs::write(repo.join("b.txt"), "will-not-commit").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let hook = repo.join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        let mut perms = std::fs::metadata(&hook).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook, perms).unwrap();
    }
    let bare = add_bare_remote(dir.path(), &repo);
    let marker = dir.path().join("pr-ran.marker");
    let script = fake_pr_script(
        dir.path(),
        &format!(
            "echo ran > {}\necho https://example.test/pr/1",
            marker.display()
        ),
    );
    let config = CompletionStepsConfig {
        pr_command: Some(format!("{} {{branch}}", script.display())),
        ..Default::default()
    };
    let (h, task_id) = session_with_task(&m, "ordering", Some(contract(true, true, true)));
    let runner = runner(dir.path(), config, allow_all());
    let report = runner
        .run(&h, task_id, &ctx(&repo, "ordering"))
        .await
        .unwrap();
    // Gate order: commit failed; push and pr are recorded Skipped
    // ("not attempted") and never executed.
    assert_eq!(
        report.failed_step(),
        Some(CompletionStep::Commit),
        "{report:?}"
    );
    assert_eq!(
        report
            .records
            .iter()
            .map(|r| (r.step, r.status))
            .collect::<Vec<_>>(),
        vec![
            (CompletionStep::Commit, CompletionStepOutcome::Failed),
            (CompletionStep::Push, CompletionStepOutcome::Skipped),
            (CompletionStep::Pr, CompletionStepOutcome::Skipped),
        ]
    );
    assert!(
        !marker.exists(),
        "the PR command must not run after a failed commit"
    );
    assert!(
        std::process::Command::new("git")
            .args(["rev-parse", "--verify", "main"])
            .current_dir(&bare)
            .output()
            .unwrap()
            .status
            .code()
            != Some(0),
        "the push must not run after a failed commit"
    );
    assert!(step_rows(&h, task_id).iter().all(|r| !r.detail.is_empty()));
}

#[cfg(unix)]
#[tokio::test]
async fn crash_between_steps_reopens_and_completes_idempotently() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let cas = dir.path().join("cas");
    let repo = init_repo(dir.path());
    std::fs::write(repo.join("work.txt"), "committed before the crash\n").unwrap();
    let contract = contract(true, true, false);
    let task_id;
    {
        // Phase 1: commit succeeds, push is skipped (no remote yet) —
        // the "crash" happens between the two steps.
        let m = SessionManager::open(&store, &cas, true).unwrap();
        let (h, tid) = session_with_task(&m, "crash between steps", Some(contract));
        task_id = tid;
        let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
        let report = runner
            .run(&h, tid, &ctx(&repo, "crash between steps"))
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Commit),
            Some(CompletionStepOutcome::Succeeded)
        );
        assert_eq!(
            report.outcome_of(CompletionStep::Push),
            Some(CompletionStepOutcome::Skipped)
        );
    }
    // The remote comes up while the "daemon is down".
    add_bare_remote(dir.path(), &repo);
    // Phase 2: reopen the SAME store and re-run: the commit is NOT
    // re-executed (its durable row is already Succeeded) and the push
    // completes; the gate is then satisfied.
    let m = SessionManager::open(&store, &cas, true).unwrap();
    let h = m
        .get_session(faktor_core::id::SessionId::new(1))
        .unwrap()
        .unwrap();
    {
        let rev = h.task_revision(task_id).unwrap();
        let rows = h
            .ledger_completion_step_statuses(task_id.raw(), rev.raw())
            .unwrap();
        assert_eq!(rows.len(), 2, "durable rows survived the reopen: {rows:?}");
    }
    let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
    let report = runner
        .run(&h, task_id, &ctx(&repo, "crash between steps"))
        .await
        .unwrap();
    assert!(report.all_succeeded(), "{report:?}");
    assert_eq!(
        report.outcome_of(CompletionStep::Commit),
        Some(CompletionStepOutcome::Succeeded)
    );
    let rev = h.task_revision(task_id).unwrap();
    let rows = h
        .ledger_completion_step_statuses(task_id.raw(), rev.raw())
        .unwrap();
    assert_eq!(
        rows.iter()
            .filter(|r| r.step == CompletionStep::Commit)
            .count(),
        1,
        "the committed step is never re-recorded on replay: {rows:?}"
    );
    assert_eq!(
        rows.iter()
            .filter(|r| r.step == CompletionStep::Push)
            .count(),
        2,
        "the retried Skipped push appends the Succeeded row: {rows:?}"
    );
    assert_eq!(
        h.completion_contract_gate(task_id).unwrap(),
        CompletionContractGate::Satisfied
    );
    assert!(dir_clean(&repo));
    let head = git_output(&repo, &["rev-parse", "HEAD"]);
    let bare = dir.path().join("remote.git");
    assert_eq!(git_output(&bare, &["rev-parse", "main"]), head);
}

#[test]
fn strict_config_validation_and_template_rendering() {
    assert!(CompletionStepsConfig::default().validate().is_ok());
    let bad = |command: &str| {
        CompletionStepsConfig {
            pr_command: Some(command.into()),
            ..Default::default()
        }
        .validate()
        .is_err()
    };
    assert!(bad(""));
    assert!(bad("gh pr create --head {branch}; rm -rf /"));
    assert!(bad("gh pr create --head {branch} && true"));
    assert!(bad("gh pr create --head {branch} $(whoami)"));
    assert!(bad("gh pr create --head {nope}"));
    assert!(bad("gh pr create --head main"));
    assert!(bad("gh pr create\n--head {branch}"));
    assert!(bad(&format!(
        "gh pr create --head {{branch}} {}",
        "x".repeat(MAX_PR_COMMAND_BYTES)
    )));
    assert!(CompletionStepsConfig {
        remote: "a b".into(),
        ..Default::default()
    }
    .validate()
    .is_err());
    assert!(CompletionStepsConfig {
        base_branch: "a..b".into(),
        ..Default::default()
    }
    .validate()
    .is_err());
    let (program, args) = render_pr_command(
        "gh pr create --head {branch} --base {base}",
        "feat/x",
        "main",
        "origin",
    )
    .unwrap();
    assert_eq!(program, "gh");
    assert_eq!(
        args,
        vec!["pr", "create", "--head", "feat/x", "--base", "main"]
    );
    assert!(!is_egress_destination("/tmp/remote.git"));
    assert!(!is_egress_destination("file:///tmp/remote.git"));
    assert!(!is_egress_destination(r"C:\repos\remote.git"));
    assert!(is_egress_destination("https://github.com/o/r.git"));
    assert!(is_egress_destination("git@github.com:o/r.git"));
    assert_eq!(
        parse_pr_url("see https://example.test/pr/1)."),
        Some("https://example.test/pr/1".to_string())
    );
    assert!(parse_pr_url("no url here").is_none());
}

/// P3 typed argv: per-element placeholder substitution (spaces are legal
/// argv characters), strict refusals, and legacy-template parity with
/// the exact historic security checks.
#[test]
fn typed_argv_pr_config_substitutes_per_element_and_keeps_legacy_parity() {
    let program = "/opt/My Tools/gh";
    let args: Vec<String> = [
        "pr",
        "create",
        "--head",
        "{branch}",
        "--base",
        "{base}",
        "--title",
        "my PR title",
        "{remote}",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    let config = CompletionStepsConfig {
        pr_program: Some(program.into()),
        pr_args: args.clone(),
        ..Default::default()
    };
    config.validate().expect("spaces are legal argv characters");
    let (rendered_program, rendered_args) =
        render_pr_argv(program, &args, "feat/x", "main", "origin").unwrap();
    assert_eq!(rendered_program, program, "the program path stays exact");
    assert_eq!(
        rendered_args,
        vec![
            "pr",
            "create",
            "--head",
            "feat/x",
            "--base",
            "main",
            "--title",
            "my PR title",
            "origin"
        ],
        "every element substitutes independently; spaces are preserved"
    );

    // Strict refusals: empty program, unknown/unclosed/stray placeholder,
    // missing {branch}, control chars, over-bound args, args without a
    // program, and both shapes configured at once.
    let bad = |program: Option<&str>, args: Vec<&str>| {
        CompletionStepsConfig {
            pr_program: program.map(str::to_string),
            pr_args: args.into_iter().map(str::to_string).collect(),
            ..Default::default()
        }
        .validate()
        .is_err()
    };
    assert!(bad(Some(""), vec!["{branch}"]));
    assert!(bad(Some("/bin/gh"), vec!["--head", "{nope}"]));
    assert!(bad(Some("/bin/gh"), vec!["--head", "{branch"]));
    assert!(bad(Some("/bin/gh"), vec!["--head", "{branch}}"]));
    assert!(bad(Some("/bin/gh"), vec!["--head", "main"]));
    assert!(bad(Some("/bin/gh"), vec!["--head", "{branch}\u{7}"]));
    let too_many: Vec<&str> = std::iter::repeat_n("{branch}", MAX_PR_ARGS + 1).collect();
    assert!(bad(Some("/bin/gh"), too_many));
    assert!(bad(None, vec!["{branch}"]));
    assert!(CompletionStepsConfig {
        pr_command: Some("gh pr create --head {branch}".into()),
        pr_program: Some("/bin/gh".into()),
        ..Default::default()
    }
    .validate()
    .is_err());

    // Legacy parity: the historic render is byte-identical and every
    // historic refusal still fires.
    let (legacy_program, legacy_args) = render_pr_command(
        "gh pr create --head {branch} --base {base}",
        "feat/x",
        "main",
        "origin",
    )
    .unwrap();
    assert_eq!(legacy_program, "gh");
    assert_eq!(
        legacy_args,
        vec!["pr", "create", "--head", "feat/x", "--base", "main"]
    );
    for invalid in [
        "",
        "gh pr create --head {branch}; rm -rf /",
        "gh pr create --head {branch} && true",
        "gh pr create --head {branch} $(whoami)",
        "gh pr create --head {nope}",
        "gh pr create --head main",
        "gh pr create\n--head {branch}",
    ] {
        assert!(
            validate_pr_command(invalid).is_err(),
            "{invalid:?} must stay refused"
        );
    }
    assert!(
        validate_pr_argv("gh pr create --head {branch}; rm -rf /", &[]).is_ok(),
        "typed argv never splits or shells: spaces/metacharacters are plain characters"
    );
}

// ------------------- verified-root race injection (P0 completion gates)

fn repo_tree(repo: &Path) -> String {
    faktor_fs::tree_manifest::tree_manifest_digest(
        repo,
        faktor_fs::tree_manifest::MAX_TREE_MANIFEST_ENTRIES,
    )
    .unwrap()
}

/// A session whose workspace root IS `root`, with a durable integration
/// record whose final snapshot equals `root`'s current digest — exactly
/// the world an orchestrated verified run lives in.
fn session_bound_to_root(
    m: &Arc<SessionManager>,
    root: &Path,
    goal: &str,
    contract: Option<CompletionContract>,
) -> (SessionHandle, TaskId, String) {
    let ws = m.create_workspace(root.to_str().unwrap()).unwrap();
    let h = m
        .create_session(ws, "completion-race", "fake", "fake")
        .unwrap();
    let task_id = h.task_id().unwrap();
    let now = h.now_ms();
    h.create_task(Task {
        task_id,
        session_id: h.id(),
        goal: goal.into(),
        created_ms: now,
        updated_ms: now,
        ..Default::default()
    })
    .unwrap();
    if let Some(contract) = contract {
        let rev = h.task_revision(task_id).unwrap();
        h.set_completion_contract(task_id, rev, contract).unwrap();
    }
    let tree = repo_tree(root);
    h.ledger_integration_record_set(&faktor_session::IntegrationRecordRow {
        run_id: "run-race".into(),
        task_id: task_id.raw(),
        base_revision: None,
        base_snapshot: None,
        run_base_snapshot: None,
        candidate_snapshot: None,
        landed_snapshot: Some(tree.clone()),
        proof_basis_digest: None,
        integration_txn_id: None,
        final_root: root.to_string_lossy().into_owned(),
        final_snapshot_hash: tree.clone(),
        integrated_files: vec![],
        integrated_file_count: 0,
        integrated_files_digest: String::new(),
        conflicts: vec![],
        conflict_count: 0,
        sources: vec![],
        source_count: 0,
        sources_digest: String::new(),
        at_ms: now,
    })
    .unwrap();
    (h, task_id, tree)
}

fn passing_record_with_tree(
    h: &SessionHandle,
    task_id: TaskId,
    tree: &str,
) -> VerificationRecordId {
    h.create_verification_record(
        task_id,
        Some(tree.to_string()),
        vec![],
        vec![],
        vec![],
        vec![],
        None,
        VerificationStatus::Passed,
        h.now_ms(),
    )
    .unwrap()
}

fn assert_task_stays_verifying_and_steps_invalidated(h: &SessionHandle, task_id: TaskId) {
    assert_eq!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::Verifying,
        "an invalidated completion must never complete the task"
    );
    // The gate reports the unmet step EITHER as Invalidated (the step
    // itself was refused before its side effect) or as the durable
    // snapshot mismatch of an already-succeeded step whose root moved —
    // both are RETRYABLE refusals, never a terminal failure.
    match h.completion_contract_gate(task_id).unwrap() {
        CompletionContractGate::Refused(TaskError::CompletionStepNotSucceeded {
            status, ..
        }) => assert_eq!(status, CompletionStepOutcome::Invalidated),
        CompletionContractGate::Refused(TaskError::CompletionStepSnapshotMismatch { .. }) => {}
        other => panic!("expected a retryable refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn injected_edit_after_verification_before_commit_blocks_the_commit() {
    let (dir, m) = manager();
    let repo = init_repo(dir.path());
    std::fs::write(repo.join("feature.txt"), "verified content\n").unwrap();
    let (h, task_id, tree) =
        session_bound_to_root(&m, &repo, "race commit", Some(contract(true, false, false)));
    drive_to_verifying(&h, task_id);
    let proof = passing_record_with_tree(&h, task_id, &tree);
    // The injected edit lands AFTER the proof was minted and BEFORE the
    // commit step runs: the per-step revalidation must refuse.
    let repo_hook = repo.clone();
    let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all())
        .with_pre_step_hook(Arc::new(move |step| {
            if step == CompletionStep::Commit {
                std::fs::write(repo_hook.join("feature.txt"), "injected edit\n").unwrap();
            }
        }));
    let report = runner
        .run_completion_steps(&h, task_id, proof, &ctx(&repo, "race commit"))
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Commit),
        Some(CompletionStepOutcome::Invalidated),
        "{report:?}"
    );
    // No commit happened: the init commit is the only one.
    let log = git_output(&repo, &["log", "--oneline"]);
    assert_eq!(log.lines().count(), 1, "no injected commit may land: {log}");
    assert_task_stays_verifying_and_steps_invalidated(&h, task_id);
}

#[tokio::test]
async fn injected_edit_after_commit_before_push_blocks_the_push() {
    let (dir, m) = manager();
    let repo = init_repo(dir.path());
    let bare = add_bare_remote(dir.path(), &repo);
    std::fs::write(repo.join("feature.txt"), "verified content\n").unwrap();
    let (h, task_id, tree) =
        session_bound_to_root(&m, &repo, "race push", Some(contract(true, true, false)));
    drive_to_verifying(&h, task_id);
    let proof = passing_record_with_tree(&h, task_id, &tree);
    let repo_hook = repo.clone();
    let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all())
        .with_pre_step_hook(Arc::new(move |step| {
            if step == CompletionStep::Push {
                std::fs::write(repo_hook.join("feature.txt"), "injected after commit\n").unwrap();
            }
        }));
    let report = runner
        .run_completion_steps(&h, task_id, proof, &ctx(&repo, "race push"))
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Commit),
        Some(CompletionStepOutcome::Succeeded),
        "{report:?}"
    );
    assert_eq!(
        report.outcome_of(CompletionStep::Push),
        Some(CompletionStepOutcome::Invalidated),
        "{report:?}"
    );
    // The commit landed; the push must NOT have reached the remote.
    assert_eq!(git_output(&repo, &["log", "--oneline"]).lines().count(), 2);
    let pushed = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "main"])
        .current_dir(&bare)
        .output()
        .unwrap();
    assert!(
        pushed.status.code() != Some(0),
        "the remote must have no branch after an invalidated push"
    );
    assert_task_stays_verifying_and_steps_invalidated(&h, task_id);
}

#[cfg(unix)]
#[tokio::test]
async fn injected_edit_after_push_before_pr_blocks_the_pr() {
    let (dir, m) = manager();
    let repo = init_repo(dir.path());
    add_bare_remote(dir.path(), &repo);
    std::fs::write(repo.join("feature.txt"), "verified content\n").unwrap();
    let marker = dir.path().join("pr-ran.marker");
    let script = fake_pr_script(
        dir.path(),
        &format!(
            "echo ran > {}\necho https://example.test/pr/9",
            marker.display()
        ),
    );
    let config = CompletionStepsConfig {
        pr_command: Some(format!("{} {{branch}}", script.display())),
        ..Default::default()
    };
    let (h, task_id, tree) =
        session_bound_to_root(&m, &repo, "race pr", Some(contract(true, true, true)));
    drive_to_verifying(&h, task_id);
    let proof = passing_record_with_tree(&h, task_id, &tree);
    let repo_hook = repo.clone();
    let runner =
        runner(dir.path(), config, allow_all()).with_pre_step_hook(Arc::new(move |step| {
            if step == CompletionStep::Pr {
                std::fs::write(repo_hook.join("feature.txt"), "injected after push\n").unwrap();
            }
        }));
    let report = runner
        .run_completion_steps(&h, task_id, proof, &ctx(&repo, "race pr"))
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Pr),
        Some(CompletionStepOutcome::Invalidated),
        "{report:?}"
    );
    assert!(
        !marker.exists(),
        "the PR command must not run against a moved root"
    );
    assert!(report.pr_url.is_none());
    let bare = dir.path().join("remote.git");
    let head = git_output(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(
        git_output(&bare, &["rev-parse", "main"]),
        head,
        "the push that ran before the edit stays landed"
    );
    assert_task_stays_verifying_and_steps_invalidated(&h, task_id);
}

#[tokio::test]
async fn crash_after_push_before_status_row_is_idempotent_via_remote_tracking() {
    let (dir, m) = manager();
    let repo = init_repo(dir.path());
    let bare = add_bare_remote(dir.path(), &repo);
    std::fs::write(repo.join("work.txt"), "landed before the crash\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "faktor: land the work"]);
    // The commit+push happened, but the durable step row never landed
    // (the "crash"). The runner must detect the pushed HEAD through the
    // remote-tracking ref and record Succeeded WITHOUT pushing again.
    git(&repo, &["push", "-q", "origin", "main"]);
    let (h, task_id, tree) = session_bound_to_root(
        &m,
        &repo,
        "crash after push",
        Some(contract(false, true, false)),
    );
    drive_to_verifying(&h, task_id);
    let proof = passing_record_with_tree(&h, task_id, &tree);
    let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
    let report = runner
        .run_completion_steps(&h, task_id, proof, &ctx(&repo, "crash after push"))
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Push),
        Some(CompletionStepOutcome::Succeeded),
        "{report:?}"
    );
    let detail = &report
        .records
        .iter()
        .find(|r| r.step == CompletionStep::Push)
        .unwrap()
        .detail;
    assert!(detail.contains("idempotent replay"), "{detail}");
    let head = git_output(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(git_output(&bare, &["rev-parse", "main"]), head);
    let rows = step_rows(&h, task_id);
    assert_eq!(
        rows.len(),
        1,
        "one durable row, no duplicate push: {rows:?}"
    );
    assert_eq!(rows[0].status, CompletionStepOutcome::Succeeded);
    assert_eq!(
        h.completion_contract_gate(task_id).unwrap(),
        CompletionContractGate::Satisfied
    );
}

/// Wave-0 commit race: a human edit injected AFTER the proof
/// revalidation and BEFORE the tree construction is a typed refusal with
/// nothing committed — the tree comes from the verified manifest, so a
/// diverged worktree can never be committed, and the race window between
/// `add` and `commit` does not exist at all.
#[tokio::test]
async fn edit_between_revalidation_and_tree_build_is_refused_with_nothing_committed() {
    let (dir, m) = manager();
    let repo = init_repo(dir.path());
    let (h, task_id, tree) =
        session_bound_to_root(&m, &repo, "commit race", Some(contract(true, false, false)));
    drive_to_verifying(&h, task_id);
    let proof = passing_record_with_tree(&h, task_id, &tree);
    let head_before = git_output(&repo, &["rev-parse", "HEAD"]);
    let hook_repo = repo.clone();
    let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all())
        .with_commit_tree_hook(Arc::new(move || {
            std::fs::write(hook_repo.join("README.md"), "human edit mid-add\n").unwrap();
        }));
    let report = runner
        .run_completion_steps(&h, task_id, proof, &ctx(&repo, "commit race"))
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Commit),
        Some(CompletionStepOutcome::Failed),
        "{report:?}"
    );
    let detail = &report
        .records
        .iter()
        .find(|r| r.step == CompletionStep::Commit)
        .unwrap()
        .detail;
    assert!(detail.contains("divergence"), "{detail}");
    assert_eq!(
        git_output(&repo, &["rev-parse", "HEAD"]),
        head_before,
        "nothing was committed"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).unwrap(),
        "human edit mid-add\n",
        "the human edit is preserved, never overwritten by a commit"
    );
}

/// Wave-0 PR binding: an artifact whose recorded remote head no longer
/// matches the LIVE remote head refuses the PR step before any provider
/// call (no PR on top of a moved commit).
#[tokio::test]
async fn pr_refuses_when_the_recorded_remote_head_moved() {
    let (dir, m) = manager();
    let repo = init_repo(dir.path());
    let bare = add_bare_remote(dir.path(), &repo);
    let (h, task_id, tree) = session_bound_to_root(
        &m,
        &repo,
        "pr remote drift",
        Some(contract(false, false, true)),
    );
    let (revision, _) = h.completion_contract(task_id).unwrap().unwrap();
    drive_to_verifying(&h, task_id);
    let proof = passing_record_with_tree(&h, task_id, &tree);
    let head = git_output(&repo, &["rev-parse", "HEAD"]);
    // The push step published this exact commit earlier: record the
    // artifact with the encoded remote ref.
    let manifest = crate::runtime::merge::canonical_manifest_rows(&repo, MAX_BASE_ENTRIES)
        .unwrap()
        .into_iter()
        .map(|(path, state)| VerifiedManifestEntry {
            path: path.to_string_lossy().into_owned(),
            state,
        })
        .collect();
    let artifact = VerifiedGitArtifact {
        task_id: task_id.raw(),
        revision: revision.raw(),
        verification_record: proof.raw(),
        verified_root_digest: tree.clone(),
        verified_manifest: manifest,
        git_tree_oid: Some(git_output(&repo, &["rev-parse", "HEAD^{tree}"])),
        commit_oid: Some(head.clone()),
        local_ref: Some("refs/heads/main".into()),
        remote_ref: Some(format!("origin:refs/heads/main@{head}")),
        updated_ms: h.now_ms(),
    };
    h.ledger_verified_git_artifact_set(&artifact).unwrap();
    // A human/other runtime moves the remote ref to a different commit.
    std::fs::write(repo.join("other.txt"), "moved on\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "faktor: other"]);
    let moved = git_output(&repo, &["rev-parse", "HEAD"]);
    assert_ne!(moved, head);
    git(
        &repo,
        &[
            "push",
            "-q",
            "--force",
            "origin",
            &format!("{moved}:refs/heads/main"),
        ],
    );
    assert_eq!(git_output(&bare, &["rev-parse", "main"]), moved);
    // Keep the local HEAD on the verified commit: only the REMOTE moved.
    git(&repo, &["reset", "-q", "--hard", &head]);
    assert_eq!(git_output(&repo, &["rev-parse", "HEAD"]), head);
    let provider = scm_fake::FakeScmProvider::new();
    let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all())
        .with_scm_provider(provider.clone());
    let report = runner
        .run_completion_steps(&h, task_id, proof, &ctx(&repo, "pr remote drift"))
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Pr),
        Some(CompletionStepOutcome::Failed),
        "{report:?}"
    );
    let detail = &report
        .records
        .iter()
        .find(|r| r.step == CompletionStep::Pr)
        .unwrap()
        .detail;
    assert!(detail.contains("live remote head"), "{detail}");
    assert_eq!(
        provider.pr_calls(),
        0,
        "the provider must never be called for a moved remote"
    );
}

// --------------------------- durable external-operation identity (B)

/// A repo whose `origin` names a GitHub repository but has no real
/// remote: the typed path never touches the network (the provider seam
/// does), only the push/idempotency checks read refs.
fn init_repo_with_github_remote(root: &Path) -> std::path::PathBuf {
    let repo = init_repo(root);
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/widgets.git",
        ],
    );
    repo
}

fn native_pr_fixture(
    dir: &Path,
    goal: &str,
) -> (Arc<SessionManager>, SessionHandle, TaskId, String, PathBuf) {
    let m =
        SessionManager::open(dir.join("native-pr-store"), dir.join("native-pr-cas"), true).unwrap();
    let repo = init_repo_with_github_remote(dir);
    let (h, task_id, tree) =
        session_bound_to_root(&m, &repo, goal, Some(contract(false, false, true)));
    drive_to_verifying(&h, task_id);
    (m, h, task_id, tree, repo)
}

fn native_pr_runner(dir: &Path, provider: Arc<scm_fake::FakeScmProvider>) -> CompletionStepRunner {
    runner(dir, CompletionStepsConfig::default(), allow_all()).with_scm_provider(provider)
}

fn pr_operation_rows(h: &SessionHandle) -> Vec<faktor_session::ledger::ExternalOperationRow> {
    h.ledger_external_operations().unwrap()
}

fn contract_revision(h: &SessionHandle, task_id: TaskId) -> u64 {
    h.completion_contract(task_id)
        .unwrap()
        .expect("contract")
        .0
        .raw()
}

/// The create path journals the EXACT operation identity BEFORE the
/// remote call and the remote object identity after it: two rows, the
/// first `Prepared` (no remote identity), the last `Completed`.
#[tokio::test]
async fn native_pr_records_the_operation_before_the_remote_call() {
    let dir = tempfile::tempdir().unwrap();
    let (m, h, task_id, tree, repo) = native_pr_fixture(dir.path(), "native pr");
    let provider = scm_fake::FakeScmProvider::new();
    let runner = native_pr_runner(dir.path(), provider.clone());
    let report = runner
        .run_completion_steps(
            &h,
            task_id,
            passing_record_with_tree(&h, task_id, &tree),
            &ctx(&repo, "native pr"),
        )
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Pr),
        Some(CompletionStepOutcome::Succeeded),
        "{report:?}"
    );
    assert_eq!(provider.pr_calls(), 1);
    assert_eq!(provider.remote_creations(), 1);
    let key = native_pr_operation_key(task_id, contract_revision(&h, task_id), "github");
    let ops: Vec<_> = pr_operation_rows(&h)
        .into_iter()
        .filter(|row| row.operation_key == key)
        .collect();
    assert_eq!(ops.len(), 2, "prepared then completed: {ops:?}");
    assert_eq!(
        ops[0].state,
        faktor_session::ledger::ExternalOperationState::Prepared
    );
    assert!(ops[0].remote_object_id.is_none());
    assert_eq!(ops[0].input.head, "main");
    assert_eq!(
        ops[1].state,
        faktor_session::ledger::ExternalOperationState::Completed
    );
    assert!(ops[1].remote_object_id.is_some());
    assert!(ops[1].remote_object_version.is_some());
    assert!(
        ops[1].reconciled_at.is_none(),
        "first create is not a reconciliation"
    );
    let _ = m;
}

/// Crash BEFORE the remote call: exactly one `Prepared` row exists and
/// the provider was never called. The restart reconciles from the
/// recorded identity and creates the PR exactly once.
#[tokio::test]
async fn native_pr_crash_before_remote_call_retries_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let (_m, h, task_id, tree, repo) = native_pr_fixture(dir.path(), "crash before call");
    let provider = scm_fake::FakeScmProvider::new();
    let key = native_pr_operation_key(task_id, contract_revision(&h, task_id), "github");
    let crashed = native_pr_runner(dir.path(), provider.clone())
        .with_pr_crash_seam(PrOperationCrashPoint::BeforeRemoteCall);
    let err = crashed
        .run_completion_steps(
            &h,
            task_id,
            passing_record_with_tree(&h, task_id, &tree),
            &ctx(&repo, "crash before call"),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, CompletionStepError::InjectedCrash(_)),
        "{err}"
    );
    assert_eq!(provider.pr_calls(), 0, "the remote call never happened");
    let ops: Vec<_> = pr_operation_rows(&h)
        .into_iter()
        .filter(|row| row.operation_key == key)
        .collect();
    assert_eq!(ops.len(), 1);
    assert_eq!(
        ops[0].state,
        faktor_session::ledger::ExternalOperationState::Prepared
    );
    // The crash left NO step status row: the restart re-executes.
    assert!(step_rows(&h, task_id).is_empty());
    let restarted = native_pr_runner(dir.path(), provider.clone());
    let report = restarted
        .run_completion_steps(
            &h,
            task_id,
            passing_record_with_tree(&h, task_id, &tree),
            &ctx(&repo, "crash before call"),
        )
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Pr),
        Some(CompletionStepOutcome::Succeeded),
        "{report:?}"
    );
    assert_eq!(provider.pr_calls(), 1, "retried exactly once");
    assert_eq!(provider.remote_creations(), 1);
    let ops: Vec<_> = pr_operation_rows(&h)
        .into_iter()
        .filter(|row| row.operation_key == key)
        .collect();
    assert_eq!(ops.len(), 2);
    assert!(ops[1].reconciled_at.is_some(), "the restart reconciled");
}

/// Crash AFTER the remote creation but BEFORE the completion row: the
/// restart reconciles through the marker/head/base lookup, so the
/// provider creates exactly ONE PR and the step then certifies.
#[tokio::test]
async fn native_pr_crash_after_remote_call_reconciles_without_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let (_m, h, task_id, tree, repo) = native_pr_fixture(dir.path(), "crash after call");
    let provider = scm_fake::FakeScmProvider::new();
    let key = native_pr_operation_key(task_id, contract_revision(&h, task_id), "github");
    let crashed = native_pr_runner(dir.path(), provider.clone())
        .with_pr_crash_seam(PrOperationCrashPoint::AfterRemoteCallBeforeRecord);
    let err = crashed
        .run_completion_steps(
            &h,
            task_id,
            passing_record_with_tree(&h, task_id, &tree),
            &ctx(&repo, "crash after call"),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, CompletionStepError::InjectedCrash(_)),
        "{err}"
    );
    assert_eq!(provider.remote_creations(), 1, "the PR was created once");
    let ops: Vec<_> = pr_operation_rows(&h)
        .into_iter()
        .filter(|row| row.operation_key == key)
        .collect();
    assert_eq!(ops.len(), 1, "the completion row never landed");
    assert_eq!(
        ops[0].state,
        faktor_session::ledger::ExternalOperationState::Prepared
    );
    assert!(step_rows(&h, task_id).is_empty());
    let restarted = native_pr_runner(dir.path(), provider.clone());
    let report = restarted
        .run_completion_steps(
            &h,
            task_id,
            passing_record_with_tree(&h, task_id, &tree),
            &ctx(&repo, "crash after call"),
        )
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Pr),
        Some(CompletionStepOutcome::Succeeded),
        "{report:?}"
    );
    assert_eq!(
        provider.remote_creations(),
        1,
        "reconciliation must NOT create a duplicate PR"
    );
    assert_eq!(provider.pull_request_count(), 1);
    let ops: Vec<_> = pr_operation_rows(&h)
        .into_iter()
        .filter(|row| row.operation_key == key)
        .collect();
    assert_eq!(ops.len(), 2);
    assert!(ops[1].reconciled_at.is_some());
    assert_eq!(ops[1].remote_object_id.as_deref(), Some("1"));
}

/// A durable row under the SAME operation key with a DIFFERENT input
/// identity (a different head) is a typed refusal: the runtime never
/// retargets an external operation and never touches the provider.
#[tokio::test]
async fn native_pr_input_identity_conflict_is_a_typed_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let (_m, h, task_id, tree, repo) = native_pr_fixture(dir.path(), "conflict");
    let revision = contract_revision(&h, task_id);
    let key = native_pr_operation_key(task_id, revision, "github");
    let input = ExternalOperationInput {
        organization: "acme".into(),
        repository: "widgets".into(),
        head: "someone-elses-branch".into(),
        base: "main".into(),
        marker: native_pr_marker(task_id, revision),
    };
    let conflicting = ExternalOperationRow {
        id: ExternalOperationRow::content_id(&key, "github", SCM_PULL_REQUEST_KIND, &input),
        operation_key: key.clone(),
        provider: "github".into(),
        kind: SCM_PULL_REQUEST_KIND.into(),
        input,
        state: faktor_session::ledger::ExternalOperationState::Prepared,
        remote_object_id: None,
        remote_object_version: None,
        started_at: h.now_ms(),
        reconciled_at: None,
    };
    h.ledger_external_operation_set(&conflicting).unwrap();
    let provider = scm_fake::FakeScmProvider::new();
    let runner = native_pr_runner(dir.path(), provider.clone());
    let report = runner
        .run_completion_steps(
            &h,
            task_id,
            passing_record_with_tree(&h, task_id, &tree),
            &ctx(&repo, "conflict"),
        )
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Pr),
        Some(CompletionStepOutcome::Failed),
        "{report:?}"
    );
    let detail = &report
        .records
        .iter()
        .find(|r| r.step == CompletionStep::Pr)
        .unwrap()
        .detail;
    assert!(
        detail.contains("external_operation_input_identity_conflict"),
        "{detail}"
    );
    assert_eq!(provider.pr_calls(), 0, "no remote call on a conflict");
    assert_eq!(provider.branch_calls(), 0);
}

/// A recorded remote-object VERSION that the provider no longer reports
/// is a typed refusal: the run never silently certifies a moved object.
#[tokio::test]
async fn native_pr_remote_object_version_mismatch_is_a_typed_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let (_m, h, task_id, tree, repo) = native_pr_fixture(dir.path(), "version mismatch");
    let revision = contract_revision(&h, task_id);
    let key = native_pr_operation_key(task_id, revision, "github");
    let input = ExternalOperationInput {
        organization: "acme".into(),
        repository: "widgets".into(),
        head: "main".into(),
        base: "main".into(),
        marker: native_pr_marker(task_id, revision),
    };
    let completed = ExternalOperationRow {
        id: ExternalOperationRow::content_id(&key, "github", SCM_PULL_REQUEST_KIND, &input),
        operation_key: key.clone(),
        provider: "github".into(),
        kind: SCM_PULL_REQUEST_KIND.into(),
        input,
        state: faktor_session::ledger::ExternalOperationState::Completed,
        remote_object_id: Some("pr-1".into()),
        // The recorded version; the provider has moved on.
        remote_object_version: Some("v1".into()),
        started_at: h.now_ms(),
        reconciled_at: None,
    };
    h.ledger_external_operation_set(&completed).unwrap();
    let provider = scm_fake::FakeScmProvider::new();
    provider.force_pull_request_version("moved-v9");
    let runner = native_pr_runner(dir.path(), provider.clone());
    let report = runner
        .run_completion_steps(
            &h,
            task_id,
            passing_record_with_tree(&h, task_id, &tree),
            &ctx(&repo, "version mismatch"),
        )
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Pr),
        Some(CompletionStepOutcome::Failed),
        "{report:?}"
    );
    let detail = &report
        .records
        .iter()
        .find(|r| r.step == CompletionStep::Pr)
        .unwrap()
        .detail;
    assert!(
        detail.contains("external_operation_remote_object_mismatch"),
        "{detail}"
    );
    assert!(detail.contains("moved-v9"), "{detail}");
}

/// A branch that moved away from the exact verified head is a typed
/// refusal BEFORE any PR is created: the adapter's branch result is
/// re-asserted against the verified head (defense in depth for a
/// provider that returns a moved branch instead of conflicting).
#[tokio::test]
async fn native_pr_branch_version_drift_is_a_typed_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let (_m, h, task_id, tree, repo) = native_pr_fixture(dir.path(), "branch drift");
    let provider = scm_fake::FakeScmProvider::new();
    provider.force_branch_head("someone-elses-sha");
    let runner = native_pr_runner(dir.path(), provider.clone());
    let report = runner
        .run_completion_steps(
            &h,
            task_id,
            passing_record_with_tree(&h, task_id, &tree),
            &ctx(&repo, "branch drift"),
        )
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Pr),
        Some(CompletionStepOutcome::Failed),
        "{report:?}"
    );
    let detail = &report
        .records
        .iter()
        .find(|r| r.step == CompletionStep::Pr)
        .unwrap()
        .detail;
    assert!(
        detail.contains("external_operation_remote_object_mismatch"),
        "{detail}"
    );
    assert_eq!(provider.pr_calls(), 0, "no PR operation after branch drift");
    // The durable Prepared row (written before the remote call) is the
    // only row: no remote object identity was ever journaled.
    let ops = pr_operation_rows(&h);
    assert_eq!(ops.len(), 1, "{ops:?}");
    assert_eq!(
        ops[0].state,
        faktor_session::ledger::ExternalOperationState::Prepared
    );
    assert!(ops[0].remote_object_id.is_none());
}

/// The orchestrator maps the contract onto the adapter request exactly
/// (organization/repository from the git remote, branch/base, verified
/// head, task marker, derived PR title and the marker-carrying body) and
/// the adapter's certified PR number/version land in the durable row.
#[tokio::test]
async fn native_pr_maps_the_contract_onto_the_scm_request() {
    let dir = tempfile::tempdir().unwrap();
    let (_m, h, task_id, tree, repo) = native_pr_fixture(dir.path(), "map the request");
    let provider = scm_fake::FakeScmProvider::new();
    let runner = native_pr_runner(dir.path(), provider.clone());
    let goal = "map the request";
    let report = runner
        .run_completion_steps(
            &h,
            task_id,
            passing_record_with_tree(&h, task_id, &tree),
            &ctx(&repo, goal),
        )
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Pr),
        Some(CompletionStepOutcome::Succeeded),
        "{report:?}"
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    let request = &requests[0];
    let revision = contract_revision(&h, task_id);
    assert_eq!(
        request.operation_id.as_str(),
        native_pr_operation_key(task_id, revision, "github")
    );
    assert_eq!(request.organization, "acme");
    assert_eq!(request.repository, "widgets");
    assert_eq!(request.branch, "main");
    assert_eq!(request.base, "main");
    assert_eq!(request.head_sha, git_output(&repo, &["rev-parse", "HEAD"]));
    assert_eq!(request.marker, native_pr_marker(task_id, revision));
    assert_eq!(request.title, commit_message(goal));
    assert!(
        request.body.contains(&request.marker) && request.body.contains(&task_id.to_string()),
        "the PR body names the task and the reconciliation marker: {}",
        request.body
    );
    let complete = pr_operation_rows(&h)
        .into_iter()
        .find(|row| {
            row.operation_key == native_pr_operation_key(task_id, revision, "github")
                && row.state == faktor_session::ledger::ExternalOperationState::Completed
        })
        .expect("completed external-operation row");
    assert_eq!(complete.remote_object_id.as_deref(), Some("1"));
    assert_eq!(complete.remote_object_version.as_deref(), Some("v1"));
    assert_eq!(
        report.pr_url.as_deref(),
        Some("https://github.com/acme/widgets/pull/1")
    );
}

/// A typed provider refusal on the adapter seam is a `Failed` step
/// carrying the stable `ScmError` code — never a silent success.
#[tokio::test]
async fn native_pr_provider_refusal_is_a_typed_step_failure() {
    let dir = tempfile::tempdir().unwrap();
    let (_m, h, task_id, tree, repo) = native_pr_fixture(dir.path(), "provider refusal");
    let provider = scm_fake::FakeScmProvider::new();
    provider.fail_reconciliation("installation token minting failed");
    let runner = native_pr_runner(dir.path(), provider.clone());
    let report = runner
        .run_completion_steps(
            &h,
            task_id,
            passing_record_with_tree(&h, task_id, &tree),
            &ctx(&repo, "provider refusal"),
        )
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Pr),
        Some(CompletionStepOutcome::Failed),
        "{report:?}"
    );
    let detail = &report
        .records
        .iter()
        .find(|r| r.step == CompletionStep::Pr)
        .unwrap()
        .detail;
    assert!(detail.contains("scm_provider"), "{detail}");
    assert!(detail.contains("scm_transport"), "{detail}");
}

/// The generic `pr_program` expert command implements NO typed
/// reconciliation protocol: on the production proof path it is a typed
/// terminal refusal, never a certification — and it never even runs.
#[cfg(unix)]
#[tokio::test]
async fn pr_program_without_reconciliation_cannot_certify_the_native_pr_step() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo_with_github_remote(dir.path());
    let marker = dir.path().join("pr-program-ran.marker");
    let script = fake_pr_script(
        dir.path(),
        &format!(
            "echo ran > {}\necho https://example.test/pr/1",
            marker.display()
        ),
    );
    let config = CompletionStepsConfig {
        pr_program: Some(script.display().to_string()),
        pr_args: vec!["{branch}".into()],
        ..Default::default()
    };
    let m = SessionManager::open(
        dir.path().join("expert-store"),
        dir.path().join("expert-cas"),
        true,
    )
    .unwrap();
    let (h, task_id, tree) =
        session_bound_to_root(&m, &repo, "expert pr", Some(contract(false, false, true)));
    drive_to_verifying(&h, task_id);
    let proof = passing_record_with_tree(&h, task_id, &tree);
    let runner = runner(dir.path(), config, allow_all());
    let report = runner
        .run_completion_steps(&h, task_id, proof, &ctx(&repo, "expert pr"))
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Pr),
        Some(CompletionStepOutcome::Failed),
        "{report:?}"
    );
    let detail = &report
        .records
        .iter()
        .find(|r| r.step == CompletionStep::Pr)
        .unwrap()
        .detail;
    assert!(
        detail.contains(PR_REQUIRES_TYPED_RECONCILIATION),
        "{detail}"
    );
    assert!(!marker.exists(), "the expert command must not run");
    assert!(pr_operation_rows(&h)
        .iter()
        .all(|row| row.kind != SCM_PULL_REQUEST_KIND));
    // The gate refuses terminally: the native PR step is never satisfied.
    assert!(matches!(
        h.completion_contract_gate(task_id).unwrap(),
        CompletionContractGate::Refused(TaskError::CompletionStepFailed { .. })
    ));
}

/// A contracted PR step with NO SCM provider wired is an EXPLICIT
/// configuration blocker: fail closed with the stable code (never the
/// historic silent `Skipped`, never a rebuild without the provider).
#[tokio::test]
async fn native_pr_without_a_wired_scm_provider_is_a_configuration_blocker() {
    let dir = tempfile::tempdir().unwrap();
    let (_m, h, task_id, tree, repo) = native_pr_fixture(dir.path(), "no pr provider");
    let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
    let report = runner
        .run_completion_steps(
            &h,
            task_id,
            passing_record_with_tree(&h, task_id, &tree),
            &ctx(&repo, "no pr provider"),
        )
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Pr),
        Some(CompletionStepOutcome::Failed),
        "{report:?}"
    );
    let detail = &report
        .records
        .iter()
        .find(|r| r.step == CompletionStep::Pr)
        .unwrap()
        .detail;
    assert!(detail.contains(NATIVE_PR_SCM_NOT_CONFIGURED), "{detail}");
    assert!(detail.contains("no SCM provider is configured"), "{detail}");
    assert!(
        pr_operation_rows(&h).is_empty(),
        "the blocker is recorded before any operation identity is minted"
    );
}

/// The test-only legacy path keeps the historic unconfigured-command
/// `Skipped`; only the production proof path carries the hard blocker.
#[tokio::test]
async fn legacy_unconfigured_native_pr_path_stays_skipped() {
    let (dir, m) = manager();
    let (h, task_id) = session_with_task(
        &m,
        "legacy no pr config",
        Some(contract(false, false, true)),
    );
    let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
    let report = runner
        .run(
            &h,
            task_id,
            &ctx(&dir.path().join("nope"), "legacy no pr config"),
        )
        .await
        .unwrap();
    assert_eq!(
        report.outcome_of(CompletionStep::Pr),
        Some(CompletionStepOutcome::Skipped),
        "{report:?}"
    );
}

/// The repository parser is exact: only a two-segment GitHub reference
/// is accepted; the installation identity is resolved by the SCM adapter
/// from the durable synced rows, never from the URL.
#[test]
fn scm_repository_parsing_is_exact() {
    let parsed = parse_scm_repository("https://github.com/acme/widgets.git").expect("https url");
    assert_eq!(parsed, ("acme".to_string(), "widgets".to_string()));
    let parsed = parse_scm_repository("git@github.com:acme/widgets.git").expect("scp");
    assert_eq!(parsed, ("acme".to_string(), "widgets".to_string()));
    for rejected in [
        "",
        "https://gitlab.com/acme/widgets.git",
        "https://github.com/acme",
        "https://github.com/acme/widgets/extra",
        "/tmp/remote.git",
        "https://github.com/acme/widgets.git/../../evil",
    ] {
        assert!(
            parse_scm_repository(rejected).is_none(),
            "{rejected:?} must be refused"
        );
    }
}
