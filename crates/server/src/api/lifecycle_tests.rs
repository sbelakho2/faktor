use crate::api::tests::*;
use crate::api::*;

/// (a) Injected serve failure: the owner records UNAVAILABLE with the
/// typed stable code and cause, `is_alive` turns false, and a later
/// `shutdown` surfaces the SAME typed error instead of a clean stop (the
/// production serve future is never `.ok()`-discarded).
#[tokio::test]
async fn unexpected_native_serve_error_is_recorded_typed_in_health() {
    let addr: std::net::SocketAddr = "127.0.0.1:8790".parse().unwrap();
    let handle = ServerHandle::failing_serve_for_test(
        addr,
        std::io::Error::other("injected accept failure"),
    );
    wait_until_server_dead(&handle).await;
    let status = handle.status();
    assert!(
        status.is_unavailable(),
        "unexpected death must be typed: {status:?}"
    );
    assert_eq!(status.code(), Some("server_serve_failed"));
    let line = status.health_line();
    assert!(line.contains("UNAVAILABLE"), "{line}");
    assert!(line.contains("server_serve_failed"), "{line}");
    assert!(line.contains("injected accept failure"), "{line}");
    assert!(line.contains(&addr.to_string()), "{line}");
    assert!(!handle.is_alive());
    let error = handle.shutdown().await.unwrap_err();
    assert_eq!(error.code(), "server_serve_failed");
    assert!(
        matches!(
            error,
            ServerShutdownError::Serve(ServerServeError::Serve { .. })
        ),
        "expected the typed serve error, got {error}"
    );
}

/// (a2) A serve future that completes CLEANLY without a shutdown request
/// is still an unexpected end: the socket stopped accepting while the
/// owner never asked it to, so the status must say UNAVAILABLE
/// (`server_serve_ended`), never `Stopped`.
#[tokio::test]
async fn unexpected_native_serve_end_is_not_a_clean_stop() {
    let addr: std::net::SocketAddr = "127.0.0.1:8791".parse().unwrap();
    let handle = ServerHandle::ended_serve_for_test(addr);
    let probe = handle.probe_for_test();
    wait_until_server_dead(&handle).await;
    let status = handle.status();
    assert_eq!(status.code(), Some("server_serve_ended"), "{status:?}");
    assert!(!handle.is_alive());
    assert!(
        matches!(
            probe.terminal(),
            Some(ServerStatus::Unavailable {
                code: "server_serve_ended",
                ..
            })
        ),
        "the recorded terminal must name the unexpected end: {:?}",
        probe.terminal()
    );
    // The typed join result is Ok (the future itself succeeded), but the
    // recorded health never lies: it stays Unavailable.
    handle.shutdown().await.unwrap();
}

/// (b) External abort (task killed out from under the handle): health
/// still names the death typed — never `Serving`, never a silent
/// `Stopped`.
#[tokio::test]
async fn externally_aborted_native_task_health_names_the_death() {
    let dir = tempfile::tempdir().unwrap();
    let handle = serve(test_deps(dir.path()), 0).await.unwrap();
    assert!(handle.is_alive());
    handle.abort_task_for_test();
    wait_until_server_dead(&handle).await;
    let status = handle.status();
    assert_eq!(status.code(), Some("server_task_died"), "{status:?}");
    assert!(status.health_line().contains("UNAVAILABLE"));
    assert!(!handle.is_alive());
    // The owner join surfaces the task death typed as well.
    let error = handle.shutdown().await.unwrap_err();
    assert_eq!(error.code(), "server_task_failed");
    assert!(
        matches!(
            error,
            ServerShutdownError::Serve(ServerServeError::Task { .. })
        ),
        "expected the typed task error, got {error}"
    );
}

/// (c) `shutdown().await` joins the owned task within the bounded window
/// and maps the clean graceful stop to `Ok(())`; the socket is released.
#[tokio::test]
async fn native_shutdown_joins_within_the_bound_and_maps_clean_stop_to_ok() {
    let dir = tempfile::tempdir().unwrap();
    let handle = serve(test_deps(dir.path()), 0).await.unwrap();
    let addr = handle.addr;
    assert!(handle.is_alive());
    let slack = Duration::from_secs(5);
    let started = std::time::Instant::now();
    let joined = tokio::time::timeout(SERVER_SHUTDOWN_BOUND + slack, handle.shutdown()).await;
    assert!(joined.is_ok(), "shutdown must join within the bound");
    joined.unwrap().unwrap();
    assert!(
        started.elapsed() < SERVER_SHUTDOWN_BOUND + slack,
        "the bounded join must not exceed the window"
    );
    // The owned task completed (not detached): the listener socket is
    // free.
    wait_until_rebindable(addr).await;
}

/// (d) Shutdown is idempotent: the one-shot request is honored exactly
/// once and every later request is a no-op; the join still returns `Ok`.
#[tokio::test]
async fn native_shutdown_request_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let handle = serve(test_deps(dir.path()), 0).await.unwrap();
    assert!(handle.request_shutdown(), "the first request wins");
    assert!(!handle.request_shutdown(), "a repeated request is a no-op");
    assert!(!handle.request_shutdown(), "still a no-op");
    handle.shutdown().await.unwrap();
}

/// (e) Drop aborts (never detaches): dropping the handle WITHOUT joining
/// releases the listener socket, fires the live-drop signal (the same
/// branch emits the error-level diagnostic), and leaves the task
/// aborted rather than gracefully joined. The one-shot signal lives in
/// the shared owner state, so a detached task would keep the socket
/// bound and this test would time out.
#[tokio::test]
async fn native_handle_drop_aborts_and_releases_the_socket() {
    let dir = tempfile::tempdir().unwrap();
    let handle = serve(test_deps(dir.path()), 0).await.unwrap();
    let addr = handle.addr;
    let probe = handle.probe_for_test();
    assert!(!probe.dropped_live(), "no signal while the handle is owned");
    drop(handle);
    assert!(
        probe.dropped_live(),
        "dropping a live handle must fire the loud live-drop signal"
    );
    wait_until_rebindable(addr).await;
}

/// (f) No detached task: the owner records the terminal stop and reports
/// complete after `shutdown` — the shared status says `Stopped` (never
/// `Serving`), the live-drop signal stays silent (the explicit
/// `shutdown().await` path never emits the diagnostic), and the listener
/// socket is released.
#[tokio::test]
async fn native_owner_reports_complete_after_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let handle = serve(test_deps(dir.path()), 0).await.unwrap();
    let addr = handle.addr;
    let probe = handle.probe_for_test();
    assert!(
        probe.terminal().is_none(),
        "no terminal snapshot while the task is alive"
    );
    assert_eq!(handle.status(), ServerStatus::Serving);
    assert_eq!(handle.status().health_line(), "native server: serving");
    handle.shutdown().await.unwrap();
    assert_eq!(
        probe.terminal(),
        Some(ServerStatus::Stopped),
        "the owner observed the task's clean completion"
    );
    assert!(
        !probe.dropped_live(),
        "the explicit shutdown path must never fire the live-drop diagnostic"
    );
    assert_eq!(
        ServerStatus::Stopped.health_line(),
        "native server: stopped"
    );
    wait_until_rebindable(addr).await;
}
