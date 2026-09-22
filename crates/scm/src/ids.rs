//! Typed, validated SCM identities. Every id is a distinct type (a
//! repository name can never be passed where an installation id is
//! expected), every constructor validates its bounds, and deserialization
//! re-validates (a hostile DTO can never smuggle an invalid identity into
//! the domain).

use std::fmt;
use std::num::NonZeroU64;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::ScmError;

/// Bound on one repository owner (user or organization) name.
pub const MAX_OWNER_BYTES: usize = 100;
/// Bound on one repository name.
pub const MAX_REPOSITORY_BYTES: usize = 100;
/// Bound on one git ref name accepted by [`RemoteRef`].
pub const MAX_REF_NAME_BYTES: usize = 255;
/// Bound on one external-operation identity string.
pub const MAX_OPERATION_ID_BYTES: usize = 200;
/// The largest installation id the type admits: `i64::MAX`.
///
/// The durable rows persist installation ids in SIGNED SQLite `INTEGER`
/// columns; bounding the type here keeps the signed column an exact,
/// injective, order-preserving image of this type's domain (GitHub App
/// installation ids do not need the full `u64` space).
pub const MAX_INSTALLATION_ID: u64 = i64::MAX as u64;

/// The provider-side installation identity (GitHub App installation id):
/// `1..=i64::MAX`, never zero.
///
/// # Signed SQLite mapping
///
/// `scm_installation.installation_id` and `scm_repository.installation_id`
/// are SIGNED SQLite `INTEGER` columns, so the type deliberately does NOT
/// admit the full `u64` space. [`ScmInstallationId::to_sqlite_i64`] is the
/// mapping: total, injective and order-preserving, so SQL
/// `ORDER BY`/range/`MAX` and external tooling observe exactly the declared
/// semantics. A raw value above [`MAX_INSTALLATION_ID`] is refused typed by
/// every constructor naming the limit; a `u64` above `i64::MAX` would wrap
/// negative in the column and invert ordering/positivity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScmInstallationId(u64);

impl ScmInstallationId {
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// The signed SQLite `INTEGER` image of this id: the exact value the
    /// `installation_id` columns hold.
    ///
    /// Total and lossless by the constructor bound `1..=i64::MAX`: the
    /// cast can never wrap, and `a < b` implies
    /// `to_sqlite_i64(a) < to_sqlite_i64(b)`.
    pub const fn to_sqlite_i64(self) -> i64 {
        self.0 as i64
    }

    /// The only raw-`u64` constructor: zero is not a real installation, and
    /// a value above [`MAX_INSTALLATION_ID`] cannot be represented in the
    /// signed SQLite column, so both are typed errors. Untrusted/decoded
    /// values (webhook payloads, storage, wire) must enter through here;
    /// there is no infallible `u64` constructor that could smuggle an
    /// out-of-domain value into the domain.
    pub fn try_from_raw(raw: u64) -> Result<Self, ScmError> {
        if raw == 0 {
            return Err(ScmError::InvalidInput("installation id cannot be 0".into()));
        }
        if raw > MAX_INSTALLATION_ID {
            return Err(ScmError::InvalidInput(format!(
                "installation id {raw} exceeds the maximum {MAX_INSTALLATION_ID} \
                 (i64::MAX, the signed SQLite INTEGER bound)"
            )));
        }
        Ok(Self(raw))
    }

    /// Guarded constructor from an already-non-zero value. Non-zero alone is
    /// NOT sufficient (the signed-SQLite bound still applies), so this
    /// re-validates through [`Self::try_from_raw`]; there is no infallible
    /// constructor that could admit a value above [`MAX_INSTALLATION_ID`].
    pub fn try_from_non_zero(raw: NonZeroU64) -> Result<Self, ScmError> {
        Self::try_from_raw(raw.get())
    }
}

impl fmt::Display for ScmInstallationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for ScmInstallationId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for ScmInstallationId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = u64::deserialize(d)?;
        Self::try_from_raw(raw).map_err(D::Error::custom)
    }
}

/// One provider-neutral repository identity: installation + owner + name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct RepositoryRef {
    installation: ScmInstallationId,
    owner: String,
    name: String,
}

