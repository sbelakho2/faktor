#[cfg(test)]
mod terminal_authority_tests {
    use super::super::*;
    use faktor_terminal::{LimitState, TreeBudgets};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn manager() -> (tempfile::TempDir, Arc<SessionManager>) {
        let dir = tempfile::tempdir().unwrap();
        let manager =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        (dir, manager)
    }

    fn session(manager: &Arc<SessionManager>, title: &str) -> String {
        let ws = manager.create_workspace("/tmp").unwrap();
        manager
            .create_session(ws, title, "fake", "m")
            .unwrap()
            .id()
            .to_string()
    }

    fn spawn_request(command: &str, args: &[&str]) -> TerminalSpawnRequest {
        TerminalSpawnRequest {
            command: command.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            cwd: None,
            env: Vec::new(),
            rows: 24,
            cols: 80,
        }
    }

    /// A probe that records the marker handed to the first spawn and returns
    /// it for every later probe of that pid (simulated live process).
    fn recording_probe() -> (IdentityProbe, Arc<Mutex<HashMap<u32, i64>>>) {
        let map = Arc::new(Mutex::new(HashMap::new()));
        let map2 = Arc::clone(&map);
        let probe: IdentityProbe = Arc::new(move |pid| {
            let mut map = map2.lock().unwrap();
            Some(*map.entry(pid).or_insert(1_700_000_000_000))
        });
        (probe, map)
    }

    /// The EXPLICIT user-granted shell contract: tests that exercise a REAL
    /// PTY spawn carry it (the authority default is the fail-closed
    /// `os_isolated` shape, so a test grants exactly like an operator).
    fn granted_policy() -> TerminalAuthorityPolicy {
        TerminalAuthorityPolicy::explicit_user_granted_shell()
    }

    /// A service over `manager`/`probe` whose authority carries `policy`
    /// (the explicit test seam; unregistered).
    fn service_over(
        manager: &Arc<SessionManager>,
        probe: IdentityProbe,
        policy: TerminalAuthorityPolicy,
    ) -> Arc<TerminalService> {
        TerminalService::with_execution_authority(
            manager.clone(),
            probe,
            Arc::new(SessionExecutionAuthority::with_policy(
                manager.clone(),
                policy,
            )),
        )
    }

    /// A spawn-hook service whose authority carries the explicit test grant.
    fn hook_service(
        manager: &Arc<SessionManager>,
        probe: IdentityProbe,
        hook: SpawnHook,
    ) -> Arc<TerminalService> {
        let mut service =
            TerminalService::with_probe_and_policy(manager.clone(), probe, granted_policy());
        service.spawn_hook = Some(hook);
        Arc::new(service)
    }

    fn terminal_kinds(handle: &SessionHandle) -> Vec<TerminalEventKind> {
        handle
            .ledger_terminal_rows(None)
            .unwrap()
            .into_iter()
            .map(|record| record.kind)
            .collect()
    }

    /// Spawn or skip (documented platform refusal: PTY spawn is refused on
    /// platforms without a backend, exactly like every other PTY test).
    fn spawn_or_skip(
        service: &Arc<TerminalService>,
        sid: &str,
        request: &TerminalSpawnRequest,
    ) -> Option<TerminalCreation> {
        match service.spawn(sid, request) {
            Ok(creation) => Some(creation),
            Err(TerminalServiceError::Refused(_)) | Err(TerminalServiceError::Unavailable(_)) => {
                None
            }
            Err(other) => panic!("unexpected spawn failure: {other}"),
        }
    }

