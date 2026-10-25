// TEST-SEPARATION: oracle=mutation isolation=read-only-artifacts
#![cfg(all(feature = "git", feature = "lsp"))]
//! Mutation witnesses for impact semantic-process ownership.
//!
//! The regression this pins: LSP/git children were spawned under
//! `ProcessOwner::Session(SessionId::new(1))`, so ending user session 1
//! killed semantic subprocesses shared by every session. Production spawn
//! sites must derive ownership from the caller-supplied REAL workspace (or
//! the standalone daemon host), never fabricate a session.

fn git_is_workspace_owned(source: &str) -> bool {
    source.contains("pub fn new(supervisor: Arc<ProcessSupervisor>, owner: ImpactProcessOwner)")
        && source.contains("owner: owner.process_owner(),")
        && !source.contains("owner: ProcessOwner::Session(")
}

fn lsp_is_workspace_owned(source: &str) -> bool {
    source.contains("owner: ImpactProcessOwner,")
        && source.contains("owner: owner.process_owner(),")
        && source.contains("LspSession::start(root, options, supervisor.clone(), owner)")
        && !source.contains("owner: ProcessOwner::Session(")
}

#[test]
fn impact_semantic_spawn_owners_are_workspace_scoped_and_mutation_sensitive() {
    const GIT: &str = include_str!("../src/git.rs");
    const LSP: &str = include_str!("../src/lsp.rs");
    assert!(
        git_is_workspace_owned(GIT),
        "git children must use the caller-supplied workspace/system owner"
    );
    assert!(
        lsp_is_workspace_owned(LSP),
        "the LSP child must use the caller-supplied workspace/system owner"
    );

    let reverted_git = GIT.replace(
        "owner: owner.process_owner(),",
        "owner: ProcessOwner::Session(SessionId::new(1)),",
    );
    assert!(
        !git_is_workspace_owned(&reverted_git),
        "witness must detect the fabricated session-1 git owner"
    );
    let reverted_lsp = LSP.replace(
        "owner: owner.process_owner(),",
        "owner: ProcessOwner::Session(SessionId::new(1)),",
    );
    assert!(
        !lsp_is_workspace_owned(&reverted_lsp),
        "witness must detect the fabricated session-1 LSP owner"
    );
    let unthreaded_lsp = LSP.replace(
        "LspSession::start(root, options, supervisor.clone(), owner)",
        "LspSession::start(root, options, supervisor.clone(), ImpactProcessOwner::Daemon)",
    );
    assert!(
        !lsp_is_workspace_owned(&unthreaded_lsp),
        "witness must detect a dropped caller workspace id"
    );
}