impl RepositoryRef {
    pub fn try_new(
        installation: ScmInstallationId,
        owner: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<Self, ScmError> {
        let owner = owner.into();
        let name = name.into();
        validate_segment("repository owner", &owner, MAX_OWNER_BYTES)?;
        validate_segment("repository name", &name, MAX_REPOSITORY_BYTES)?;
        if name == "." || name == ".." {
            return Err(ScmError::InvalidInput(format!(
                "repository name {name:?} is not a valid path segment"
            )));
        }
        Ok(Self {
            installation,
            owner,
            name,
        })
    }

    pub fn installation(&self) -> ScmInstallationId {
        self.installation
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `owner/name` (GitHub's stable repository identity spelling).
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// Case-insensitive equality (GitHub repository names are
    /// case-insensitive).
    pub fn same_repository(&self, other: &Self) -> bool {
        self.installation == other.installation
            && self.owner.eq_ignore_ascii_case(&other.owner)
            && self.name.eq_ignore_ascii_case(&other.name)
    }
}

impl<'de> Deserialize<'de> for RepositoryRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct File {
            installation: ScmInstallationId,
            owner: String,
            name: String,
        }
        let file = File::deserialize(d)?;
        Self::try_new(file.installation, file.owner, file.name).map_err(D::Error::custom)
    }
}

/// One issue identity: repository + positive issue number.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct IssueRef {
    repository: RepositoryRef,
    number: u64,
}

impl IssueRef {
    pub fn try_new(repository: RepositoryRef, number: u64) -> Result<Self, ScmError> {
        validate_number("issue number", number)?;
        Ok(Self { repository, number })
    }

    pub fn repository(&self) -> &RepositoryRef {
        &self.repository
    }

    pub fn number(&self) -> u64 {
        self.number
    }
}

impl<'de> Deserialize<'de> for IssueRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct File {
            repository: RepositoryRef,
            number: u64,
        }
        let file = File::deserialize(d)?;
        Self::try_new(file.repository, file.number).map_err(D::Error::custom)
    }
}

/// One pull-request identity: repository + positive PR number.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct PullRequestRef {
    repository: RepositoryRef,
    number: u64,
}

impl PullRequestRef {
    pub fn try_new(repository: RepositoryRef, number: u64) -> Result<Self, ScmError> {
        validate_number("pull request number", number)?;
        Ok(Self { repository, number })
    }

    pub fn repository(&self) -> &RepositoryRef {
        &self.repository
    }

    pub fn number(&self) -> u64 {
        self.number
    }
}

impl<'de> Deserialize<'de> for PullRequestRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct File {
            repository: RepositoryRef,
            number: u64,
        }
        let file = File::deserialize(d)?;
        Self::try_new(file.repository, file.number).map_err(D::Error::custom)
    }
}

/// One git ref inside a repository (`refs/heads/x`, `refs/tags/y`, or a
/// bare branch name, which is normalized to `refs/heads/<name>`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct RemoteRef {
    repository: RepositoryRef,
    name: String,
}

impl RemoteRef {
    pub fn try_new(repository: RepositoryRef, name: impl Into<String>) -> Result<Self, ScmError> {
        let name = name.into();
        validate_ref_name(&name)?;
        Ok(Self { repository, name })
    }

    pub fn repository(&self) -> &RepositoryRef {
        &self.repository
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The full ref spelling (`refs/...`), normalizing a bare branch name.
    pub fn canonical(&self) -> String {
        if self.name.starts_with("refs/") {
            self.name.clone()
        } else {
            format!("refs/heads/{}", self.name)
        }
    }
}

impl<'de> Deserialize<'de> for RemoteRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct File {
            repository: RepositoryRef,
            name: String,
        }
        let file = File::deserialize(d)?;
        Self::try_new(file.repository, file.name).map_err(D::Error::custom)
    }
}

/// The caller's durable external-operation identity (journaled before any
/// remote call). Bounded printable ASCII without whitespace: it is a stable
/// key, never free-form prose.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ExternalOperationId(String);

