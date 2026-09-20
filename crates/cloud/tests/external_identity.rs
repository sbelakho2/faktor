//! Adversarial tests of subject-first external identity resolution: an IdP
//! email rename follows the linked subject (never re-homes it), a verified
//! email owned by a different account is a typed conflict with no implicit
//! merge, an explicit subject reassignment is refused, an unverified email
//! never overwrites or creates, concurrent logins across two SQLite
//! connections bind exactly one link, and organization isolation holds.
//!
//! Every scenario runs against the in-memory store AND a durable SQLite store.

use std::sync::{Arc, Barrier};

use faktor_cloud::ids::{OrganizationId, UserId};
use faktor_cloud::rbac::{PrincipalSubject, Role};
use faktor_cloud::store::{ControlPlaneStore, MemoryControlPlaneStore, SqliteControlPlaneStore};
use faktor_cloud::{ControlPlane, ControlPlaneError, ManualClock};

const T0: i64 = 1_700_000_000_000;
const IDP: &str = "https://idp.example.test";

fn bootstrap(cp: &ControlPlane, name: &str, owner_email: &str, key: &str) -> OrganizationId {
    cp.bootstrap_organization(name, owner_email, "Owner", key)
        .expect("bootstrap")
        .organization
        .id
}

fn principal_user(cp: &ControlPlane, token: &str) -> UserId {
    match cp.authenticate(token).expect("authenticate").subject {
        PrincipalSubject::User(id) => id,
        PrincipalSubject::ServiceAccount(_) => panic!("expected a user principal"),
    }
}

/// Run one scenario against both store backends.
fn for_each_store(run: impl Fn(&ControlPlane, &Arc<dyn ControlPlaneStore>)) {
    let store: Arc<dyn ControlPlaneStore> = Arc::new(MemoryControlPlaneStore::new());
    let cp = ControlPlane::new(store.clone(), Arc::new(ManualClock::new(T0)));
    run(&cp, &store);

    let dir = tempfile::tempdir().expect("tempdir");
    let store: Arc<dyn ControlPlaneStore> = Arc::new(
        SqliteControlPlaneStore::open(&dir.path().join("control-plane.db")).expect("open sqlite"),
    );
    let cp = ControlPlane::new(store.clone(), Arc::new(ManualClock::new(T0)));
    run(&cp, &store);
}

/// An IdP email change for a BOUND subject logs into the linked user and
/// adopts the new verified email on THAT user: the subject never moves.
#[test]
fn verified_email_rename_follows_the_linked_subject() {
    for_each_store(|cp, store| {
        let org = bootstrap(cp, "Acme", "owner@acme.test", "boot");
        let first = cp
            .login_external(
                &org,
                IDP,
                "sub-alice",
                "alice@example.test",
                "Alice",
                true,
                Role::Member,
            )
            .expect("first login");
        let alice = first.user.id.clone();
        assert_eq!(principal_user(cp, first.token.expose()), alice);

        // The IdP renames alice: the subject is unchanged, the email moved.
        let renamed = cp
            .login_external(
                &org,
                IDP,
                "sub-alice",
                "Alice.New@Example.test",
                "Alice",
                true,
                Role::Member,
            )
            .expect("renamed login");
        assert_eq!(
            renamed.user.id, alice,
            "the subject link decides the account"
        );
        assert_eq!(
            renamed.user.email, "alice.new@example.test",
            "the verified email is normalized and adopted"
        );
        assert_eq!(principal_user(cp, renamed.token.expose()), alice);
        assert_eq!(
            store.user(&alice).unwrap().unwrap().email,
            "alice.new@example.test",
            "the rename is durable"
        );
        assert!(
            store.user_by_email("alice@example.test").unwrap().is_none(),
            "the old address is freed, not duplicated"
        );
        assert_eq!(
            store
                .external_identity(IDP, "sub-alice")
                .unwrap()
                .unwrap()
                .user,
            alice
        );
        assert_eq!(store.external_identities_for_user(&alice).unwrap().len(), 1);
        assert_eq!(
            store.membership(&org, &alice).unwrap().unwrap().role,
            Role::Member,
            "exactly the one membership for the linked user"
        );
    });
}

