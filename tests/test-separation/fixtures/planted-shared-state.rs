// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//
// Planted class (c): the test mutates a process-global data directory that
// the operator's real agent also uses.
#[test]
fn planted_shared_state() {
    std::env::set_var("FAKTOR_DATA_DIR", "/home/operator/.local/share/faktor");
    let _ = std::env::var("FAKTOR_DATA_DIR");
    std::env::set_current_dir("/tmp/shared-agent-root").unwrap();
}
