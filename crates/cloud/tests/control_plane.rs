//! Adversarial integration tests of the control plane: tenant isolation
//! with no existence leak (denials are byte-identical to missing-row
//! denials), invitation lifecycle across a store restart, service-account
//! scoping (role ∩ scope) and exactly-once approval decisions.

use std::sync::Arc;

use faktor_cloud::ids::{ApprovalId, InvitationId, OrganizationId, SecretToken, UserId};
use faktor_cloud::model::{InvitationStatus, Membership, DEFAULT_INVITATION_TTL_MS};
use faktor_cloud::rbac::{authorize, Action, Resource, Role};
use faktor_cloud::store::{ControlPlaneStore, MemoryControlPlaneStore, SqliteControlPlaneStore};
use faktor_cloud::{
    AuthSession, AuthSessionId, Clock, ControlPlane, ControlPlaneError, ManualClock, TokenHash,
};

const T0: i64 = 1_700_000_000_000;

fn memory_plane() -> (ControlPlane, Arc<dyn ControlPlaneStore>) {
    let store: Arc<dyn ControlPlaneStore> = Arc::new(MemoryControlPlaneStore::new());
    (
        ControlPlane::new(store.clone(), Arc::new(ManualClock::new(T0))),
        store,
    )
}

fn bootstrap(
    cp: &ControlPlane,
    name: &str,
    email: &str,
    key: &str,
) -> faktor_cloud::BootstrapResult {
    cp.bootstrap_organization(name, email, "Owner", key)
        .expect("bootstrap")
}

/// One owner principal, its organization id and user id.
fn owner(
    cp: &ControlPlane,
    name: &str,
    email: &str,
    key: &str,
) -> (faktor_cloud::Principal, OrganizationId, UserId) {
    let boot = bootstrap(cp, name, email, key);
    let principal = cp
        .authenticate(boot.token.as_ref().expect("fresh token").expose())
        .expect("authenticate");
    (
        principal,
        boot.organization.id.clone(),
        boot.user.id.clone(),
    )
}

#[test]
fn foreign_organization_denials_are_indistinguishable_from_missing_rows() {
    let (cp, _store) = memory_plane();
    let (owner_a, _org_a, _) = owner(&cp, "Alpha", "owner@alpha.test", "a");
    let (owner_b, org_b, _) = owner(&cp, "Beta", "owner@beta.test", "b");
    let nonexistent = OrganizationId::try_new("org_00000000000000000000000000000000").unwrap();

    // Every organization-scoped entry point answers the foreign org exactly
    // like the nonexistent one: the same error value, a 404, and a message
    // that names neither the id nor the name.
    for attempted in [&org_b, &nonexistent] {
        let members = cp.members(&owner_a, attempted, None, 10).unwrap_err();
        let invite = cp
            .invite(&owner_a, attempted, "x@y.test", Role::Member, "k-invite")
            .unwrap_err();
        let approvals = cp
            .approvals(&owner_a, attempted, None, None, 10)
            .unwrap_err();
        let service_accounts = cp
            .service_accounts(&owner_a, attempted, None, 10)
            .unwrap_err();
        for error in [&members, &invite, &approvals, &service_accounts] {
            assert_eq!(
                error,
                &ControlPlaneError::NotFound("organization not found".into())
            );
            assert_eq!(error.http_status(), 404);
            let rendered = format!("{error:?} {error}");
            assert!(
                !rendered.contains("Beta"),
                "the denial leaks a name: {rendered}"
            );
            assert!(
                !rendered.contains(org_b.as_str()),
                "the denial leaks an id: {rendered}"
            );
        }
    }

    // The foreign org's own calls still work: the denial above is isolation,
    // not a broken tenant.
    let members_b = cp.members(&owner_b, &org_b, None, 10).expect("own org");
    assert_eq!(members_b.items.len(), 1);
}

#[test]
fn foreign_approval_and_invitation_ids_are_not_found() {
    let (cp, _store) = memory_plane();
    let (owner_a, _org_a, _) = owner(&cp, "Alpha", "owner@alpha.test", "a");
    let (owner_b, org_b, _) = owner(&cp, "Beta", "owner@beta.test", "b");

    let invitation_b = cp
        .invite(
            &owner_b,
            &org_b,
            "invitee@beta.test",
            Role::Member,
            "b-invite",
        )
        .expect("invite");
    let approval_b = cp
        .request_approval(
            &owner_b,
            &org_b,
            Action::SecretWrite,
            "secret:b",
            "r",
            "b-approval",
        )
        .expect("approval");

    // A foreign tenant can neither revoke the invitation nor decide the
    // approval, and learns nothing: both are plain NotFound.
    assert_eq!(
        cp.revoke_invitation(&owner_a, &invitation_b.invitation.id)
            .unwrap_err(),
        ControlPlaneError::NotFound("invitation not found".into())
    );
    assert_eq!(
        cp.decide_approval(&owner_a, &approval_b.id, true, "", "dec-foreign")
            .unwrap_err(),
        ControlPlaneError::NotFound("approval not found".into())
    );
    // An id that never existed answers identically.
    let missing_invitation = InvitationId::try_new("inv_00000000000000000000000000000000").unwrap();
    assert_eq!(
        cp.revoke_invitation(&owner_a, &missing_invitation)
            .unwrap_err(),
        cp.revoke_invitation(&owner_a, &invitation_b.invitation.id)
            .unwrap_err()
    );
    let missing_approval = ApprovalId::try_new("apr_00000000000000000000000000000000").unwrap();
    assert_eq!(
        cp.decide_approval(&owner_a, &missing_approval, true, "", "dec-missing")
            .unwrap_err(),
        cp.decide_approval(&owner_a, &approval_b.id, true, "", "dec-foreign-2")
            .unwrap_err()
    );
}