/// A verified email that already belongs to a DIFFERENT user is a typed
/// conflict: neither account is touched and the subject is not transferred.
#[test]
fn verified_email_collision_conflicts_and_transfers_nothing() {
    for_each_store(|cp, store| {
        let org = bootstrap(cp, "Acme", "owner@acme.test", "boot");
        let alice_login = cp
            .login_external(
                &org,
                IDP,
                "sub-alice",
                "alice@example.test",
                "Alice",
                true,
                Role::Member,
            )
            .expect("alice login");
        let alice = alice_login.user.id.clone();
        let (bob, created) = cp.create_user("bob@example.test", "Bob").unwrap();
        assert!(created);
        let memberships_before = store.memberships(&org, None, 10).unwrap().len();
        let identities_before = store.external_identities_for_user(&bob.id).unwrap().len();

        let error = cp
            .login_external(
                &org,
                IDP,
                "sub-alice",
                "bob@example.test",
                "Alice",
                true,
                Role::Member,
            )
            .unwrap_err();
        assert!(
            matches!(error, ControlPlaneError::Conflict(_)),
            "expected a typed conflict, got {error:?}"
        );
        assert_eq!(error.http_status(), 409);

        // A keeps its email and its subject; B keeps its email and gains
        // nothing (no link, no membership, no session for this attempt).
        assert_eq!(
            store.user(&alice).unwrap().unwrap().email,
            "alice@example.test"
        );
        assert_eq!(
            store.user(&bob.id).unwrap().unwrap().email,
            "bob@example.test"
        );
        assert_eq!(
            store
                .external_identity(IDP, "sub-alice")
                .unwrap()
                .unwrap()
                .user,
            alice
        );
        assert!(store
            .external_identities_for_user(&bob.id)
            .unwrap()
            .is_empty());
        assert!(store.membership(&org, &bob.id).unwrap().is_none());
        assert_eq!(
            store.memberships(&org, None, 10).unwrap().len(),
            memberships_before,
            "no membership is added by the refused login"
        );
        assert_eq!(
            store.external_identities_for_user(&bob.id).unwrap().len(),
            identities_before
        );

        // The linked account can still log in with its recorded email.
        let still_alice = cp
            .login_external(
                &org,
                IDP,
                "sub-alice",
                "alice@example.test",
                "Alice",
                true,
                Role::Member,
            )
            .expect("later login");
        assert_eq!(still_alice.user.id, alice);
    });
}

/// An explicit attempt to link a bound subject to another user is refused
/// typed; re-linking to the SAME user stays idempotent.
#[test]
fn explicit_subject_reassignment_is_refused() {
    for_each_store(|cp, store| {
        let org = bootstrap(cp, "Acme", "owner@acme.test", "boot");
        let alice_login = cp
            .login_external(
                &org,
                IDP,
                "sub-alice",
                "alice@example.test",
                "Alice",
                true,
                Role::Member,
            )
            .expect("alice login");
        let alice = alice_login.user.id.clone();
        let (bob, _) = cp.create_user("bob@example.test", "Bob").unwrap();

        let error = cp
            .link_external_identity(&bob.id, IDP, "sub-alice")
            .unwrap_err();
        assert!(
            matches!(error, ControlPlaneError::Conflict(_)),
            "expected a typed conflict, got {error:?}"
        );
        assert_eq!(
            store
                .external_identity(IDP, "sub-alice")
                .unwrap()
                .unwrap()
                .user,
            alice,
            "the subject stays with its owner"
        );
        assert!(store
            .external_identities_for_user(&bob.id)
            .unwrap()
            .is_empty());

        let relink = cp
            .link_external_identity(&alice, IDP, "sub-alice")
            .expect("same-user re-link is idempotent");
        assert_eq!(relink.user, alice);
        assert_eq!(store.external_identities_for_user(&alice).unwrap().len(), 1);
    });
}

