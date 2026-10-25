// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//
// Planted class (e): a scenario harness compiled into the daemon server
// source tree and run inside the live daemon process.
#[cfg(all(test, feature = "in-daemon-harness"))]
mod planted_harness_tests {
    #[test]
    fn planted_in_daemon_harness() {
        let _server = crate::serve_arc(crate::ServerDeps::default(), 0);
    }
}