    /// The platform-honest outcome of one `os_isolated` spawn attempt: on
    /// Linux the child is either admitted INSIDE the sandbox network
    /// namespace (the durable profile records the applied `deny_all`
    /// isolation and the child body ran) or — when the host's kernel/
    /// user-namespace policy refuses the unshare — refused typed with no
    /// child and nothing journaled. On every other platform the typed
    /// fail-closed refusal is the ONLY possible outcome (platform truth).
    /// Returns the creation when the spawn was admitted.
    fn expect_os_isolated_outcome(
        service: &Arc<TerminalService>,
        manager: &Arc<SessionManager>,
        sid: &str,
        request: &TerminalSpawnRequest,
        marker: &std::path::Path,
    ) -> Option<TerminalCreation> {
        match service.spawn(sid, request) {
            Ok(creation) => {
                if !cfg!(target_os = "linux") {
                    panic!(
                        "only Linux has a per-process network-isolation backend; no other \
                         platform may admit an os_isolated spawn"
                    );
                }
                let profile =
                    ExecutionProfile::parse(&creation.view.execution_profile).expect("profile");
                assert_eq!(profile.shell, "os_isolated");
                assert_eq!(profile.network, "required");
                assert_eq!(
                    profile.network_isolation, "deny_all",
                    "an admitted os_isolated spawn records the isolation it applied"
                );
                // The child body runs after the spawn returns: bound the wait
                // so this is never a fork/exec race.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while !marker.exists() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                assert!(
                    marker.exists(),
                    "an admitted os_isolated child runs its body (a network namespace confines \
                     the network, never the filesystem)"
                );
                Some(creation)
            }
            Err(TerminalServiceError::Denied(message)) => {
                assert!(message.contains("sandbox unavailable"), "{message}");
                assert!(message.contains("DenyAll"), "{message}");
                assert!(
                    message.contains("unenforced") || message.contains("unconfined"),
                    "{message}"
                );
                assert_eq!(service.live_rows(), 0);
                assert!(
                    !marker.exists(),
                    "the refused child never exec'd its program body"
                );
                let handle = manager
                    .get_session(SessionId::new(sid.parse().unwrap()))
                    .unwrap()
                    .unwrap();
                assert!(
                    handle.ledger_terminal_rows(None).unwrap().is_empty(),
                    "a fail-closed refusal journals nothing"
                );
                None
            }
            other => panic!(
                "an os_isolated spawn must isolate the child or refuse typed, never anything \
                 else: {:?}",
                other.err()
            ),
        }
    }

    /// Drain a live terminal until `needle` appears (or `timeout` elapses);
    /// returns everything accumulated.
    #[cfg(target_os = "linux")]
    fn drain_until(handle: &TerminalHandle, needle: &str, timeout: std::time::Duration) -> String {
        let deadline = std::time::Instant::now() + timeout;
        let mut out: Vec<u8> = Vec::new();
        loop {
            out.extend_from_slice(&handle.drain_output());
            let text = String::from_utf8_lossy(&out).into_owned();
            if text.contains(needle) || std::time::Instant::now() >= deadline {
                return text;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    // ------------------------------------------------ execution authority

    /// A manager whose workspace root is a REAL directory (`root`), plus one
    /// session on it.
    fn manager_at(
        base: &std::path::Path,
        root: &std::path::Path,
        title: &str,
    ) -> (Arc<SessionManager>, String) {
        let manager = SessionManager::open(base.join("store"), base.join("cas"), true).unwrap();
        let ws = manager.create_workspace(root.to_str().unwrap()).unwrap();
        let sid = manager
            .create_session(ws, title, "fake", "m")
            .unwrap()
            .id()
            .to_string();
        (manager, sid)
    }

    fn principal_of(manager: &Arc<SessionManager>, sid: &str) -> TerminalPrincipal {
        let row = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap()
            .row()
            .unwrap();
        TerminalPrincipal::owner(row.id, row.task_id, None)
    }

    #[test]
    fn authority_denies_foreign_and_unknown_principals_before_any_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("candidate");
        std::fs::create_dir_all(&root).unwrap();
        let (manager, a) = manager_at(dir.path(), &root, "authority-a");
        let ws = manager
            .get_session(SessionId::new(a.parse().unwrap()))
            .unwrap()
            .unwrap()
            .row()
            .unwrap()
            .workspace_id;
        let b = manager
            .create_session(ws, "authority-b", "fake", "m")
            .unwrap()
            .id()
            .to_string();
        let authority = SessionExecutionAuthority::new(manager.clone());
        let request = spawn_request("/bin/sh", &["-c", "true"]);

        // A principal that is not the requested session's owner is denied
        // typed (the denied spawn creates no PTY and journals nothing).
        let foreign = principal_of(&manager, &a);
        match authority.authorize_terminal_spawn(&foreign, &b, &request) {
            Err(ExecutionDenial::ForeignPrincipal { .. }) => {}
            other => panic!("foreign principal must be denied: {other:?}"),
        }
        // An unknown session id is denied typed, never guessed.
        let unknown = TerminalPrincipal::owner(SessionId::new(9_999_999), TaskId::new(1), None);
        match authority.authorize_terminal_spawn(&unknown, "9999999", &request) {
            Err(ExecutionDenial::UnknownSession { .. }) => {}
            other => panic!("unknown session must be denied: {other:?}"),
        }
        // A hostile session string is denied typed too.
        match authority.authorize_terminal_spawn(&unknown, "not-a-session", &request) {
            Err(ExecutionDenial::InvalidSession { .. }) => {}
            other => panic!("hostile session id must be denied: {other:?}"),
        }

        // Service level: a denied admission leaves no live row and no
        // durable terminal row behind, for EITHER session.
        let denied_policy = TerminalAuthorityPolicy {
            granted: CapabilitySet::EMPTY,
            ..TerminalAuthorityPolicy::default()
        };
        let (probe, _map) = recording_probe();
        let service = TerminalService::with_execution_authority(
            manager.clone(),
            probe,
            Arc::new(SessionExecutionAuthority::with_policy(
                manager.clone(),
                denied_policy,
            )),
        );
        // An unknown session is the service's own typed refusal.
        match service.spawn("424242", &request) {
            Err(TerminalServiceError::Invalid(_)) => {}
            other => panic!("unknown session spawn must be refused: {:?}", other.err()),
        }
        // A real session whose grant denies Execute is denied typed and
        // journals nothing, on both sessions.
        for target in [&a, &b] {
            match service.spawn(target, &request) {
                Err(TerminalServiceError::Denied(_)) => {}
                other => panic!("capability denial must be typed: {:?}", other.err()),
            }
        }
        assert_eq!(service.live_rows(), 0);
        for target in [&a, &b] {
            let handle = manager
                .get_session(SessionId::new(target.parse().unwrap()))
                .unwrap()
                .unwrap();
            assert!(handle.ledger_terminal_rows(None).unwrap().is_empty());
        }
    }

    /// Audit P1: a workspace-only policy (both external rules `Deny`) with
    /// a non-Required network guarantee (so no netns is needed here) maps
    /// the PTY spawn to the Required filesystem demand, projects the
    /// enforcement-honest tag, and carries the candidate root as the one
    /// confinement root. This asserts the policy → spawn wiring without
    /// requiring PTY hardware; the spawn layer itself confines or refuses
    /// typed (terminal-crate kernel tests cover the syscalls).
    #[test]
    fn workspace_guarantee_maps_the_pty_spawn_to_the_filesystem_demand() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("candidate");
        std::fs::create_dir_all(&root).unwrap();
        let (manager, sid) = manager_at(dir.path(), &root, "authority-fs-demand");
        let principal = principal_of(&manager, &sid);
        let policy = TerminalAuthorityPolicy {
            sandbox: SandboxPolicy {
                read_external: Rule::Deny,
                write_external: Rule::Deny,
                execute_shell: Rule::Allow,
                network_guarantee: SandboxGuarantee::None,
                shell_execution: ShellExecutionMode::NetworkCapableUserGranted,
                filesystem_guarantee: faktor_sandbox::FilesystemGuarantee::Required,
                ..SandboxPolicy::default()
            },
            ..TerminalAuthorityPolicy::default()
        };
        let authority = SessionExecutionAuthority::with_policy(manager, policy);
        let admitted = authority
            .authorize_terminal_spawn(&principal, &sid, &spawn_request("/bin/sh", &["-c", "true"]))
            .unwrap();
        assert_eq!(
            admitted.profile().filesystem,
            if cfg!(target_os = "linux") {
                "workspace"
            } else {
                "application-policy-only"
            },
            "the projection must be enforcement-honest"
        );
        assert_eq!(admitted.filesystem_isolation().as_tag(), "workspace");
        assert!(admitted.filesystem_isolation().is_required());
        assert_eq!(
            admitted.filesystem_isolation().roots(),
            [root.canonicalize().unwrap()],
            "the candidate root is the one confinement root"
        );
        assert_eq!(admitted.profile().filesystem_isolation, "workspace");
    }

    #[test]
    fn authority_denies_external_cwd_without_grant_and_admits_it_with_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("candidate");
        let inside = root.join("sub");
        std::fs::create_dir_all(&inside).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let (manager, sid) = manager_at(dir.path(), &root, "authority-cwd");
        let principal = principal_of(&manager, &sid);
        let authority = SessionExecutionAuthority::new(manager.clone());

        // Default cwd = the candidate root itself, admitted with the exact
        // profile (no external grant).
        let admitted = authority
            .authorize_terminal_spawn(&principal, &sid, &spawn_request("/bin/sh", &["-c", "true"]))
            .unwrap();
        assert_eq!(
            admitted.cwd(),
            root.canonicalize().unwrap().to_string_lossy()
        );
        assert!(!admitted.profile().external_cwd_granted);
        assert_eq!(
            admitted.profile().candidate_root,
            root.canonicalize().unwrap().to_string_lossy()
        );

        // A cwd inside the candidate (relative or absolute) is admitted.
        let mut inside_request = spawn_request("/bin/sh", &["-c", "true"]);
        inside_request.cwd = Some("sub".into());
        let admitted = authority
            .authorize_terminal_spawn(&principal, &sid, &inside_request)
            .unwrap();
        assert_eq!(
            admitted.cwd(),
            inside.canonicalize().unwrap().to_string_lossy()
        );
        assert!(!admitted.profile().external_cwd_granted);

        // A path outside the candidate is denied unless THIs authority
        // carries an explicit grant; traversal out is denied too.
        for escape in [
            outside.path().to_string_lossy().into_owned(),
            format!("{}", outside.path().join("..").display()),
            "../..".to_string(),
        ] {
            let mut request = spawn_request("/bin/sh", &["-c", "true"]);
            request.cwd = Some(escape.clone());
            match authority.authorize_terminal_spawn(&principal, &sid, &request) {
                Err(ExecutionDenial::CwdOutsideCandidate { .. }) => {}
                other => panic!("external cwd {escape:?} must be denied: {other:?}"),
            }
        }

        // A nonexistent cwd is a typed denial, never a guessed root.
        let mut ghost = spawn_request("/bin/sh", &["-c", "true"]);
        ghost.cwd = Some("does-not-exist".into());
        match authority.authorize_terminal_spawn(&principal, &sid, &ghost) {
            Err(ExecutionDenial::CwdUnavailable { .. }) => {}
            other => panic!("missing cwd must be denied: {other:?}"),
        }

        // The explicit grant admits the external cwd and the profile says so.
        let grant = TerminalAuthorityPolicy {
            external_cwd_grants: vec![outside.path().to_path_buf()],
            ..TerminalAuthorityPolicy::default()
        };
        let granting = SessionExecutionAuthority::with_policy(manager, grant);
        let mut request = spawn_request("/bin/sh", &["-c", "true"]);
        request.cwd = Some(outside.path().to_string_lossy().into_owned());
        let admitted = granting
            .authorize_terminal_spawn(&principal, &sid, &request)
            .unwrap();
        assert_eq!(
            admitted.cwd(),
            outside.path().canonicalize().unwrap().to_string_lossy()
        );
        assert!(admitted.profile().external_cwd_granted);
    }

    #[test]
    fn capability_denied_shell_creates_no_pty_and_no_durable_row() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("candidate");
        std::fs::create_dir_all(&root).unwrap();
        let (manager, sid) = manager_at(dir.path(), &root, "authority-caps");
        let policy = TerminalAuthorityPolicy {
            granted: CapabilitySet::from_kinds(&[CapabilityKind::Read]),
            ..TerminalAuthorityPolicy::default()
        };
        let authority = SessionExecutionAuthority::with_policy(manager.clone(), policy);
        let (probe, _map) = recording_probe();
        let service =
            TerminalService::with_execution_authority(manager.clone(), probe, Arc::new(authority));
        match service.spawn(&sid, &spawn_request("/bin/sleep", &["30"])) {
            Err(TerminalServiceError::Denied(message)) => {
                assert!(message.contains("execute"), "{message}");
            }
            other => panic!("capability denial must be typed: {:?}", other.err()),
        }
        assert_eq!(service.live_rows(), 0);
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        assert!(
            handle.ledger_terminal_rows(None).unwrap().is_empty(),
            "a denied spawn journals nothing"
        );
    }

    #[test]
    fn required_network_guarantee_isolates_the_pty_spawn_or_refuses_typed_never_unenforced() {
        // Phase D (audit P0-39) + the interactive-terminal gap: the
        // crate-default `SandboxPolicy` demands OS-level network isolation
        // (`Required` + `os_isolated`) — exactly the authority's fail-closed
        // default (the host config default too). The spawn under it is
        // EITHER genuinely confined (Linux: the shared network-namespace
        // backend, the SAME pre-exec `unshare` hook the shell supervisor
        // installs) OR refused TYPED before any child exists (macOS/Windows
        // by platform truth; Linux when the unshare is refused) — never a
        // warn-and-run unenforced shell. (The explicit operator grant admits
        // the strictly weaker network-capable spawn; see the profile test.)
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("candidate");
        std::fs::create_dir_all(&root).unwrap();
        let (manager, sid) = manager_at(dir.path(), &root, "authority-required-gate");
        let policy = TerminalAuthorityPolicy {
            sandbox: SandboxPolicy {
                execute_shell: Rule::Allow,
                // Verbatim secure default: Required + OsIsolated.
                ..SandboxPolicy::default()
            },
            ..TerminalAuthorityPolicy::default()
        };
        assert_eq!(policy.sandbox.network_guarantee, SandboxGuarantee::Required);
        assert_eq!(
            policy.sandbox.shell_execution,
            ShellExecutionMode::OsIsolated
        );
        assert!(
            policy.sandbox.validate().is_ok(),
            "the secure pairing is valid"
        );

        // Authority level: the admitted spawn carries the DenyAll
        // requirement (the policy→spawn mapping is the one locus) and the
        // profile evidence names the OS-isolation demand honestly.
        let authority = SessionExecutionAuthority::with_policy(manager.clone(), policy.clone());
        let request = spawn_request("/bin/sh", &["-c", "true"]);
        let admitted = authority
            .authorize_terminal_spawn(&principal_of(&manager, &sid), &sid, &request)
            .expect("the admission itself records the profile");
        assert_eq!(admitted.network_isolation(), NetworkIsolation::DenyAll);
        assert_eq!(admitted.profile().network, "required");
        assert_eq!(admitted.profile().shell, "os_isolated");

        // Spawn level: isolated (Linux) or the typed fail-closed refusal —
        // never an unenforced child and never an unapplied isolation claim.
        let marker = root.join("required-body-ran.txt");
        let program = format!("echo ran > {}", marker.display());
        let request = spawn_request("/bin/sh", &["-c", program.as_str()]);
        let service = service_with_policy(&manager, policy);
        let creation = expect_os_isolated_outcome(&service, &manager, &sid, &request, &marker);
        #[cfg(not(target_os = "linux"))]
        assert!(
            creation.is_none(),
            "platform truth: with no backend the os_isolated spawn MUST refuse typed"
        );
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        for record in handle.ledger_terminal_rows(None).unwrap() {
            let profile = ExecutionProfile::parse(&record.row.execution_profile).expect("profile");
            assert_eq!(
                profile.network_isolation, "deny_all",
                "every durable row of an os_isolated terminal records the applied isolation"
            );
        }
        if let Some(creation) = creation {
            let _ = service.kill(&sid, creation.handle.terminal_id(), "required cleanup");
        }
    }

    /// Linux-only: an `os_isolated` interactive terminal runs INSIDE the
    /// sandbox network namespace with a fully working pty (stdin/stdout
    /// through the master). When the host refuses the unshare (no
    /// CAP_SYS_ADMIN and no unprivileged user namespaces) the spawn is
    /// refused typed and the test skips — the backend is never faked into a
    /// grant.
    #[cfg(target_os = "linux")]
    #[test]
    fn os_isolated_terminal_runs_interactively_inside_the_network_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("candidate");
        std::fs::create_dir_all(&root).unwrap();
        let (manager, sid) = manager_at(dir.path(), &root, "os-isolated-interactive");
        let policy = TerminalAuthorityPolicy {
            sandbox: SandboxPolicy {
                execute_shell: Rule::Allow,
                ..SandboxPolicy::default()
            },
            ..TerminalAuthorityPolicy::default()
        };
        let service = service_with_policy(&manager, policy);
        let creation = match service.spawn(&sid, &spawn_request("/bin/sh", &[])) {
            Ok(creation) => creation,
            Err(TerminalServiceError::Denied(message)) => {
                assert!(message.contains("sandbox unavailable"), "{message}");
                assert_eq!(service.live_rows(), 0);
                let handle = manager
                    .get_session(SessionId::new(sid.parse().unwrap()))
                    .unwrap()
                    .unwrap();
                assert!(
                    handle.ledger_terminal_rows(None).unwrap().is_empty(),
                    "an unprivileged refusal journals nothing"
                );
                eprintln!(
                    "skipping: the sandbox network namespace is not creatable here: {message}"
                );
                return;
            }
            other => panic!("unexpected spawn failure: {:?}", other.err()),
        };
        let profile = ExecutionProfile::parse(&creation.view.execution_profile).expect("profile");
        assert_eq!(profile.shell, "os_isolated");
        assert_eq!(profile.network, "required");
        assert_eq!(profile.network_isolation, "deny_all");
        // The arithmetic marker can only appear as the shell's OUTPUT, not
        // as the line-discipline echo of our input.
        creation.handle.write(b"echo isolated-$((40+2))\n").unwrap();
        let out = drain_until(
            &creation.handle,
            "isolated-42",
            std::time::Duration::from_secs(10),
        );
        assert!(
            out.contains("isolated-42"),
            "interactive round-trip through the isolated pty: {out}"
        );
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        assert_eq!(
            terminal_kinds(&handle),
            vec![TerminalEventKind::Created, TerminalEventKind::Running]
        );
        assert!(service
            .kill(
                &sid,
                creation.handle.terminal_id(),
                "isolated interactive cleanup"
            )
            .unwrap());
        assert_eq!(
            terminal_kinds(&handle),
            vec![
                TerminalEventKind::Created,
                TerminalEventKind::Running,
                TerminalEventKind::Killed
            ]
        );
    }

    /// Linux-only: inside the `os_isolated` namespace NOTHING is reachable —
    /// no external route (the namespace has no interface and no route) and
    /// no loopback either (the reused DenyAll backend leaves `lo` DOWN by
    /// design; that is the documented loopback semantics) — while the SAME
    /// probe under the explicit user-granted shell reaches the daemon's
    /// loopback listener. Skips (typed) when the host refuses the unshare or
    /// `curl` is absent.
    #[cfg(target_os = "linux")]
    #[test]
    fn os_isolated_terminal_denies_external_and_loopback_while_the_grant_reaches_the_daemon() {
        use std::io::{Read, Write};
        // A daemon-side loopback listener: the granted child must reach it,
        // the isolated child must not.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let server = {
            let hits = Arc::clone(&hits);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                while !stop.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let mut buf = [0u8; 1024];
                            let _ = stream.read(&mut buf);
                            let _ = stream.write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                            );
                            hits.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(20));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        // `192.0.2.0/24` (TEST-NET-1) is never routable: inside the fresh
        // namespace the connect fails immediately at the route layer, and
        // outside it blackholes (bounded by --max-time).
        let script = format!(
            "command -v curl >/dev/null 2>&1 || {{ echo PROBE_NO_CURL; echo PROBE_DONE; exit 0; }}; \
             curl -sS --max-time 5 -o /dev/null http://127.0.0.1:{port}/ && echo PROBE_LOOPBACK_OK || echo PROBE_LOOPBACK_FAIL; \
             curl -sS --max-time 5 -o /dev/null http://192.0.2.1:443/ && echo PROBE_EXTERNAL_OK || echo PROBE_EXTERNAL_FAIL; \
             echo PROBE_DONE"
        );
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("candidate");
        std::fs::create_dir_all(&root).unwrap();
        let (manager, sid) = manager_at(dir.path(), &root, "os-isolated-network");

        let isolated = service_with_policy(
            &manager,
            TerminalAuthorityPolicy {
                sandbox: SandboxPolicy {
                    execute_shell: Rule::Allow,
                    ..SandboxPolicy::default()
                },
                ..TerminalAuthorityPolicy::default()
            },
        );
        let (iso_creation, iso_out) =
            match isolated.spawn(&sid, &spawn_request("/bin/sh", &["-c", script.as_str()])) {
                Ok(creation) => {
                    let out = drain_until(
                        &creation.handle,
                        "PROBE_DONE",
                        std::time::Duration::from_secs(30),
                    );
                    (creation, out)
                }
                Err(TerminalServiceError::Denied(message)) => {
                    assert!(message.contains("sandbox unavailable"), "{message}");
                    assert_eq!(isolated.live_rows(), 0);
                    eprintln!(
                        "skipping: the sandbox network namespace is not creatable here: {message}"
                    );
                    stop.store(true, Ordering::SeqCst);
                    let _ = server.join();
                    return;
                }
                other => panic!("unexpected isolated spawn failure: {:?}", other.err()),
            };
        if iso_out.contains("PROBE_NO_CURL") {
            eprintln!("skipping: curl is not available on the test host");
            let _ = isolated.kill(&sid, iso_creation.handle.terminal_id(), "no-curl cleanup");
            stop.store(true, Ordering::SeqCst);
            let _ = server.join();
            return;
        }
        assert!(
            iso_out.contains("PROBE_LOOPBACK_FAIL"),
            "loopback must be unreachable inside the DenyAll namespace (lo is left DOWN by \
             design): {iso_out}"
        );
        assert!(
            iso_out.contains("PROBE_EXTERNAL_FAIL"),
            "no external destination may be reachable inside the DenyAll namespace: {iso_out}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "the isolated child never reached the daemon listener"
        );
        let _ = isolated.kill(
            &sid,
            iso_creation.handle.terminal_id(),
            "isolated network cleanup",
        );

        // The explicit user grant: the SAME probe reaches the daemon's
        // loopback listener (grant mode is honestly network-capable).
        let granted = service_with_policy(&manager, granted_policy());
        let (grant_creation, grant_out) =
            match granted.spawn(&sid, &spawn_request("/bin/sh", &["-c", script.as_str()])) {
                Ok(creation) => {
                    let out = drain_until(
                        &creation.handle,
                        "PROBE_DONE",
                        std::time::Duration::from_secs(30),
                    );
                    (creation, out)
                }
                other => panic!("the grant must admit the PTY: {:?}", other.err()),
            };
        assert!(
            grant_out.contains("PROBE_LOOPBACK_OK"),
            "the user-granted shell must reach the daemon loopback: {grant_out}"
        );
        assert!(
            hits.load(Ordering::SeqCst) >= 1,
            "the daemon listener observed the granted child's connection"
        );
        let _ = granted.kill(
            &sid,
            grant_creation.handle.terminal_id(),
            "grant network cleanup",
        );
        stop.store(true, Ordering::SeqCst);
        let _ = server.join();

        // Journal honesty: the isolated row records deny_all/os_isolated,
        // the grant row inherit/network_capable_user_granted — never the
        // other way round.
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        let profiles: Vec<ExecutionProfile> = handle
            .ledger_terminal_rows(None)
            .unwrap()
            .iter()
            .map(|record| ExecutionProfile::parse(&record.row.execution_profile).expect("profile"))
            .collect();
        assert!(
            profiles
                .iter()
                .any(|p| p.shell == "os_isolated" && p.network_isolation == "deny_all"),
            "{profiles:?}"
        );
        assert!(
            profiles
                .iter()
                .any(|p| p.shell == "network_capable_user_granted"
                    && p.network_isolation == "inherit"),
            "{profiles:?}"
        );
    }

    #[test]
    fn authorized_spawn_records_the_exact_profile_durably_and_it_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("candidate");
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let (manager, sid) = manager_at(dir.path(), &root, "authority-profile");
        let (probe, _map) = recording_probe();
        // The production authority over the EXPLICIT operator grant
        // (`TerminalAuthorityPolicy::explicit_user_granted_shell`): the
        // authority default is the fail-closed `os_isolated` shape, and only
        // an explicit grant admits the PTY. The durable profile records that
        // honest, strictly-weaker mode — never an OS-isolation claim the PTY
        // layer does not enforce.
        let service = TerminalService::with_execution_authority(
            manager.clone(),
            probe,
            Arc::new(SessionExecutionAuthority::with_policy(
                manager.clone(),
                granted_policy(),
            )),
        );
        let mut request = spawn_request("/bin/sleep", &["30"]);
        request.cwd = Some("sub".into());
        request.env = vec!["PATH".into(), "HOME".into()];
        let Some(creation) = spawn_or_skip(&service, &sid, &request) else {
            return;
        };
        let view = &creation.view;
        assert!(
            !view.execution_profile.is_empty(),
            "the durable row records the effective profile"
        );
        let profile = ExecutionProfile::parse(&view.execution_profile).expect("profile JSON");
        assert_eq!(profile.session_id, sid.parse::<u64>().unwrap());
        assert_eq!(profile.task_id, 1);
        assert_eq!(
            profile.candidate_root,
            root.canonicalize().unwrap().to_string_lossy()
        );
        assert_eq!(profile.cwd, sub.canonicalize().unwrap().to_string_lossy());
        assert_eq!(profile.capabilities, "*");
        assert_eq!(profile.filesystem, "workspace+external:ask-ask");
        assert_eq!(profile.network, "none");
        assert_eq!(
            profile.shell, "network_capable_user_granted",
            "the durable row records the explicit user-granted shell shape, never os_isolated"
        );
        assert_eq!(
            profile.network_isolation, "inherit",
            "the grant applies no OS-level isolation and records exactly that"
        );
        assert_eq!(
            profile.filesystem_isolation, "inherit",
            "an external-Ask policy demands no workspace confinement and records exactly that"
        );
        assert_eq!(profile.budgets, TerminalBudgets::default());
        assert_eq!(
            profile.env_names,
            vec!["PATH".to_string(), "HOME".to_string()]
        );
        assert!(!profile.external_cwd_granted);

        // The durable Created/Running rows carry the byte-identical profile.
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        let rows = handle.ledger_terminal_rows(None).unwrap();
        assert_eq!(rows.len(), 2);
        for record in &rows {
            assert_eq!(record.row.execution_profile, view.execution_profile);
        }

        // Reopen: a fresh service over the same durable store projects the
        // SAME profile (the profile is a durable row fact, not daemon memory).
        let restarted = TerminalService::detached(manager.clone(), Arc::new(|_pid: u32| None));
        let views = restarted.list(&sid).unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].execution_profile, view.execution_profile);
        let rebuilt = ExecutionProfile::parse(&views[0].execution_profile).expect("profile JSON");
        assert_eq!(rebuilt.cwd, profile.cwd);
        let _ = service.kill(&sid, view.terminal_id.as_str(), "profile cleanup");
    }

    #[test]
    fn default_and_configured_os_isolated_isolate_or_fail_closed_and_record_the_mode() {
        // The authority default (no injected policy) IS the configured
        // default `[sandbox] shell = "os_isolated"` — `Required` +
        // `OsIsolated`, exactly what `for_configured_sandbox` carries from a
        // default config. The mode is enforced (DenyAll requirement) and is
        // what the profile records; the spawn either genuinely isolates the
        // child (Linux, through the shared network-namespace backend) or is
        // refused typed BEFORE any child exists (no backend platform /
        // refused unshare) — never an unenforced run.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("candidate");
        std::fs::create_dir_all(&root).unwrap();
        let (manager, sid) = manager_at(dir.path(), &root, "authority-os-isolated");

        let default_policy = TerminalAuthorityPolicy::default();
        assert_eq!(
            default_policy.sandbox.shell_execution,
            ShellExecutionMode::OsIsolated
        );
        assert_eq!(
            default_policy.sandbox.network_guarantee,
            SandboxGuarantee::Required
        );
        assert!(default_policy.sandbox.validate().is_ok());

        // The configured path carries the config's contract verbatim.
        let configured = SandboxPolicy {
            execute_shell: Rule::Allow,
            ..SandboxPolicy::default()
        };
        let from_config = TerminalAuthorityPolicy::for_configured_sandbox(&configured);
        let state = from_config.shell_execution_state();
        assert_eq!(state.mode, ShellExecutionMode::OsIsolated);
        assert_eq!(state.network_guarantee, SandboxGuarantee::Required);
        assert_eq!(
            from_config, default_policy,
            "the authority default is exactly what the configured default resolves to"
        );

        let principal = principal_of(&manager, &sid);
        for (index, policy) in [default_policy, from_config].into_iter().enumerate() {
            // Authority level: admission records the honest profile before
            // any PTY exists; the spawn gate derives DenyAll from it.
            let authority = SessionExecutionAuthority::with_policy(manager.clone(), policy.clone());
            let admitted = authority
                .authorize_terminal_spawn(
                    &principal,
                    &sid,
                    &spawn_request("/bin/sh", &["-c", "true"]),
                )
                .expect("admission records the profile before any PTY exists");
            assert_eq!(admitted.network_isolation(), NetworkIsolation::DenyAll);
            assert_eq!(admitted.profile().shell, "os_isolated");
            assert_eq!(admitted.profile().network, "required");

            // Spawn level: isolated (Linux) or the typed fail-closed refusal.
            let marker = root.join(format!("os-isolated-body-ran-{index}.txt"));
            let program = format!("echo ran > {}", marker.display());
            let service = service_with_policy(&manager, policy);
            let creation = expect_os_isolated_outcome(
                &service,
                &manager,
                &sid,
                &spawn_request("/bin/sh", &["-c", program.as_str()]),
                &marker,
            );
            #[cfg(not(target_os = "linux"))]
            assert!(
                creation.is_none(),
                "platform truth: with no backend the os_isolated spawn MUST refuse typed"
            );
            if let Some(creation) = creation {
                let _ = service.kill(&sid, creation.handle.terminal_id(), "os-isolated cleanup");
            }
        }

        // Journal honesty: every durable row of an os_isolated terminal
        // records os_isolated/deny_all; a fail-closed platform journals
        // nothing at all.
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        let rows = handle.ledger_terminal_rows(None).unwrap();
        for record in &rows {
            let profile = ExecutionProfile::parse(&record.row.execution_profile).expect("profile");
            assert_eq!(profile.shell, "os_isolated");
            assert_eq!(profile.network, "required");
            assert_eq!(profile.network_isolation, "deny_all");
        }
        #[cfg(not(target_os = "linux"))]
        assert!(rows.is_empty(), "a fail-closed refusal journals nothing");
    }

    #[test]
    fn explicit_grant_spawns_and_records_the_honest_user_granted_tag() {
        // Only the EXPLICIT operator grant admits a PTY; the durable profile
        // records the strictly weaker mode honestly, never an OS-isolation
        // claim the PTY layer does not enforce.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("candidate");
        std::fs::create_dir_all(&root).unwrap();
        let (manager, sid) = manager_at(dir.path(), &root, "authority-explicit-grant");
        let policy = granted_policy();
        let state = policy.shell_execution_state();
        assert_eq!(state.mode, ShellExecutionMode::NetworkCapableUserGranted);
        assert_eq!(state.network_guarantee, SandboxGuarantee::None);
        assert!(policy.sandbox.validate().is_ok());
        let service = service_with_policy(&manager, policy);
        let Some(creation) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let profile = ExecutionProfile::parse(&creation.view.execution_profile).expect("profile");
        assert_eq!(profile.shell, "network_capable_user_granted");
        assert_eq!(profile.network, "none");
        assert_eq!(
            profile.network_isolation, "inherit",
            "grant mode spawns inside the daemon namespace and records it honestly"
        );
        let _ = service.kill(&sid, creation.handle.terminal_id(), "grant cleanup");
    }

    #[test]
    fn interactive_terminal_class_defaults_to_the_honest_grant_and_explicit_isolation_is_honored() {
        // The TRUST-CLASS split: interactive session terminals are
        // user-initiated. UNSET (`None`) is the user-granted default — NOT
        // the agent class's `os_isolated`/`Required` secure default, which
        // is what broke every IDE terminal on backend-less platforms. An
        // EXPLICIT `Some(OsIsolated)` restores the isolation/typed-refusal
        // contract for terminals too.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("candidate");
        std::fs::create_dir_all(&root).unwrap();
        let (manager, sid) = manager_at(dir.path(), &root, "interactive-trust-class");
        let principal = principal_of(&manager, &sid);

        // UNSET: the agent-class `Required` guarantee passed alongside is
        // ignored for terminals (distinct trust class); the admitted
        // profile records the grant honestly.
        let default_terminal = TerminalAuthorityPolicy::for_interactive_session_terminals(
            None,
            SandboxGuarantee::Required,
        );
        let state = default_terminal.shell_execution_state();
        assert_eq!(state.mode, ShellExecutionMode::NetworkCapableUserGranted);
        assert_eq!(state.network_guarantee, SandboxGuarantee::None);
        assert!(default_terminal.sandbox.validate().is_ok());
        let authority =
            SessionExecutionAuthority::with_policy(manager.clone(), default_terminal.clone());
        let admitted = authority
            .authorize_terminal_spawn(&principal, &sid, &spawn_request("/bin/sh", &["-c", "true"]))
            .expect("the interactive default admits");
        assert_eq!(
            admitted.network_isolation(),
            NetworkIsolation::Inherit,
            "the interactive default must never demand a backend-less isolation"
        );
        assert_eq!(admitted.profile().shell, "network_capable_user_granted");
        assert_eq!(admitted.profile().network, "none");
        assert_eq!(admitted.profile().network_isolation, "inherit");

        let service = service_with_policy(&manager, default_terminal);
        if let Some(creation) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        {
            let profile =
                ExecutionProfile::parse(&creation.view.execution_profile).expect("profile");
            assert_eq!(profile.shell, "network_capable_user_granted");
            assert_eq!(profile.network, "none");
            assert_eq!(profile.network_isolation, "inherit");
            let _ = service.kill(
                &sid,
                creation.handle.terminal_id(),
                "interactive default cleanup",
            );
        }

        // EXPLICIT isolation on a FRESH session (the earlier spawn's rows
        // would otherwise trip the helper's no-journal-on-refusal check).
        let dir_iso = tempfile::tempdir().unwrap();
        let root_iso = dir_iso.path().join("candidate");
        std::fs::create_dir_all(&root_iso).unwrap();
        let (manager_iso, sid_iso) = manager_at(dir_iso.path(), &root_iso, "interactive-isolated");
        let isolated = TerminalAuthorityPolicy::for_interactive_session_terminals(
            Some(ShellExecutionMode::OsIsolated),
            SandboxGuarantee::Required,
        );
        assert_eq!(
            isolated.sandbox.shell_execution,
            ShellExecutionMode::OsIsolated
        );
        assert_eq!(
            isolated.sandbox.network_guarantee,
            SandboxGuarantee::Required
        );
        assert!(isolated.sandbox.validate().is_ok());
        let authority =
            SessionExecutionAuthority::with_policy(manager_iso.clone(), isolated.clone());
        let admitted = authority
            .authorize_terminal_spawn(
                &principal_of(&manager_iso, &sid_iso),
                &sid_iso,
                &spawn_request("/bin/sh", &["-c", "true"]),
            )
            .expect("explicit isolation admits with the demand recorded");
        assert_eq!(admitted.network_isolation(), NetworkIsolation::DenyAll);
        assert_eq!(admitted.profile().shell, "os_isolated");
        assert_eq!(admitted.profile().network, "required");
        let marker = root_iso.join("explicit-terminal-isolation-ran.txt");
        let program = format!("echo ran > {}", marker.display());
        let service = service_with_policy(&manager_iso, isolated);
        let creation = expect_os_isolated_outcome(
            &service,
            &manager_iso,
            &sid_iso,
            &spawn_request("/bin/sh", &["-c", program.as_str()]),
            &marker,
        );
        #[cfg(not(target_os = "linux"))]
        assert!(
            creation.is_none(),
            "explicit terminal isolation must refuse typed where no backend exists"
        );
        if let Some(creation) = creation {
            let _ = service.kill(&sid_iso, creation.handle.terminal_id(), "isolation cleanup");
        }

        // EXPLICIT grant: the same honest contract as the default, now
        // chosen; the configured non-required guarantee is carried. A
        // contradictory pairing (grant + `Required`) is refused at the
        // config boundary, so this constructor stays total by keeping the
        // grant's invariant instead of fabricating an isolation demand.
        let explicit_grant = TerminalAuthorityPolicy::for_interactive_session_terminals(
            Some(ShellExecutionMode::NetworkCapableUserGranted),
            SandboxGuarantee::None,
        );
        assert_eq!(
            explicit_grant.shell_execution_state(),
            TerminalAuthorityPolicy::explicit_user_granted_shell().shell_execution_state()
        );
        let normalized = TerminalAuthorityPolicy::for_interactive_session_terminals(
            Some(ShellExecutionMode::NetworkCapableUserGranted),
            SandboxGuarantee::Required,
        );
        assert_eq!(normalized.sandbox.network_guarantee, SandboxGuarantee::None);
        assert!(normalized.sandbox.validate().is_ok());
    }

    #[test]
    fn a_planted_grant_never_widens_another_authority() {
        // Daemon A is explicitly granted; daemon B (a separate session
        // manager, i.e. a separate authority) is constructed with the
        // fail-closed default. A's planted grant must not change what B
        // enforces, and the registry's first-wins rule returns B's secure
        // service untouched when a later grant names the same manager.
        let dir_a = tempfile::tempdir().unwrap();
        let root_a = dir_a.path().join("candidate");
        std::fs::create_dir_all(&root_a).unwrap();
        let (manager_a, sid_a) = manager_at(dir_a.path(), &root_a, "granted-daemon");
        let granted = service_with_policy(&manager_a, granted_policy());
        let Some(creation) = spawn_or_skip(&granted, &sid_a, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let profile = ExecutionProfile::parse(&creation.view.execution_profile).expect("profile");
        assert_eq!(profile.shell, "network_capable_user_granted");

        let dir_b = tempfile::tempdir().unwrap();
        let root_b = dir_b.path().join("candidate");
        std::fs::create_dir_all(&root_b).unwrap();
        let (manager_b, sid_b) = manager_at(dir_b.path(), &root_b, "secure-daemon");
        let secure = TerminalService::for_manager_with_policy(
            &manager_b,
            TerminalAuthorityPolicy::default(),
        );
        let later_grant = TerminalService::for_manager_with_policy(&manager_b, granted_policy());
        assert!(
            Arc::ptr_eq(&secure, &later_grant),
            "one service per manager: the first (secure) policy wins, a later grant cannot widen it"
        );
        for (index, service) in [&secure, &later_grant].into_iter().enumerate() {
            let authority = SessionExecutionAuthority::with_policy(
                manager_b.clone(),
                TerminalAuthorityPolicy::default(),
            );
            let admitted = authority
                .authorize_terminal_spawn(
                    &principal_of(&manager_b, &sid_b),
                    &sid_b,
                    &spawn_request("/bin/sh", &["-c", "true"]),
                )
                .unwrap();
            assert_eq!(admitted.profile().shell, "os_isolated");
            // B's spawn is isolated (Linux) or refused typed — never the
            // planted grant's network-capable shape.
            let marker = root_b.join(format!("planted-body-ran-{index}.txt"));
            let program = format!("echo ran > {}", marker.display());
            let creation = expect_os_isolated_outcome(
                service,
                &manager_b,
                &sid_b,
                &spawn_request("/bin/sh", &["-c", program.as_str()]),
                &marker,
            );
            if let Some(creation) = creation {
                let _ = service.kill(&sid_b, creation.handle.terminal_id(), "planted cleanup");
            }
        }
        let handle_b = manager_b
            .get_session(SessionId::new(sid_b.parse().unwrap()))
            .unwrap()
            .unwrap();
        for record in handle_b.ledger_terminal_rows(None).unwrap() {
            let profile = ExecutionProfile::parse(&record.row.execution_profile).expect("profile");
            assert_eq!(
                profile.shell, "os_isolated",
                "a planted grant must never widen another authority"
            );
            assert_eq!(profile.network_isolation, "deny_all");
        }

        // A's explicitly granted terminal is untouched by B's refusal.
        assert_eq!(granted.list(&sid_a).unwrap().len(), 1);
        let _ = granted.kill(&sid_a, creation.handle.terminal_id(), "planted cleanup");
    }

    /// A service over `manager` whose authority carries `policy`.
    fn service_with_policy(
        manager: &Arc<SessionManager>,
        policy: TerminalAuthorityPolicy,
    ) -> Arc<TerminalService> {
        let (probe, _map) = recording_probe();
        TerminalService::with_execution_authority(
            manager.clone(),
            probe,
            Arc::new(SessionExecutionAuthority::with_policy(
                manager.clone(),
                policy,
            )),
        )
    }

    #[test]
    fn budget_enforcement_is_persisted_effective_vs_requested_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("candidate");
        std::fs::create_dir_all(&root).unwrap();
        let (manager, sid) = manager_at(dir.path(), &root, "budget-profile");
        let service = service_with_policy(&manager, granted_policy());
        let Some(creation) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let view = &creation.view;
        let profile = ExecutionProfile::parse(&view.execution_profile).expect("profile JSON");

        // The REQUEST is exactly the policy's budgets; the EFFECTIVE report
        // is a separate, per-limit statement.
        assert_eq!(profile.budgets, TerminalBudgets::default());
        assert!(!profile.strict_budgets);
        let enforcement = profile
            .budget_enforcement
            .clone()
            .expect("the durable row records the effective enforcement");
        assert_eq!(enforcement.wall, LimitState::Enforced, "{enforcement:?}");
        for (name, state) in [
            ("cpu", enforcement.cpu),
            ("memory", enforcement.memory),
            ("processes", enforcement.processes),
        ] {
            assert_ne!(
                state,
                LimitState::NotRequested,
                "{name} was requested and must be reported as a real state: {enforcement:?}"
            );
        }
        assert!(
            view.execution_profile.contains("budgetEnforcement"),
            "{}",
            view.execution_profile
        );

        // The durable rows carry the byte-identical profile.
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        for record in handle.ledger_terminal_rows(None).unwrap() {
            assert_eq!(record.row.execution_profile, view.execution_profile);
        }

        // Reopen: a fresh service over the same durable store projects the
        // SAME requested + effective record (a durable row fact, never
        // daemon memory).
        let restarted = TerminalService::detached(manager.clone(), Arc::new(|_pid: u32| None));
        let views = restarted.list(&sid).unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].execution_profile, view.execution_profile);
        let rebuilt = ExecutionProfile::parse(&views[0].execution_profile).expect("profile JSON");
        assert_eq!(rebuilt.budgets, profile.budgets);
        assert_eq!(rebuilt.budget_enforcement, profile.budget_enforcement);
        let _ = service.kill(&sid, view.terminal_id.as_str(), "budget cleanup");
    }

    #[test]
    fn legacy_profile_json_parses_with_no_effective_budget_claim() {
        // A row written before enforcement existed has neither the strictness
        // flag nor an effective report. It must parse (the profile is
        // evidence) and must NOT fabricate an enforcement claim.
        let legacy = r#"{"sessionId":1,"taskId":1,"workspaceId":1,"agentId":null,
            "candidateRoot":"/tmp","cwd":"/tmp","capabilities":"*",
            "filesystem":"workspace","network":"none",
            "budgets":{"cpuMillis":1000,"memoryBytes":1024,"maxProcesses":2,"wallTimeMs":3000},
            "envNames":[],"externalCwdGranted":false}"#;
        let profile = ExecutionProfile::parse(legacy).expect("legacy profile must parse");
        assert!(profile.budget_enforcement.is_none());
        assert!(!profile.strict_budgets);
        assert_eq!(profile.budgets.wall_time_ms, 3000);
        assert_eq!(profile.budgets.max_processes, 2);
        assert!(
            profile.shell.is_empty(),
            "a legacy row never recorded a shell tag: no tag is fabricated"
        );
    }

    #[test]
    fn disabled_budgets_are_parity_not_requested_and_never_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("candidate");
        std::fs::create_dir_all(&root).unwrap();
        let (manager, sid) = manager_at(dir.path(), &root, "budget-disabled");
        // Even a STRICT profile with NO requested limit must spawn: there is
        // nothing to enforce, so nothing can be unenforceable. (The explicit
        // operator grant admits the PTY; this test is about budgets.)
        let policy = TerminalAuthorityPolicy {
            budgets: TerminalBudgets::disabled(),
            strict_budgets: true,
            ..granted_policy()
        };
        let service = service_with_policy(&manager, policy);
        let Some(creation) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let profile = ExecutionProfile::parse(&creation.view.execution_profile).expect("profile");
        assert_eq!(profile.budgets, TerminalBudgets::disabled());
        assert!(profile.strict_budgets);
        assert_eq!(
            profile.budget_enforcement,
            Some(BudgetEnforcement::not_requested()),
            "a disabled budget is NotRequested, never silently 'unlimited'"
        );
        // The lifecycle is exactly the pre-budget lifecycle.
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        assert_eq!(
            terminal_kinds(&handle),
            vec![TerminalEventKind::Created, TerminalEventKind::Running]
        );
        assert!(service
            .kill(&sid, creation.handle.terminal_id(), "disabled cleanup")
            .unwrap());
        assert_eq!(
            terminal_kinds(&handle),
            vec![
                TerminalEventKind::Created,
                TerminalEventKind::Running,
                TerminalEventKind::Killed
            ]
        );
    }

    #[test]
    fn strict_profile_refuses_typed_when_the_platform_cannot_enforce() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("candidate");
        std::fs::create_dir_all(&root).unwrap();
        let (manager, sid) = manager_at(dir.path(), &root, "budget-strict-refusal");
        let policy = TerminalAuthorityPolicy {
            strict_budgets: true,
            budget_platform: BudgetPlatform::all_unenforceable(),
            ..TerminalAuthorityPolicy::default()
        };

        // Authority level: the pre-spawn gate is a typed denial naming the
        // limits, and it happens before any PTY exists.
        let authority = SessionExecutionAuthority::with_policy(manager.clone(), policy.clone());
        let principal = principal_of(&manager, &sid);
        match authority.authorize_terminal_spawn(
            &principal,
            &sid,
            &spawn_request("/bin/sleep", &["30"]),
        ) {
            Err(ExecutionDenial::BudgetUnavailable { resource, reason }) => {
                assert!(resource.contains("cpu"), "{resource}");
                assert!(resource.contains("memory"), "{resource}");
                assert!(resource.contains("processes"), "{resource}");
                assert!(reason.contains("strict"), "{reason}");
            }
            other => panic!("a strict unsupported budget must be denied: {other:?}"),
        }

        // Service level: typed refusal, no live row and NOTHING journaled.
        let service = service_with_policy(&manager, policy);
        match service.spawn(&sid, &spawn_request("/bin/sleep", &["30"])) {
            Err(TerminalServiceError::Denied(message)) => {
                assert!(
                    message.contains("cpu") || message.contains("memory"),
                    "{message}"
                );
            }
            other => panic!("the strict spawn must be refused typed: {:?}", other.err()),
        }
        assert_eq!(service.live_rows(), 0);
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        assert!(
            handle.ledger_terminal_rows(None).unwrap().is_empty(),
            "a refused strict budget journals nothing"
        );
    }

    #[test]
    fn non_strict_unsupported_limits_spawn_and_record_the_typed_gap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("candidate");
        std::fs::create_dir_all(&root).unwrap();
        let (manager, sid) = manager_at(dir.path(), &root, "budget-unenforceable");
        let policy = TerminalAuthorityPolicy {
            strict_budgets: false,
            budget_platform: BudgetPlatform::all_unenforceable(),
            ..granted_policy()
        };
        let service = service_with_policy(&manager, policy);
        let Some(creation) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let profile = ExecutionProfile::parse(&creation.view.execution_profile).expect("profile");
        let enforcement = profile.budget_enforcement.expect("effective report");
        // Requested limits are never reported as NotRequested (that would be
        // a lie) and the wall deadline is real everywhere we can kill a tree.
        assert_ne!(enforcement.cpu, LimitState::NotRequested);
        assert_ne!(enforcement.memory, LimitState::NotRequested);
        assert_ne!(enforcement.processes, LimitState::NotRequested);
        assert_eq!(enforcement.wall, LimitState::Enforced);
        assert!(
            !enforcement.details.is_empty(),
            "an unsupported/degraded limit carries its typed reason: {enforcement:?}"
        );
        assert!(!profile.strict_budgets);
        let _ = service.kill(&sid, creation.handle.terminal_id(), "cleanup");
    }

    #[test]
    fn wall_deadline_kills_the_tree_through_the_authority_and_journals_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("candidate");
        std::fs::create_dir_all(&root).unwrap();
        let (manager, sid) = manager_at(dir.path(), &root, "budget-wall");
        // Only the wall limit is requested (and enforced everywhere), so a
        // STRICT profile is satisfiable and the deadline is the whole story.
        let policy = TerminalAuthorityPolicy {
            budgets: TerminalBudgets {
                wall_time_ms: 300,
                ..TerminalBudgets::disabled()
            },
            strict_budgets: true,
            ..granted_policy()
        };
        let service = service_with_policy(&manager, policy);
        // A LEADER WITH DESCENDANTS: the deadline must take the whole
        // guardian-owned tree, not just the direct child.
        let Some(creation) = spawn_or_skip(
            &service,
            &sid,
            &spawn_request("/bin/sh", &["-c", "sleep 30 & sleep 30 & wait"]),
        ) else {
            return;
        };
        let terminal_id = creation.handle.terminal_id().to_string();
        let leader_pid = creation.view.pid;
        let profile = ExecutionProfile::parse(&creation.view.execution_profile).expect("profile");
        assert_eq!(
            profile.budget_enforcement.expect("report").wall,
            LimitState::Enforced
        );
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();

        // The watchdog kills the WHOLE guardian-owned tree at the deadline:
        // the row transitions to killed exactly once, with the reason.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let rows = service.list(&sid).unwrap();
            if rows[0].state_tag() == "killed" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the wall deadline never killed the terminal: {rows:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!creation.handle.is_alive());
        let rows = service.list(&sid).unwrap();
        assert!(rows[0].detail.contains("wall"), "{}", rows[0].detail);
        assert_eq!(
            terminal_kinds(&handle),
            vec![
                TerminalEventKind::Created,
                TerminalEventKind::Running,
                TerminalEventKind::Killed
            ],
            "exactly one kill transition for the wall deadline"
        );
        // No orphans: the whole process group is gone.
        let orphan_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while faktor_pty::guardian::group_exists(leader_pid)
            && std::time::Instant::now() < orphan_deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !faktor_pty::guardian::group_exists(leader_pid),
            "the wall deadline must leave no orphan in the process group of pid {leader_pid}"
        );
        // Idempotent: the deadline cannot kill twice.
        assert!(!service.kill(&sid, &terminal_id, "late kill").unwrap());
        assert_eq!(terminal_kinds(&handle).len(), 3);
    }

    #[test]
    fn a_finished_terminal_is_never_killed_late_by_its_cancelled_wall() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("candidate");
        std::fs::create_dir_all(&root).unwrap();
        let (manager, sid) = manager_at(dir.path(), &root, "budget-wall-cancel");
        let policy = TerminalAuthorityPolicy {
            budgets: TerminalBudgets {
                wall_time_ms: 400,
                ..TreeBudgets::disabled()
            },
            ..granted_policy()
        };
        let service = service_with_policy(&manager, policy);
        let Some(creation) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let terminal_id = creation.handle.terminal_id().to_string();
        // End the terminal well before the deadline: the live row (and its
        // armed watchdog) is gone, so the deadline must not journal a second
        // transition (or signal anything) later.
        assert!(service.kill(&sid, &terminal_id, "early kill").unwrap());
        std::thread::sleep(std::time::Duration::from_millis(900));
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        let rows = handle.ledger_terminal_rows(None).unwrap();
        assert_eq!(rows.len(), 3, "no late wall transition: {rows:?}");
        assert_eq!(rows[2].detail, "early kill");
    }

    #[test]
    fn create_running_kill_round_trip_journals_one_sequence_and_kills_the_tree() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-roundtrip");
        let (probe, _map) = recording_probe();
        let service = service_over(&manager, probe, granted_policy());
        let Some(creation) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let terminal_id = creation.handle.terminal_id().to_string();
        assert!(creation.handle.pid() > 0);
        assert!(!creation.handle.ownership_id().is_empty());
        assert!(creation.view.start_time_ms > 0, "identity recorded");
        assert_eq!(creation.view.state_tag(), "running");

        // The durable row stream is exactly created -> running.
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        assert_eq!(
            terminal_kinds(&handle),
            vec![TerminalEventKind::Created, TerminalEventKind::Running]
        );

        // Scope-enforced listing carries the full ownership.
        let rows = service.list(&sid).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].terminal_id, terminal_id);
        assert_eq!(rows[0].state_tag(), "running");
        assert!(rows[0].alive);
        assert_eq!(rows[0].task_id.raw(), 1);
        assert!(rows[0].agent_id.is_none());

        // Kill routes through the pty authority and journals once.
        assert!(service.kill(&sid, &terminal_id, "roundtrip kill").unwrap());
        assert!(!creation.handle.is_alive());
        assert_eq!(
            terminal_kinds(&handle),
            vec![
                TerminalEventKind::Created,
                TerminalEventKind::Running,
                TerminalEventKind::Killed
            ]
        );
        let rows = service.list(&sid).unwrap();
        assert_eq!(rows[0].state_tag(), "killed");
        assert!(!rows[0].alive);

        // Idempotent: a second kill journals NOTHING.
        assert!(!service.kill(&sid, &terminal_id, "again").unwrap());
        assert_eq!(terminal_kinds(&handle).len(), 3);
        // The handle path journals exactly once too.
        creation.handle.kill().unwrap();
        assert_eq!(terminal_kinds(&handle).len(), 3);
    }

    /// A one-shot flag + condvar gate.
    #[cfg(unix)]
    type Gate = Arc<(Mutex<bool>, std::sync::Condvar)>;

    /// A `(entered, release)` condvar pair: force the spawn→registration
    /// window open and hold it until the test has attacked every reaping
    /// surface.
    #[cfg(unix)]
    fn interleaving_gate() -> (Gate, Gate) {
        (
            Arc::new((Mutex::new(false), std::sync::Condvar::new())),
            Arc::new((Mutex::new(false), std::sync::Condvar::new())),
        )
    }

    #[cfg(unix)]
    fn wait_flag(flag: &Gate, what: &str) {
        let (lock, cv) = &**flag;
        let mut seen = lock.lock().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !*seen {
            let (guard, _timeout) = cv
                .wait_timeout(seen, std::time::Duration::from_millis(20))
                .unwrap();
            seen = guard;
            assert!(
                std::time::Instant::now() < deadline,
                "the spawn hook never {what}"
            );
        }
    }

    /// ADVERSARIAL INTERLEAVING: hold a create open between `Pty::spawn` and
    /// registration and prove that the child is owned from birth — the sweep,
    /// the recovery scan (session-local and global) and the listing can never
    /// observe or reap it as an unowned/orphan process; after release the
    /// create is owned by the live row exactly once, and only an explicit
    /// kill ends it.
    #[test]
    #[cfg(unix)]
    fn spawn_window_is_owned_from_birth_and_survives_every_reaper() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-birth-ownership");
        let (probe, _map) = recording_probe();
        let (entered, release) = interleaving_gate();
        let entered_hook = Arc::clone(&entered);
        let release_hook = Arc::clone(&release);
        let hook: SpawnHook = Arc::new(move |terminal_id: &str, pid: u32| {
            assert!(!terminal_id.is_empty(), "the hook sees the owned id");
            assert!(pid > 0, "the hook sees the real child pid");
            let (lock, cv) = &*entered_hook;
            *lock.lock().unwrap() = true;
            cv.notify_all();
            let (lock, cv) = &*release_hook;
            let mut released = lock.lock().unwrap();
            while !*released {
                let (guard, _) = cv
                    .wait_timeout(released, std::time::Duration::from_secs(10))
                    .unwrap();
                released = guard;
            }
            Ok(())
        });
        let service = hook_service(&manager, probe, hook);
        let spawn_service = Arc::clone(&service);
        let spawn_sid = sid.clone();
        let spawning = std::thread::spawn(move || {
            spawn_service.spawn(&spawn_sid, &spawn_request("/bin/sleep", &["30"]))
        });

        wait_flag(&entered, "entered");
        let (terminal_id, pid) = {
            let pending = service.lock_pending();
            assert_eq!(
                pending.len(),
                1,
                "the child must be pending-owned before any journal/registration"
            );
            let (id, pty) = pending.iter().next().unwrap();
            let observed = (id.clone(), pty.lock().unwrap().pid());
            observed
        };
        assert!(pid > 0 && service.owns_child(&terminal_id));

        // Every internal reaping surface runs while the create is in flight.
        service.sweep_live();
        service.recover_session(&sid).unwrap();
        service.recover_all().unwrap();
        let rows = service.list(&sid).unwrap();
        assert!(
            rows.iter().all(|row| row.terminal_id != terminal_id),
            "the in-flight child has no durable row yet: {rows:?}"
        );
        assert!(
            faktor_pty::guardian::group_exists(pid),
            "no sweep/recovery path may reap a child owned from birth (pid {pid})"
        );
        assert!(
            service.owns_child(&terminal_id),
            "the pending ownership survives every scan"
        );

        {
            let (lock, cv) = &*release;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
        let creation = spawning
            .join()
            .unwrap()
            .expect("the create succeeds after the window");
        assert_eq!(creation.handle.pid(), pid, "the create returns the child");
        assert!(creation.handle.is_alive(), "the returned child is alive");
        assert!(
            service.lock_pending().is_empty(),
            "the successful create promotes out of the pending map"
        );
        assert!(service.owns_child(&terminal_id));
        let rows = service.session_rows(SessionId::new(sid.parse().unwrap()));
        assert_eq!(rows.len(), 1, "exactly one session-owned row: {rows:?}");
        assert_eq!(rows[0].1.pid, pid);
        assert!(service.kill(&sid, &terminal_id, "cleanup").unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while faktor_pty::guardian::group_exists(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "the explicit kill must take the whole group (pid {pid})"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// A failure injected inside the birth window (before journaling) kills
    /// the just-spawned child and leaves no owner, no live row and no
    /// durable row — no create failure may leak a process.
    #[test]
    #[cfg(unix)]
    fn spawn_window_failure_kills_the_child_and_leaves_no_orphan() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-birth-refusal");
        let (probe, _map) = recording_probe();
        let seen = Arc::new(Mutex::new(None::<u32>));
        let seen_hook = Arc::clone(&seen);
        let hook: SpawnHook = Arc::new(move |_terminal_id: &str, pid: u32| {
            *seen_hook.lock().unwrap() = Some(pid);
            Err("injected refusal inside the birth window".into())
        });
        let service = hook_service(&manager, probe, hook);
        let error = match service.spawn(&sid, &spawn_request("/bin/sleep", &["30"])) {
            Ok(_) => panic!("the injected refusal must fail the create"),
            Err(error) => error,
        };
        assert!(
            matches!(error, TerminalServiceError::Refused(ref message) if message.contains("injected refusal")),
            "the refusal is typed and names the injection: {error:?}"
        );
        let pid = seen.lock().unwrap().expect("the hook observed the child");
        assert!(service.lock_pending().is_empty(), "no pending owner");
        assert_eq!(service.live_rows(), 0, "no live row");
        assert!(service
            .session_rows(SessionId::new(sid.parse().unwrap()))
            .is_empty());
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        assert!(
            handle.ledger_terminal_rows(None).unwrap().is_empty(),
            "no durable row"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while faktor_pty::guardian::group_exists(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "the refused child must be killed and reaped (pid {pid})"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Load cycles: concurrent create/kill rounds on ONE service. Every
    /// returned child is owned+registered before the create returns, every
    /// explicit kill leaves no live row and no group member, and the
    /// service's ownership maps drain to empty.
    #[test]
    #[cfg(unix)]
    fn concurrent_create_kill_cycles_never_expose_unowned_children() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-cycle-load");
        let (probe, _map) = recording_probe();
        let service = service_over(&manager, probe, granted_policy());
        let mut workers = Vec::new();
        for worker in 0..3u32 {
            let service = Arc::clone(&service);
            let sid = sid.clone();
            workers.push(std::thread::spawn(move || {
                for cycle in 0..8u32 {
                    let request = spawn_request("/bin/sleep", &["30"]);
                    let creation = service
                        .spawn(&sid, &request)
                        .unwrap_or_else(|e| panic!("create {worker}/{cycle} failed: {e:?}"));
                    let terminal_id = creation.handle.terminal_id().to_string();
                    let pid = creation.handle.pid();
                    assert!(pid > 0);
                    assert!(
                        service.owns_child(&terminal_id),
                        "a returned child is owned (worker {worker}, cycle {cycle})"
                    );
                    assert!(
                        creation.handle.is_alive(),
                        "a returned child is alive (worker {worker}, cycle {cycle})"
                    );
                    let rows = service.session_rows(SessionId::new(sid.parse().unwrap()));
                    assert!(
                        rows.iter()
                            .any(|(id, row)| *id == terminal_id && row.pid == pid),
                        "the returned child is session-registered (worker {worker})"
                    );
                    assert!(service.kill(&sid, &terminal_id, "cycle").unwrap());
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    while faktor_pty::guardian::group_exists(pid) {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "cycle kill leaves no group member (pid {pid})"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    assert!(
                        !service.owns_child(&terminal_id),
                        "owned until killed, then not"
                    );
                }
            }));
        }
        for worker in workers {
            worker.join().expect("cycle worker");
        }
        assert!(service.lock_pending().is_empty(), "pending drained");
        assert_eq!(service.live_rows(), 0, "every cycle terminal is retired");
    }

    #[test]
    fn foreign_scope_is_denied_without_touching_the_owned_terminal() {
        let (_dir, manager) = manager();
        let a = session(&manager, "scope-a");
        let b = session(&manager, "scope-b");
        let (probe, _map) = recording_probe();
        let service = service_over(&manager, probe, granted_policy());
        let Some(creation) = spawn_or_skip(&service, &a, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let terminal_id = creation.handle.terminal_id().to_string();

        // B's listing never contains A's terminal.
        assert!(service.list(&b).unwrap().is_empty());

        // Every foreign operation is denied typed AND leaves the row alone.
        for denied in [
            service.input(&b, &terminal_id, b"x"),
            service.resize(&b, &terminal_id, 10, 10),
        ] {
            assert!(denied.is_err(), "foreign scope must be denied");
        }
        assert!(
            service.kill(&b, &terminal_id, "hostile").is_err(),
            "foreign kill must be denied"
        );
        let rows = service.list(&a).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state_tag(), "running");
        assert!(rows[0].alive, "the owned terminal is untouched");
        let a_handle = manager
            .get_session(SessionId::new(a.parse().unwrap()))
            .unwrap()
            .unwrap();
        assert_eq!(
            terminal_kinds(&a_handle),
            vec![TerminalEventKind::Created, TerminalEventKind::Running]
        );
        let _ = service.kill(&a, &terminal_id, "cleanup");
    }

    #[test]
    fn restart_marks_unreachable_terminals_lost_and_refuses_io_typed() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-restart");
        let (probe, identities) = recording_probe();
        let service = service_over(&manager, probe, granted_policy());
        let Some(creation) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let terminal_id = creation.handle.terminal_id().to_string();
        let pid = creation.view.pid;
        let identity = identities.lock().unwrap().get(&pid).copied().unwrap();

        // A restarted daemon: a fresh service over the same durable store
        // whose probe still observes the recorded identity (the process may
        // even still run) but which owns NO live pty handle.
        let restarted = TerminalService::detached(
            manager.clone(),
            Arc::new(move |probed: u32| (probed == pid).then_some(identity)),
        );
        let report = restarted.recover_all().unwrap();
        assert_eq!(report.lost, vec![terminal_id.clone()]);
        let rows = restarted.list(&sid).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state_tag(), "lost");
        assert!(!rows[0].alive);
        assert!(rows[0].detail.contains("unreachable"), "{}", rows[0].detail);

        // I/O is refused typed (Lost), never pid-signalled.
        match restarted.input(&sid, &terminal_id, b"x") {
            Err(TerminalServiceError::Lost { .. }) => {}
            other => panic!("expected typed Lost refusal, got {other:?}"),
        }
        match restarted.resize(&sid, &terminal_id, 10, 10) {
            Err(TerminalServiceError::Lost { .. }) => {}
            other => panic!("expected typed Lost refusal, got {other:?}"),
        }
        match restarted.kill(&sid, &terminal_id, "hostile") {
            Err(TerminalServiceError::Lost { .. }) => {}
            other => panic!("expected typed Lost refusal, got {other:?}"),
        }

        // The stale row is reconciled exactly once, typed.
        let view = restarted
            .reconcile(
                &sid,
                &terminal_id,
                TerminalReconcileDisposition::Collected,
                None,
            )
            .unwrap();
        assert_eq!(view.state_tag(), "reconciled");
        assert!(matches!(
            restarted.reconcile(
                &sid,
                &terminal_id,
                TerminalReconcileDisposition::Killed,
                None
            ),
            Err(TerminalServiceError::State { .. })
        ));
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        assert_eq!(
            terminal_kinds(&handle),
            vec![
                TerminalEventKind::Created,
                TerminalEventKind::Running,
                TerminalEventKind::Lost,
                TerminalEventKind::Reconciled
            ]
        );
    }

    #[test]
    fn recycled_pid_identity_mismatch_is_refused_and_never_adopted() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-recycled");
        let (probe, identities) = recording_probe();
        let service = service_over(&manager, probe, granted_policy());
        let Some(creation) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let terminal_id = creation.handle.terminal_id().to_string();
        let pid = creation.view.pid;
        let recorded = identities.lock().unwrap().get(&pid).copied().unwrap();

        // The restarted daemon probes the same pid with a DIFFERENT start
        // marker: the pid was recycled by an unrelated process.
        let recycled_marker = recorded + 1_000_000;
        let restarted = TerminalService::detached(
            manager.clone(),
            Arc::new(move |probed: u32| (probed == pid).then_some(recycled_marker)),
        );
        let report = restarted.recover_all().unwrap();
        assert_eq!(report.lost, vec![terminal_id.clone()]);
        let rows = restarted.list(&sid).unwrap();
        assert_eq!(rows[0].state_tag(), "lost");
        assert!(rows[0].detail.contains("recycled"), "{}", rows[0].detail);

        let before = rows.len();
        let refused = restarted.reconcile(
            &sid,
            &terminal_id,
            TerminalReconcileDisposition::Killed,
            Some(ProcessIdentity {
                pid,
                start_time_ms: recycled_marker,
            }),
        );
        match refused {
            Err(TerminalServiceError::IdentityMismatch { .. }) => {}
            other => panic!("expected IdentityMismatch refusal, got {other:?}"),
        }
        // Nothing was journaled: the row is still Lost.
        assert_eq!(restarted.list(&sid).unwrap().len(), before);
        assert_eq!(restarted.list(&sid).unwrap()[0].state_tag(), "lost");
    }

    #[test]
    fn concurrent_kill_race_journals_exactly_one_terminal_sequence() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-race");
        let (probe, _map) = recording_probe();
        let service = service_over(&manager, probe, granted_policy());
        let Some(creation) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let terminal_id = creation.handle.terminal_id().to_string();
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();

        let rounds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let shared_handle = Arc::new(creation.handle);
        let mut threads = Vec::new();
        for index in 0..8 {
            let service = Arc::clone(&service);
            let sid = sid.clone();
            let terminal_id = terminal_id.clone();
            let rounds = Arc::clone(&rounds);
            let shared_handle = Arc::clone(&shared_handle);
            threads.push(std::thread::spawn(move || {
                // Both the service path and the handle path race the same
                // row: exactly one terminal transition may survive.
                if index % 2 == 0 {
                    let _ = service.kill(&sid, &terminal_id, "race");
                } else {
                    let _ = shared_handle.kill();
                }
                rounds.fetch_add(1, Ordering::SeqCst);
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(rounds.load(Ordering::SeqCst), 8);
        assert_eq!(
            terminal_kinds(&handle),
            vec![
                TerminalEventKind::Created,
                TerminalEventKind::Running,
                TerminalEventKind::Killed
            ],
            "exactly one terminal transition survives the race"
        );
    }

    #[test]
    fn restart_equality_no_in_memory_cache_can_diverge_from_the_durable_rows() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-equality");
        let (probe, _map) = recording_probe();
        let service = service_over(&manager, probe, granted_policy());
        let Some(first) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let Some(second) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let first_id = first.handle.terminal_id().to_string();
        let second_id = second.handle.terminal_id().to_string();
        assert!(service.kill(&sid, &first_id, "equality").unwrap());

        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        let raw_fold = handle.ledger_terminal_rows(None).unwrap();
        let live_views = service.list(&sid).unwrap();
        assert_eq!(live_views.len(), 2);
        // The cache equals the durable fold, row by row.
        for view in &live_views {
            let last = raw_fold
                .iter()
                .rfind(|record| record.row.terminal_id == view.terminal_id)
                .expect("durable row");
            let first = raw_fold
                .iter()
                .find(|record| record.row.terminal_id == view.terminal_id)
                .expect("durable row");
            assert_eq!(view.state, last.kind);
            assert_eq!(view.pid, last.row.pid);
            assert_eq!(view.start_time_ms, last.row.start_time_ms);
            assert_eq!(view.updated_ms, last.row.at_ms);
            assert_eq!(view.spawned_ms, first.row.at_ms);
        }

        // A fresh service (restart) rebuilds the same fold; only the
        // running -> lost recovery transition may differ.
        let restarted = TerminalService::detached(manager.clone(), Arc::new(|_pid: u32| None));
        restarted.recover_all().unwrap();
        let after = restarted.list(&sid).unwrap();
        assert_eq!(after.len(), 2);
        for view in &after {
            if view.terminal_id == second_id {
                assert_eq!(view.state_tag(), "lost");
            } else {
                assert_eq!(view.state_tag(), "killed");
            }
        }
        // Idempotent: a second recovery scan journals nothing new.
        let raw_after = handle.ledger_terminal_rows(None).unwrap().len();
        restarted.recover_all().unwrap();
        assert_eq!(handle.ledger_terminal_rows(None).unwrap().len(), raw_after);
        assert_eq!(restarted.list(&sid).unwrap(), after);
    }

    #[test]
    fn reconciliation_of_a_live_row_is_refused_typed() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-reconcile-live");
        let (probe, _map) = recording_probe();
        let service = service_over(&manager, probe, granted_policy());
        let Some(creation) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let terminal_id = creation.handle.terminal_id().to_string();
        let refused = service.reconcile(
            &sid,
            &terminal_id,
            TerminalReconcileDisposition::Killed,
            None,
        );
        assert!(matches!(refused, Err(TerminalServiceError::State { .. })));
        let _ = service.kill(&sid, &terminal_id, "cleanup");
    }

    #[test]
    fn unverified_identity_is_never_adopted_by_pid_alone() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-unverified");
        // The spawn probe observes nothing: the durable row records no
        // verifiable identity.
        let service = service_over(&manager, Arc::new(|_pid: u32| None), granted_policy());
        let Some(creation) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        assert_eq!(creation.view.start_time_ms, 0);
        // A restarted daemon whose probe WOULD observe the pid must still
        // refuse to adopt the row: no recorded identity = unverifiable.
        let seen = Arc::new(AtomicBool::new(false));
        let seen2 = Arc::clone(&seen);
        let pid = creation.view.pid;
        let restarted = TerminalService::detached(
            manager.clone(),
            Arc::new(move |probed: u32| {
                if probed == pid {
                    seen2.store(true, Ordering::SeqCst);
                    Some(42)
                } else {
                    None
                }
            }),
        );
        restarted.recover_all().unwrap();
        assert!(seen.load(Ordering::SeqCst), "the pid was probed");
        let rows = restarted.list(&sid).unwrap();
        assert_eq!(rows[0].state_tag(), "lost");
        assert!(rows[0].detail.contains("unverified"), "{}", rows[0].detail);
    }

    #[test]
    fn exited_terminal_is_swept_and_journals_exited_exactly_once() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-exit");
        let (probe, _map) = recording_probe();
        let service = service_over(&manager, probe, granted_policy());
        let Some(creation) =
            spawn_or_skip(&service, &sid, &spawn_request("/bin/sh", &["-c", "exit 0"]))
        else {
            return;
        };
        let terminal_id = creation.handle.terminal_id().to_string();
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let rows = service.list(&sid).unwrap();
            if rows[0].state_tag() == "exited" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the exited child was never swept: {rows:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!creation.handle.is_alive());
        assert_eq!(
            terminal_kinds(&handle),
            vec![
                TerminalEventKind::Created,
                TerminalEventKind::Running,
                TerminalEventKind::Exited
            ],
            "exactly one exited row"
        );
        // A second sweep journals nothing.
        let _ = service.list(&sid).unwrap();
        assert_eq!(terminal_kinds(&handle).len(), 3);
        // I/O on an exited row is a typed state refusal.
        match service.input(&sid, &terminal_id, b"x") {
            Err(TerminalServiceError::State { .. }) => {}
            other => panic!("expected typed state refusal, got {other:?}"),
        }
        // An exited row is already terminal: a kill is an idempotent no-op.
        assert!(!service.kill(&sid, &terminal_id, "late kill").unwrap());
        assert_eq!(terminal_kinds(&handle).len(), 3);
    }

    #[test]
    fn poisoned_service_locks_recover_on_next_authority_op() {
        // The live map, the pending-ownership map, the inflight set, the
        // per-session fold cache and the recovery serializer are all DERIVED
        // from the durable ledger rows — no cross-entry invariant can be left
        // broken by a panicking holder. Poison every one of them and prove
        // the next authority operation recovers and serves instead of
        // panicking (no panic cascade).
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-poison");
        let (probe, _map) = recording_probe();
        let service = TerminalService::detached(manager, probe);

        let poisoner = {
            let service = Arc::clone(&service);
            std::thread::spawn(move || {
                let _live = service.live.lock().unwrap();
                let _pending = service.pending.lock().unwrap();
                let _inflight = service.inflight.lock().unwrap();
                let _index = service.index.lock().unwrap();
                let _recovery = service.recovery_lock.lock().unwrap();
                panic!("poison every terminal-service lock");
            })
        };
        assert!(poisoner.join().is_err(), "the holder must unwind");
        assert!(service.live.is_poisoned());
        assert!(service.pending.is_poisoned());
        assert!(service.inflight.is_poisoned());
        assert!(service.index.is_poisoned());
        assert!(service.recovery_lock.is_poisoned());

        // list() touches the recovery serializer + index + live map; the
        // durable rows stay the authority and the answer must be served.
        let rows = service.list(&sid).unwrap();
        assert!(rows.is_empty(), "{rows:?}");
        assert!(service
            .session_rows(SessionId::new(sid.parse().unwrap()))
            .is_empty());
        assert_eq!(service.live_rows(), 0);
        assert!(!service.owns_child("unowned-terminal"));
    }

    /// A hand-corrupted durable terminal row whose id fields are zero (a
    /// value the typed appenders refuse, so it can only come from a decoded
    /// hostile JSON payload) must surface as a typed refusal naming the
    /// terminal and field — never `Id::new(0)` panicking the listing or
    /// reconcile task — and the authority must stay usable with valid rows.
    #[test]
    fn corrupt_durable_terminal_ids_refuse_typed_and_authority_stays_usable() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-corrupt-id");
        let (probe, _map) = recording_probe();
        let service = TerminalService::detached(manager, probe);
        let handle = service.session_handle(&sid).unwrap();
        let base = TerminalDurableRow {
            terminal_id: "term-corrupt".into(),
            session_id: handle.id().raw(),
            task_id: 1,
            agent_id: None,
            operation_id: 9,
            pid: 4242,
            start_time_ms: 0,
            at_ms: 10,
            execution_profile: String::new(),
        };
        let terminal = |row: TerminalDurableRow| DurableTerminal {
            row,
            state: TerminalEventKind::Created,
            detail: String::new(),
            exit_code: None,
            seq: 1,
            spawned_ms: 10,
            updated_ms: 10,
        };

        // Valid rows project exactly.
        let view = terminal(base.clone()).view(None).unwrap();
        assert_eq!(view.session_id, handle.id());
        assert_eq!(view.task_id, TaskId::new(1));
        assert_eq!(view.operation_id, OpId::new(9));

        for (field, corrupt) in [
            (
                "session_id",
                TerminalDurableRow {
                    session_id: 0,
                    ..base.clone()
                },
            ),
            (
                "task_id",
                TerminalDurableRow {
                    task_id: 0,
                    ..base.clone()
                },
            ),
            (
                "operation_id",
                TerminalDurableRow {
                    operation_id: 0,
                    ..base.clone()
                },
            ),
        ] {
            match terminal(corrupt).view(None) {
                Err(TerminalServiceError::Refused(message)) => assert!(
                    message.contains(field) && message.contains("corrupt"),
                    "the refusal must name {field} and corruption: {message}"
                ),
                other => panic!("corrupt {field} must refuse typed, got {other:?}"),
            }
        }

        // The authority stays usable: a valid durable row still lists with
        // its decoded ids after the hostile projections were refused.
        handle.ledger_terminal_created(&base).unwrap();
        let rows = service.list(&sid).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].terminal_id, "term-corrupt");
        assert_eq!(rows[0].operation_id, OpId::new(9));
        assert_eq!(rows[0].session_id, handle.id());
        assert_eq!(rows[0].task_id, TaskId::new(1));

        // The durable JSON path: corrupting the PERSISTED payload's
        // operation_id to 0 (only reachable by hand-corrupting the DB) is
        // refused typed by `list` — never a panic, never a silently skipped
        // row. Repairing the row restores the authority.
        service
            .session
            .store()
            .sql_execute(&format!(
                "UPDATE ledger_entry \
                 SET payload = replace(payload, '\"operation_id\":9', '\"operation_id\":0') \
                 WHERE session_id = {} AND entry_type = 'terminal_created'",
                handle.id().raw()
            ))
            .unwrap();
        match service.list(&sid) {
            Err(TerminalServiceError::Refused(message)) => assert!(
                message.contains("operation_id"),
                "the durable corruption refusal must name operation_id: {message}"
            ),
            other => panic!("durable operation_id 0 must refuse typed, got {other:?}"),
        }
        service
            .session
            .store()
            .sql_execute(&format!(
                "UPDATE ledger_entry \
                 SET payload = replace(payload, '\"operation_id\":0', '\"operation_id\":9') \
                 WHERE session_id = {} AND entry_type = 'terminal_created'",
                handle.id().raw()
            ))
            .unwrap();
        let repaired = service.list(&sid).unwrap();
        assert_eq!(repaired.len(), 1, "{repaired:?}");
        assert_eq!(repaired[0].operation_id, OpId::new(9));
    }
}
