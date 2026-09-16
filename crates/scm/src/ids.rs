//! Typed, validated SCM identities. Every id is a distinct type (a
//! repository name can never be passed where an installation id is
//! expected), every constructor validates its bounds, and deserialization
//! re-validates (a hostile DTO can never smuggle an invalid identity into
//! the domain).

use std::fmt;

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

/// The provider-side installation identity (GitHub App installation id).
/// Never zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScmInstallationId(u64);

impl ScmInstallationId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    /// A validated installation id: zero is not a real installation.
    pub fn try_from_raw(raw: u64) -> Result<Self, ScmError> {
        if raw == 0 {
            return Err(ScmError::InvalidInput("installation id cannot be 0".into()));
        }
        Ok(Self(raw))
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
        RepositoryRef::try_new(ScmInstallationId::new(7), "acme", "widgets").unwrap()
    }

    #[test]
    fn installation_zero_is_refused() {
        assert!(ScmInstallationId::try_from_raw(0).is_err());
        assert_eq!(ScmInstallationId::try_from_raw(9).unwrap().raw(), 9);
        assert!(serde_json::from_str::<ScmInstallationId>("0").is_err());
        assert_eq!(
            serde_json::from_str::<ScmInstallationId>("12").unwrap(),
            ScmInstallationId::new(12)
        );
    }

    #[test]
    fn repository_segments_are_strictly_validated() {
        assert!(RepositoryRef::try_new(ScmInstallationId::new(1), "", "x").is_err());
        assert!(RepositoryRef::try_new(ScmInstallationId::new(1), "a/b", "x").is_err());
        assert!(RepositoryRef::try_new(ScmInstallationId::new(1), "a", "..").is_err());
        assert!(RepositoryRef::try_new(ScmInstallationId::new(1), "-a", "x").is_err());
        assert!(RepositoryRef::try_new(ScmInstallationId::new(1), "a", "x.").is_err());
        assert!(RepositoryRef::try_new(ScmInstallationId::new(1), "a b", "x").is_err());
        assert!(RepositoryRef::try_new(ScmInstallationId::new(1), "a", "x".repeat(101)).is_err());
        assert_eq!(repo().full_name(), "acme/widgets");
        assert!(repo().same_repository(
            &RepositoryRef::try_new(ScmInstallationId::new(7), "ACME", "Widgets").unwrap()
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