impl ExternalOperationId {
    pub fn try_new(raw: impl Into<String>) -> Result<Self, ScmError> {
        let raw = raw.into();
        if raw.is_empty() || raw.len() > MAX_OPERATION_ID_BYTES {
            return Err(ScmError::InvalidInput(format!(
                "external operation id must be 1..={MAX_OPERATION_ID_BYTES} bytes"
            )));
        }
        if !raw
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\')
        {
            return Err(ScmError::InvalidInput(
                "external operation id must be printable ASCII without whitespace, quotes or backslashes"
                    .into(),
            ));
        }
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExternalOperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ExternalOperationId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::try_new(raw).map_err(D::Error::custom)
    }
}

/// Validate one path-segment-shaped name: bounded ASCII alphanumerics plus
/// `-`, `_` and `.`; no leading/trailing separator, no `..`.
pub fn validate_segment(kind: &str, value: &str, max: usize) -> Result<(), ScmError> {
    if value.is_empty() || value.len() > max {
        return Err(ScmError::InvalidInput(format!(
            "{kind} must be 1..={max} bytes"
        )));
    }
    if value.starts_with('-') || value.starts_with('.') || value.ends_with('.') {
        return Err(ScmError::InvalidInput(format!(
            "{kind} {value:?} may not start with '-'/'.' or end with '.'"
        )));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        return Err(ScmError::InvalidInput(format!(
            "{kind} {value:?} contains characters outside [A-Za-z0-9._-]"
        )));
    }
    Ok(())
}

fn validate_number(kind: &str, value: u64) -> Result<(), ScmError> {
    if value == 0 {
        return Err(ScmError::InvalidInput(format!("{kind} cannot be 0")));
    }
    Ok(())
}