#[test]
fn invitation_lifecycle_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    let clock = Arc::new(ManualClock::new(T0));
    let store: Arc<dyn ControlPlaneStore> = Arc::new(SqliteControlPlaneStore::open(&path).unwrap());
    let org;
    let token: SecretToken;
    let invitee_id: UserId;
    {
        let cp = ControlPlane::new(store.clone(), clock.clone());
        let boot = bootstrap(&cp, "Acme", "owner@acme.test", "boot");
        org = boot.organization.id.clone();
        let owner = cp
            .authenticate(boot.token.as_ref().unwrap().expose())
            .unwrap();
        let issued = cp
            .invite(&owner, &org, "new@acme.test", Role::Admin, "invite-key")
            .expect("invite");
        assert_eq!(issued.invitation.status, InvitationStatus::Pending);
        // The idempotent replay returns the SAME invitation and no token.
        let replay = cp
            .invite(&owner, &org, "new@acme.test", Role::Admin, "invite-key")
            .expect("replay");
        assert_eq!(replay.invitation.id, issued.invitation.id);
        assert!(replay.token.is_none());
        token = issued.token.expect("fresh invitation token");
        let (invitee, _) = cp.create_user("new@acme.test", "New").unwrap();
        invitee_id = invitee.id;
        assert_eq!(
            cp.members(&owner, &org, None, 10).unwrap().items.len(),
            1,
            "the invitation is not a membership until accepted"
        );
    }
    // Restart: the invitation token still resolves and accepts exactly once.
    let cp = ControlPlane::new(store.clone(), clock.clone());
    let membership: Membership = cp
        .accept_invitation(token.expose(), &invitee_id, "accept-key")
        .unwrap();
    assert_eq!(membership.organization, org);
    assert_eq!(membership.role, Role::Admin);
    // A same-key retry replays the recorded membership (crash-retry safe)...
    let replayed = cp
        .accept_invitation(token.expose(), &invitee_id, "accept-key")
        .unwrap();
    assert_eq!(replayed.id, membership.id);
    // ...while the same token under a NEW key is a terminal conflict.
    assert!(matches!(
        cp.accept_invitation(token.expose(), &invitee_id, "accept-key-2")
            .unwrap_err(),
        ControlPlaneError::Conflict(_)
    ));

    // An invitation that expires before acceptance is refused, never
    // silently honored; a revoked one is refused too.
    let fresh = bootstrap(&cp, "Gamma", "owner@gamma.test", "boot-gamma");
    let owner = cp
        .authenticate(fresh.token.as_ref().unwrap().expose())
        .unwrap();
    let gamma = fresh.organization.id.clone();
    let expiring = cp
        .invite(
            &owner,
            &gamma,
            "late@gamma.test",
            Role::Member,
            "invite-late",
        )
        .expect("invite");
    let late_token = expiring.token.expect("token");
    clock.advance(DEFAULT_INVITATION_TTL_MS + 1);
    let (late_user, _) = cp.create_user("late@gamma.test", "Late").unwrap();
    assert!(matches!(
        cp.accept_invitation(late_token.expose(), &late_user.id, "late-accept")
            .unwrap_err(),
        ControlPlaneError::Conflict(_)
    ));
    let revocable = cp
        .invite(
            &owner,
            &gamma,
            "later@gamma.test",
            Role::Viewer,
            "invite-revoke",
        )
        .expect("invite");
    let revoked = cp
        .revoke_invitation(&owner, &revocable.invitation.id)
        .expect("revoke");
    assert_eq!(revoked.status, InvitationStatus::Revoked);
    assert!(matches!(
        cp.revoke_invitation(&owner, &revocable.invitation.id)
            .unwrap_err(),
        ControlPlaneError::Conflict(_)
    ));
}