/// An unverified email never overwrites the linked account's email and never
/// creates a user; a brand-new subject with an unverified email is refused
/// before any row is written.
#[test]
fn unverified_email_never_overwrites_or_creates() {
    for_each_store(|cp, store| {
        let org = bootstrap(cp, "Acme", "owner@acme.test", "boot");
        let alice_login = cp
            .login_external(
                &org,
                IDP,
                "sub-alice",
                "alice@example.test",
                "Alice",
                true,
                Role::Member,
            )
            .expect("alice login");
        let alice = alice_login.user.id.clone();

        let unverified = cp
            .login_external(
                &org,
                IDP,
                "sub-alice",
                "attacker@example.test",
                "Alice",
                false,
                Role::Member,
            )
            .expect("an unverified claim does not block the linked login");
        assert_eq!(unverified.user.id, alice);
        assert_eq!(unverified.user.email, "alice@example.test");
        assert!(
            store
                .user_by_email("attacker@example.test")
                .unwrap()
                .is_none(),
            "no user is created from an unverified claim"
        );
        assert_eq!(
            store.user(&alice).unwrap().unwrap().email,
            "alice@example.test"
        );

        let error = cp
            .login_external(
                &org,
                IDP,
                "sub-new",
                "unverified@example.test",
                "New",
                false,
                Role::Viewer,
            )
            .unwrap_err();
        assert!(
            matches!(error, ControlPlaneError::Unauthorized(_)),
            "expected an unauthorized refusal, got {error:?}"
        );
        assert!(store
            .user_by_email("unverified@example.test")
            .unwrap()
            .is_none());
        assert!(store.external_identity(IDP, "sub-new").unwrap().is_none());
    });
}

/// A disabled linked user is refused and the refusal writes nothing (no
/// email reconciliation, no session).
#[test]
fn disabled_linked_user_is_refused_without_writes() {
    for_each_store(|cp, store| {
        let org = bootstrap(cp, "Acme", "owner@acme.test", "boot");
        let login = cp
            .login_external(
                &org,
                IDP,
                "sub-alice",
                "alice@example.test",
                "Alice",
                true,
                Role::Member,
            )
            .expect("alice login");
        let mut disabled = store.user(&login.user.id).unwrap().unwrap();
        disabled.disabled = true;
        store.put_user(&disabled).unwrap();

        let error = cp
            .login_external(
                &org,
                IDP,
                "sub-alice",
                "renamed@example.test",
                "Alice",
                true,
                Role::Member,
            )
            .unwrap_err();
        assert!(matches!(error, ControlPlaneError::Unauthorized(_)));
        assert_eq!(
            store.user(&login.user.id).unwrap().unwrap().email,
            "alice@example.test",
            "a disabled user's email is not reconciled"
        );
    });
}

/// Two connections of the SAME SQLite database race a login for one new
/// subject: the transaction machinery serializes them, exactly ONE link row
/// exists, and both logins resolve the same user.
#[test]
fn concurrent_logins_bind_exactly_one_subject_across_two_connections() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("control-plane.db");
    let store_a: Arc<dyn ControlPlaneStore> =
        Arc::new(SqliteControlPlaneStore::open(&path).expect("open a"));
    let store_b: Arc<dyn ControlPlaneStore> =
        Arc::new(SqliteControlPlaneStore::open(&path).expect("open b"));
    let cp_a = ControlPlane::new(store_a.clone(), Arc::new(ManualClock::new(T0)));
    let cp_b = ControlPlane::new(store_b.clone(), Arc::new(ManualClock::new(T0)));
    let org = bootstrap(&cp_a, "Acme", "owner@acme.test", "boot");

    let barrier = Barrier::new(2);
    let login = |cp: &ControlPlane| {
        barrier.wait();
        cp.login_external(
            &org,
            IDP,
            "sub-race",
            "race@example.test",
            "Race",
            true,
            Role::Member,
        )
        .expect("concurrent login")
    };
    let (first, second) = std::thread::scope(|scope| {
        let handle_a = scope.spawn(|| login(&cp_a));
        let handle_b = scope.spawn(|| login(&cp_b));
        (
            handle_a.join().expect("thread a"),
            handle_b.join().expect("thread b"),
        )
    });

    assert_eq!(
        first.user.id, second.user.id,
        "both logins resolve one user"
    );
    let identity = store_a
        .external_identity(IDP, "sub-race")
        .unwrap()
        .expect("exactly one link");
    assert_eq!(identity.user, first.user.id);
    assert_eq!(
        store_a
            .external_identities_for_user(&first.user.id)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store_a
            .user_by_email("race@example.test")
            .unwrap()
            .unwrap()
            .id,
        first.user.id
    );
    assert_eq!(
        store_a
            .memberships(&org, None, 10)
            .unwrap()
            .iter()
            .filter(|m| m.user == first.user.id)
            .count(),
        1,
        "exactly one membership for the raced user"
    );
}