/// Validate one git ref/branch name with the documented git rules that
/// matter for remote calls: bounded, no control characters or spaces, no
/// `..`, `@{`, `~^:?*[\`, no leading/trailing `/`, no `//`, no component
/// starting with `.` or ending with `.lock`, no lone `@`.
pub fn validate_ref_name(name: &str) -> Result<(), ScmError> {
    if name.is_empty() || name.len() > MAX_REF_NAME_BYTES {
        return Err(ScmError::InvalidInput(format!(
            "ref name must be 1..={MAX_REF_NAME_BYTES} bytes"
        )));
    }
    if name != name.trim() || name.starts_with('/') || name.ends_with('/') || name.ends_with('.') {
        return Err(ScmError::InvalidInput(format!(
            "ref name {name:?} has an illegal shape"
        )));
    }
    if name.contains("..")
        || name.contains("@{")
        || name.contains("//")
        || name.contains("\\")
        || name.contains('\u{7f}')
    {
        return Err(ScmError::InvalidInput(format!(
            "ref name {name:?} contains a forbidden sequence"
        )));
    }
    if name.bytes().any(|b| {
        b.is_ascii_control() || b == b' ' || matches!(b, b'~' | b'^' | b':' | b'?' | b'*' | b'[')
    }) {
        return Err(ScmError::InvalidInput(format!(
            "ref name {name:?} contains a forbidden character"
        )));
    }
    if name == "@" {
        return Err(ScmError::InvalidInput("ref name cannot be '@'".into()));
    }
    for component in name.split('/') {
        if component.is_empty() || component.starts_with('.') || component.ends_with(".lock") {
            return Err(ScmError::InvalidInput(format!(
                "ref name {name:?} has an illegal component {component:?}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> RepositoryRef {
        RepositoryRef::try_new(
            ScmInstallationId::try_from_raw(7).unwrap(),
            "acme",
            "widgets",
        )
        .unwrap()
    }

    #[test]
    fn installation_zero_is_refused_as_a_typed_error() {
        match ScmInstallationId::try_from_raw(0) {
            Err(ScmError::InvalidInput(message)) => {
                assert!(
                    message.contains("installation id cannot be 0"),
                    "the refusal names the invariant: {message}"
                );
            }
            other => panic!("zero must be a typed InvalidInput, got {other:?}"),
        }
        assert_eq!(ScmInstallationId::try_from_raw(9).unwrap().raw(), 9);
        assert!(serde_json::from_str::<ScmInstallationId>("0").is_err());
        assert_eq!(
            serde_json::from_str::<ScmInstallationId>("12").unwrap(),
            ScmInstallationId::try_from_raw(12).unwrap()
        );
        // The guarded NonZeroU64 path re-validates the signed-SQLite bound.
        assert_eq!(
            ScmInstallationId::try_from_non_zero(NonZeroU64::new(5).unwrap())
                .unwrap()
                .raw(),
            5
        );
    }

    /// P1 persistence-domain bound: the type admits exactly `1..=i64::MAX`
    /// so the signed SQLite `installation_id` column is an exact,
    /// order-preserving image. One above the bound and `u64::MAX` are typed
    /// refusals naming the limit — never a wrapped (negative) value.
    #[test]
    fn installation_id_is_bounded_by_the_signed_sqlite_integer() {
        let max = ScmInstallationId::try_from_raw(MAX_INSTALLATION_ID).unwrap();
        assert_eq!(max.raw(), i64::MAX as u64);
        assert_eq!(max.to_sqlite_i64(), i64::MAX);
        assert!(max.to_sqlite_i64() > 0, "the mapping stays positive");

        for out_of_domain in [MAX_INSTALLATION_ID + 1, u64::MAX] {
            match ScmInstallationId::try_from_raw(out_of_domain) {
                Err(ScmError::InvalidInput(message)) => {
                    assert!(
                        message.contains("9223372036854775807"),
                        "the refusal names the limit: {message}"
                    );
                    assert!(
                        message.contains("i64::MAX"),
                        "the refusal names the limit: {message}"
                    );
                    assert!(
                        message.contains(&out_of_domain.to_string()),
                        "the refusal names the offending value: {message}"
                    );
                }
                other => panic!("{out_of_domain} must be a typed refusal, got {other:?}"),
            }
        }

        // Deserialization re-validates the same bound: a hostile DTO cannot
        // smuggle an out-of-domain id into the domain.
        for hostile in ["9223372036854775808", "18446744073709551615"] {
            assert!(
                serde_json::from_str::<ScmInstallationId>(hostile).is_err(),
                "{hostile} must be refused by Deserialize"
            );
        }
        assert_eq!(
            serde_json::from_str::<ScmInstallationId>("9223372036854775807").unwrap(),
            max
        );

        // The guarded non-zero constructor re-validates the bound too.
        assert!(ScmInstallationId::try_from_non_zero(NonZeroU64::new(u64::MAX).unwrap()).is_err());
        assert!(ScmInstallationId::try_from_non_zero(
            NonZeroU64::new(MAX_INSTALLATION_ID + 1).unwrap()
        )
        .is_err());
        assert_eq!(
            ScmInstallationId::try_from_non_zero(NonZeroU64::new(1).unwrap())
                .unwrap()
                .to_sqlite_i64(),
            1
        );

        // Ordering is the domain order all the way to the bound: no wrap.
        let near = ScmInstallationId::try_from_raw(MAX_INSTALLATION_ID - 1).unwrap();
        let low = ScmInstallationId::try_from_raw(1).unwrap();
        assert!(low < near && near < max);
        assert!(low.to_sqlite_i64() < near.to_sqlite_i64());
        assert!(near.to_sqlite_i64() < max.to_sqlite_i64());
    }

    #[test]
    fn repository_ref_cannot_observe_a_zero_installation() {
        // There is no infallible raw-u64 constructor. Every path a zero
        // could take is either a typed error (try_from_raw / Deserialize)
        // or re-validated (try_from_non_zero).
        assert!(ScmInstallationId::try_from_raw(0).is_err());
        let err = serde_json::from_str::<RepositoryRef>(
            r#"{"installation":0,"owner":"acme","name":"widgets"}"#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("installation id cannot be 0"),
            "hostile zero must be refused before RepositoryRef sees an id: {err}"
        );
        let id = ScmInstallationId::try_from_non_zero(NonZeroU64::new(1).unwrap()).unwrap();
        assert_ne!(id.raw(), 0);
        assert!(RepositoryRef::try_new(id, "acme", "widgets").is_ok());
    }

    #[test]
    fn repository_segments_are_strictly_validated() {
        let one = ScmInstallationId::try_from_raw(1).unwrap();
        assert!(RepositoryRef::try_new(one, "", "x").is_err());
        assert!(RepositoryRef::try_new(one, "a/b", "x").is_err());
        assert!(RepositoryRef::try_new(one, "a", "..").is_err());
        assert!(RepositoryRef::try_new(one, "-a", "x").is_err());
        assert!(RepositoryRef::try_new(one, "a", "x.").is_err());
        assert!(RepositoryRef::try_new(one, "a b", "x").is_err());
        assert!(RepositoryRef::try_new(one, "a", "x".repeat(101)).is_err());
        assert_eq!(repo().full_name(), "acme/widgets");
        assert!(repo().same_repository(
            &RepositoryRef::try_new(
                ScmInstallationId::try_from_raw(7).unwrap(),
                "ACME",
                "Widgets"
            )
            .unwrap()
        ));
    }

    #[test]
    fn ref_numbers_and_operation_ids_are_bounded() {
        assert!(IssueRef::try_new(repo(), 0).is_err());
        assert!(PullRequestRef::try_new(repo(), 0).is_err());
        assert!(IssueRef::try_new(repo(), 1).is_ok());
        assert!(ExternalOperationId::try_new("").is_err());
        assert!(ExternalOperationId::try_new("has space").is_err());
        assert!(ExternalOperationId::try_new("quote\"inside").is_err());
        assert!(ExternalOperationId::try_new("a".repeat(201)).is_err());
        assert!(ExternalOperationId::try_new("task:1:rev:2:github:pull_request").is_ok());
    }

    #[test]
    fn ref_names_reject_git_hostile_shapes() {
        for bad in [
            "",
            "refs/heads/",
            "refs/heads/x..y",
            "refs/heads/x y",
            "refs/heads/x~1",
            "refs/heads/x:y",
            "refs/heads/x?",
            "refs/heads/x*",
            "refs/heads/x[",
            "refs/heads/x@{1}",
            "refs//heads/x",
            "/refs/heads/x",
            "refs/heads/.hidden",
            "refs/heads/x.lock",
            "@",
            "refs/heads/x\\y",
            "refs/heads/x.",
        ] {
            assert!(validate_ref_name(bad).is_err(), "{bad:?} must be refused");
        }
        for good in ["main", "refs/heads/feature/x-1", "refs/tags/v1.0.0"] {
            assert!(validate_ref_name(good).is_ok(), "{good:?} must be accepted");
        }
        let r = RemoteRef::try_new(repo(), "feature/x").unwrap();
        assert_eq!(r.canonical(), "refs/heads/feature/x");
        assert_eq!(
            RemoteRef::try_new(repo(), "refs/tags/v1")
                .unwrap()
                .canonical(),
            "refs/tags/v1"
        );
    }

    #[test]
    fn json_deserialization_revalidates_every_identity() {
        let ok = r#"{"installation":3,"owner":"acme","name":"widgets"}"#;
        let parsed: RepositoryRef = serde_json::from_str(ok).unwrap();
        assert_eq!(parsed.full_name(), "acme/widgets");
        for bad in [
            r#"{"installation":0,"owner":"acme","name":"widgets"}"#,
            r#"{"installation":9223372036854775808,"owner":"acme","name":"widgets"}"#,
            r#"{"installation":18446744073709551615,"owner":"acme","name":"widgets"}"#,
            r#"{"installation":3,"owner":"acme","name":"../escape"}"#,
            r#"{"installation":3,"owner":"acme","name":"widgets","extra":1}"#,
        ] {
            assert!(
                serde_json::from_str::<RepositoryRef>(bad).is_err(),
                "{bad:?} must be refused"
            );
        }
        assert!(serde_json::from_str::<IssueRef>(
            r#"{"repository":{"installation":3,"owner":"a","name":"b"},"number":0}"#
        )
        .is_err());
        assert!(serde_json::from_str::<ExternalOperationId>("\"a b\"").is_err());
    }
}