#[test]
fn service_account_scoping_and_disabled_tokens() {
    let (cp, store) = memory_plane();
    let (owner, org, _) = owner(&cp, "Acme", "owner@acme.test", "boot");

    let issued = cp
        .create_service_account(
            &owner,
            &org,
            "ci",
            Role::Member,
            vec![Action::RepositoryRead, Action::RunCreate],
        )
        .expect("create service account");
    let token = issued.token.expect("token");
    let principal = cp.authenticate(token.expose()).expect("authenticate");
    assert!(principal.is_service_account());

    // In role and scope: allowed. Out of scope: refused. Out of role:
    // refused.
    assert!(authorize(
        &principal,
        &org,
        Resource::Repository,
        Action::RepositoryRead
    )
    .is_ok());
    assert!(authorize(&principal, &org, Resource::Run, Action::RunCreate).is_ok());
    assert!(authorize(
        &principal,
        &org,
        Resource::Repository,
        Action::RepositoryWrite
    )
    .is_err());
    assert!(authorize(&principal, &org, Resource::Member, Action::MemberRead).is_err());
    // The service account can never widen its own tenant.
    let foreign = OrganizationId::try_new("org_foreign").unwrap();
    assert!(authorize(
        &principal,
        &foreign,
        Resource::Repository,
        Action::RepositoryRead
    )
    .is_err());

    // Disabled accounts stop authenticating.
    let mut disabled = issued.account.clone();
    disabled.disabled = true;
    store.put_service_account(&disabled).unwrap();
    assert!(matches!(
        cp.authenticate(token.expose()).unwrap_err(),
        ControlPlaneError::Unauthorized(_)
    ));

    // Roles cannot be Owner, and an empty scope set is refused.
    assert!(cp
        .create_service_account(
            &owner,
            &org,
            "root",
            Role::Owner,
            vec![Action::RepositoryRead]
        )
        .is_err());
    assert!(cp
        .create_service_account(&owner, &org, "noscope", Role::Member, vec![])
        .is_err());
}

#[test]
fn sessions_and_approval_decisions_are_exactly_once_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    let clock = Arc::new(ManualClock::new(T0));
    let store: Arc<dyn ControlPlaneStore> = Arc::new(SqliteControlPlaneStore::open(&path).unwrap());
    let approval_id;
    let org;
    let owner_user;
    {
        let cp = ControlPlane::new(store.clone(), clock.clone());
        let boot = bootstrap(&cp, "Acme", "owner@acme.test", "boot");
        org = boot.organization.id.clone();
        owner_user = boot.user.id.clone();
        let owner = cp
            .authenticate(boot.token.as_ref().unwrap().expose())
            .unwrap();
        let approval = cp
            .request_approval(
                &owner,
                &org,
                Action::BillingWrite,
                "billing:acme",
                "upgrade",
                "k",
            )
            .unwrap();
        approval_id = approval.id.clone();
        let replay = cp
            .request_approval(
                &owner,
                &org,
                Action::BillingWrite,
                "billing:acme",
                "upgrade",
                "k",
            )
            .unwrap();
        assert_eq!(replay.id, approval.id);
    }
    // Restart: a second tenant's owner cannot even see the approval, and a
    // fresh session for the SAME owner (issued through the durable store,
    // since plaintext tokens are never stored) decides it exactly once.
    let cp = ControlPlane::new(store.clone(), clock.clone());
    let other = bootstrap(&cp, "Other", "owner@other.test", "boot-other");
    let other_owner = cp
        .authenticate(other.token.as_ref().unwrap().expose())
        .unwrap();
    assert!(matches!(
        cp.decide_approval(&other_owner, &approval_id, true, "", "dec-other")
            .unwrap_err(),
        ControlPlaneError::NotFound(_)
    ));

    let token = SecretToken::try_new("reopened-owner-session").unwrap();
    store
        .put_auth_session(&AuthSession {
            id: AuthSessionId::try_new("ses_reopened").unwrap(),
            organization: org.clone(),
            user: owner_user,
            token_hash: TokenHash::of(token.expose()),
            created_ms: clock.now_ms(),
            expires_ms: clock.now_ms() + 1_000_000,
            revoked_ms: None,
        })
        .unwrap();
    let owner = cp.authenticate(token.expose()).unwrap();
    let decided = cp
        .decide_approval(&owner, &approval_id, true, "ok", "dec-restart")
        .unwrap();
    assert_eq!(decided.status, faktor_cloud::ApprovalStatus::Approved);
    // The same key replays the recorded decision across the restart...
    let replayed = cp
        .decide_approval(&owner, &approval_id, true, "ok", "dec-restart")
        .unwrap();
    assert_eq!(replayed.decided_ms, decided.decided_ms);
    // ...and a new key on the terminal approval is a typed conflict.
    assert!(matches!(
        cp.decide_approval(&owner, &approval_id, false, "", "dec-restart-2")
            .unwrap_err(),
        ControlPlaneError::Conflict(_)
    ));

    // Idempotency keys are durable too: the same key + request replays and
    // the same key + different request conflicts, across the restart.
    let cp = ControlPlane::new(store, clock);
    let replay = bootstrap(&cp, "Acme", "owner@acme.test", "boot");
    assert_eq!(replay.organization.id, org);
    assert!(replay.token.is_none(), "no token is re-presented on replay");
    assert!(matches!(
        cp.bootstrap_organization("Different", "owner@acme.test", "Owner", "boot")
            .unwrap_err(),
        ControlPlaneError::Conflict(_)
    ));
}