/// Delimiter-bearing `(provider, subject)` pairs must not alias: a joined
/// `"{provider}:{subject}"` key would collapse `("a:b", "c")` and `("a",
/// "b:c")` onto one row. Subject-first login must resolve each pair to its
/// own account on BOTH backends (SQLite's UNIQUE(provider, subject) is the
/// reference semantics).
#[test]
fn delimiter_bearing_provider_subject_pairs_do_not_alias() {
    for_each_store(|cp, store| {
        let org = bootstrap(cp, "Acme", "owner@acme.test", "boot");
        let left = cp
            .login_external(
                &org,
                "a:b",
                "c",
                "left@example.test",
                "Left",
                true,
                Role::Member,
            )
            .expect("left login");
        let right = cp
            .login_external(
                &org,
                "a",
                "b:c",
                "right@example.test",
                "Right",
                true,
                Role::Member,
            )
            .expect("right login");
        assert_ne!(
            left.user.id, right.user.id,
            "the collision pair must resolve to distinct accounts"
        );

        // Subject-first replays resolve the right account for each pair.
        let left_again = cp
            .login_external(
                &org,
                "a:b",
                "c",
                "left@example.test",
                "Left",
                true,
                Role::Member,
            )
            .expect("left replay");
        assert_eq!(left_again.user.id, left.user.id);
        let right_again = cp
            .login_external(
                &org,
                "a",
                "b:c",
                "right@example.test",
                "Right",
                true,
                Role::Member,
            )
            .expect("right replay");
        assert_eq!(right_again.user.id, right.user.id);

        assert_eq!(
            store.external_identity("a:b", "c").unwrap().unwrap().user,
            left.user.id
        );
        assert_eq!(
            store.external_identity("a", "b:c").unwrap().unwrap().user,
            right.user.id
        );
    });
}

/// Organization isolation: a linked subject joining a second org keeps both
/// memberships independent, and a missing or deleted org is a typed
/// `NotFound` that writes nothing.
#[test]
fn organization_isolation_is_preserved() {
    for_each_store(|cp, store| {
        let org_a = bootstrap(cp, "Alpha", "owner@alpha.test", "boot-a");
        let org_b = bootstrap(cp, "Beta", "owner@beta.test", "boot-b");
        let first = cp
            .login_external(
                &org_a,
                IDP,
                "sub-iso",
                "iso@alpha.test",
                "Iso",
                true,
                Role::Member,
            )
            .expect("login into alpha");
        let iso = first.user.id.clone();

        let second = cp
            .login_external(
                &org_b,
                IDP,
                "sub-iso",
                "iso@alpha.test",
                "Iso",
                true,
                Role::Viewer,
            )
            .expect("login into beta");
        assert_eq!(second.user.id, iso, "the same global user joins beta");
        assert_eq!(
            store.membership(&org_a, &iso).unwrap().unwrap().role,
            Role::Member,
            "alpha's membership is untouched"
        );
        assert_eq!(
            store.membership(&org_b, &iso).unwrap().unwrap().role,
            Role::Viewer
        );

        let missing = OrganizationId::try_new("org_00000000000000000000000000000000").unwrap();
        let error = cp
            .login_external(
                &missing,
                IDP,
                "sub-new",
                "new@example.test",
                "New",
                true,
                Role::Member,
            )
            .unwrap_err();
        assert_eq!(
            error,
            ControlPlaneError::NotFound("organization not found".into())
        );
        assert!(store.user_by_email("new@example.test").unwrap().is_none());
        assert!(store.external_identity(IDP, "sub-new").unwrap().is_none());

        let mut deleted = store.organization(&org_b).unwrap().unwrap();
        deleted.deleted = true;
        store.put_organization(&deleted).unwrap();
        let error = cp
            .login_external(
                &org_b,
                IDP,
                "sub-iso",
                "iso@alpha.test",
                "Iso",
                true,
                Role::Member,
            )
            .unwrap_err();
        assert_eq!(
            error,
            ControlPlaneError::NotFound("organization not found".into())
        );
    });
}
