//! BK05 persistent permission policy: versioned rules with explicit scope
//! and expiry, ONE evaluator shared by preview and dispatch, managed denies
//! that can never be widened by a project/preset, and a bounded decision
//! audit.
//!
//! Matching is deliberately not prefix-based: a rule matches a canonical
//! target only by exact string equality or an explicit `glob:` pattern that
//! must match the WHOLE target. A decorative command prefix therefore never
//! approves a compound shell or an interpreter payload.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db::{now_ms, FoundationDb};
use crate::{page_limit_i64, FoundationError};

pub const MAX_RULES: usize = 2_000;
pub const MAX_RULE_TARGET_BYTES: usize = 4096;
pub const MAX_RULE_TOOL_BYTES: usize = 256;
pub const MAX_AUDIT_PAGE: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Ask => "ask",
            Decision::Deny => "deny",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "allow" => Decision::Allow,
            "ask" => Decision::Ask,
            "deny" => Decision::Deny,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleScope {
    User,
    Project,
    Session,
    Task,
    Managed,
}

impl RuleScope {
    pub fn as_str(self) -> &'static str {
        match self {
            RuleScope::User => "user",
            RuleScope::Project => "project",
            RuleScope::Session => "session",
            RuleScope::Task => "task",
            RuleScope::Managed => "managed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "user" => RuleScope::User,
            "project" => RuleScope::Project,
            "session" => RuleScope::Session,
            "task" => RuleScope::Task,
            "managed" => RuleScope::Managed,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub tool_id: String,
    pub canonical_target: String,
    pub effect: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub scope: RuleScope,
    pub tool_id: String,
    pub target: String,
    pub decision: Decision,
    pub created_ms: i64,
    pub expires_ms: Option<i64>,
    pub revision: u64,
    pub managed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    pub revision: u64,
    pub rules: Vec<Rule>,
    pub managed_denies: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub decision: Decision,
    pub revision: u64,
    pub rule_id: Option<String>,
    pub reason: String,
    pub matched: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleDraft {
    pub id: Option<String>,
    pub scope: RuleScope,
    pub tool_id: String,
    pub target: String,
    pub decision: Decision,
    pub expires_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: i64,
    pub decision: Decision,
    pub tool_id: String,
    pub target: String,
    pub rule_id: Option<String>,
    pub reason: String,
    pub revision: u64,
    pub decided_ms: i64,
    /// `preview` | `dispatch`
    pub source: String,
}

pub struct PolicyService {
    db: FoundationDb,
}

impl PolicyService {
    pub fn new(db: FoundationDb) -> Self {
        Self { db }
    }

    pub fn policy(&self) -> Result<Policy, FoundationError> {
        self.db
            .read("policy snapshot", |conn| {
                let revision: i64 = conn
                    .query_row(
                        "SELECT revision FROM permission_policy_meta WHERE id = 1",
                        [],
                        |r| r.get(0),
                    )
                    .map_err(|e| FoundationError::Store(e.to_string()))?;
                let mut stmt = conn
                    .prepare(
                        "SELECT id, scope, tool_id, target, decision, created_ms,
                            expires_ms, revision, managed
                     FROM permission_rule ORDER BY managed DESC, created_ms ASC, id ASC
                     LIMIT ?1",
                    )
                    .map_err(|e| FoundationError::Store(e.to_string()))?;
                let rules = stmt
                    .query_map(params![MAX_RULES as i64], |r| {
                        let scope_raw: String = r.get(1)?;
                        let decision_raw: String = r.get(4)?;
                        let scope = RuleScope::parse(&scope_raw).ok_or_else(|| {
                            rusqlite::Error::FromSqlConversionFailure(
                                1,
                                rusqlite::types::Type::Text,
                                Box::new(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "unknown rule scope",
                                )),
                            )
                        })?;
                        let decision = Decision::parse(&decision_raw).ok_or_else(|| {
                            rusqlite::Error::FromSqlConversionFailure(
                                4,
                                rusqlite::types::Type::Text,
                                Box::new(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "unknown rule decision",
                                )),
                            )
                        })?;
                        Ok(Rule {
                            id: r.get(0)?,
                            scope,
                            tool_id: r.get(2)?,
                            target: r.get(3)?,
                            decision,
                            created_ms: r.get(5)?,
                            expires_ms: r.get(6)?,
                            revision: u64::try_from(r.get::<_, i64>(7)?).unwrap_or(0),
                            managed: r.get::<_, i64>(8)? != 0,
                        })
                    })
                    .map_err(|e| FoundationError::Store(e.to_string()))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| FoundationError::Store(e.to_string()))?;
                Ok((revision, rules))
            })
            .map(|(revision, rules)| {
                let managed_denies = rules
                    .iter()
                    .filter(|r| r.managed && r.decision == Decision::Deny)
                    .cloned()
                    .collect();
                Policy {
                    revision: revision.max(0) as u64,
                    rules,
                    managed_denies,
                }
            })
    }

    /// Evaluate ONE canonical action. Managed denies always win; then the
    /// strongest matching user rule wins (Deny > Allow > Ask); unmatched =
    /// Ask. Expired rules never match. Both preview and dispatch call this
    /// exact function.
    pub fn evaluate(&self, action: &Action) -> Result<Verdict, FoundationError> {
        let policy = self.policy()?;
        let now = now_ms();
        if !valid_action(action) {
            return Ok(Verdict {
                decision: Decision::Deny,
                revision: policy.revision,
                rule_id: None,
                reason: "malformed canonical action".into(),
                matched: false,
            });
        }
        if let Some(rule) = policy
            .managed_denies
            .iter()
            .find(|rule| rule_matches(rule, action, now))
        {
            return Ok(Verdict {
                decision: Decision::Deny,
                revision: policy.revision,
                rule_id: Some(rule.id.clone()),
                reason: "enforced managed constraint".into(),
                matched: true,
            });
        }
        let mut matched: Option<&Rule> = None;
        for rule in &policy.rules {
            if rule.managed || !rule_matches(rule, action, now) {
                continue;
            }
            matched = Some(match matched {
                None => rule,
                Some(previous) => {
                    if strength(rule.decision) > strength(previous.decision) {
                        rule
                    } else {
                        previous
                    }
                }
            });
        }
        match matched {
            Some(rule) => Ok(Verdict {
                decision: rule.decision,
                revision: policy.revision,
                rule_id: Some(rule.id.clone()),
                reason: format!(
                    "matched {} rule {} for {}",
                    rule.scope.as_str(),
                    rule.id,
                    action.canonical_target
                ),
                matched: true,
            }),
            None => Ok(Verdict {
                decision: Decision::Ask,
                revision: policy.revision,
                rule_id: None,
                reason: "no matching rule; ask".into(),
                matched: false,
            }),
        }
    }

    /// Evaluate AND record a bounded audit row. `source` is `preview` or
    /// `dispatch` (never an untrusted value).
    pub fn evaluate_recorded(
        &self,
        action: &Action,
        source: &str,
    ) -> Result<Verdict, FoundationError> {
        let source = match source {
            "preview" | "dispatch" => source,
            _ => {
                return Err(FoundationError::Malformed(
                    "audit source must be preview or dispatch".into(),
                ))
            }
        };
        let verdict = self.evaluate(action)?;
        self.db.write("policy audit", |conn| {
            conn.execute(
                "INSERT INTO permission_audit(
                   decision, tool_id, target, rule_id, reason, revision, decided_ms, source)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    verdict.decision.as_str(),
                    action.tool_id,
                    action.canonical_target,
                    verdict.rule_id,
                    verdict.reason,
                    verdict.revision as i64,
                    now_ms(),
                    source
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(())
        })?;
        Ok(verdict)
    }

    pub fn upsert_rule(
        &self,
        draft: &RuleDraft,
        expected_revision: u64,
    ) -> Result<Rule, FoundationError> {
        validate_rule(draft)?;
        let id = draft.id.clone().unwrap_or_else(new_rule_id);
        self.db.write("policy upsert", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin rule: {e}")))?;
            let current: i64 = tx
                .query_row(
                    "SELECT revision FROM permission_policy_meta WHERE id = 1",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if current.max(0) as u64 != expected_revision {
                return Err(FoundationError::Conflict(format!(
                    "policy revision changed: expected {expected_revision} but found {current}"
                )));
            }
            let count: i64 = tx
                .query_row("SELECT COUNT(*) FROM permission_rule", [], |r| r.get(0))
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT revision FROM permission_rule WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if existing.is_none() && count as usize >= MAX_RULES {
                return Err(FoundationError::Oversized(format!(
                    "at most {MAX_RULES} permission rules are retained"
                )));
            }
            let revision = current.max(0) as u64 + 1;
            let now = now_ms();
            tx.execute(
                "INSERT INTO permission_rule(
                   id, scope, tool_id, target, decision, created_ms, expires_ms, revision, managed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)
                 ON CONFLICT(id) DO UPDATE SET
                   scope = excluded.scope,
                   tool_id = excluded.tool_id,
                   target = excluded.target,
                   decision = excluded.decision,
                   expires_ms = excluded.expires_ms,
                   revision = excluded.revision",
                params![
                    id,
                    draft.scope.as_str(),
                    draft.tool_id,
                    draft.target,
                    draft.decision.as_str(),
                    now,
                    draft.expires_ms,
                    revision as i64
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.execute(
                "UPDATE permission_policy_meta SET revision = ?1, updated_ms = ?2 WHERE id = 1",
                params![revision as i64, now],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rule = tx
                .query_row(
                    "SELECT id, scope, tool_id, target, decision, created_ms, expires_ms, revision, managed
                     FROM permission_rule WHERE id = ?1",
                    params![id],
                    map_rule,
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit rule: {e}")))?;
            Ok(rule)
        })
    }

    pub fn delete_rule(&self, id: &str, expected_revision: u64) -> Result<u64, FoundationError> {
        self.db.write("policy delete", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin delete: {e}")))?;
            let current: i64 = tx
                .query_row(
                    "SELECT revision FROM permission_policy_meta WHERE id = 1",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if current.max(0) as u64 != expected_revision {
                return Err(FoundationError::Conflict(format!(
                    "policy revision changed: expected {expected_revision} but found {current}"
                )));
            }
            let managed: Option<i64> = tx
                .query_row(
                    "SELECT managed FROM permission_rule WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            match managed {
                None => return Err(FoundationError::NotFound(format!("permission rule {id}"))),
                Some(1) => {
                    return Err(FoundationError::Conflict(
                        "managed rules cannot be deleted locally".into(),
                    ))
                }
                Some(_) => {}
            }
            tx.execute("DELETE FROM permission_rule WHERE id = ?1", params![id])
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let revision = current.max(0) as u64 + 1;
            tx.execute(
                "UPDATE permission_policy_meta SET revision = ?1, updated_ms = ?2 WHERE id = 1",
                params![revision as i64, now_ms()],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit delete: {e}")))?;
            Ok(revision)
        })
    }

    /// Insert a managed deny. Only the managed/enterprise authority calls
    /// this; HTTP surfaces cannot set `managed`.
    pub fn upsert_managed_deny(&self, draft: &RuleDraft) -> Result<Rule, FoundationError> {
        validate_rule(draft)?;
        let id = draft.id.clone().unwrap_or_else(new_rule_id);
        self.db.write("policy managed", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin managed: {e}")))?;
            let current: i64 = tx
                .query_row(
                    "SELECT revision FROM permission_policy_meta WHERE id = 1",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let revision = current.max(0) as u64 + 1;
            tx.execute(
                "INSERT INTO permission_rule(
                   id, scope, tool_id, target, decision, created_ms, expires_ms, revision, managed)
                 VALUES (?1, ?2, ?3, ?4, 'deny', ?5, ?6, ?7, 1)
                 ON CONFLICT(id) DO UPDATE SET
                   scope = excluded.scope,
                   tool_id = excluded.tool_id,
                   target = excluded.target,
                   decision = 'deny',
                   expires_ms = excluded.expires_ms,
                   revision = excluded.revision,
                   managed = 1",
                params![
                    id,
                    RuleScope::Managed.as_str(),
                    draft.tool_id,
                    draft.target,
                    now_ms(),
                    draft.expires_ms,
                    revision as i64
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.execute(
                "UPDATE permission_policy_meta SET revision = ?1, updated_ms = ?2 WHERE id = 1",
                params![revision as i64, now_ms()],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rule = tx
                .query_row(
                    "SELECT id, scope, tool_id, target, decision, created_ms, expires_ms, revision, managed
                     FROM permission_rule WHERE id = ?1",
                    params![id],
                    map_rule,
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit managed: {e}")))?;
            Ok(rule)
        })
    }

    pub fn audit_page(
        &self,
        before_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<AuditEntry>, FoundationError> {
        let limit = page_limit_i64(limit.clamp(1, MAX_AUDIT_PAGE), "policy audit")?;
        self.db.read("policy audit page", |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, decision, tool_id, target, rule_id, reason, revision, decided_ms, source
                     FROM permission_audit WHERE id < ?1 ORDER BY id DESC LIMIT ?2",
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map(
                    params![before_id.unwrap_or(i64::MAX), limit],
                    |r| {
                        let decision_raw: String = r.get(1)?;
                        Ok(AuditEntry {
                            id: r.get(0)?,
                            decision: Decision::parse(&decision_raw).unwrap_or(Decision::Deny),
                            tool_id: r.get(2)?,
                            target: r.get(3)?,
                            rule_id: r.get(4)?,
                            reason: r.get(5)?,
                            revision: u64::try_from(r.get::<_, i64>(6)?).unwrap_or(0),
                            decided_ms: r.get(7)?,
                            source: r.get(8)?,
                        })
                    },
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(rows)
        })
    }
}

fn map_rule(row: &rusqlite::Row<'_>) -> rusqlite::Result<Rule> {
    let scope_raw: String = row.get(1)?;
    let decision_raw: String = row.get(4)?;
    Ok(Rule {
        id: row.get(0)?,
        scope: RuleScope::parse(&scope_raw).unwrap_or(RuleScope::User),
        tool_id: row.get(2)?,
        target: row.get(3)?,
        decision: Decision::parse(&decision_raw).unwrap_or(Decision::Deny),
        created_ms: row.get(5)?,
        expires_ms: row.get(6)?,
        revision: u64::try_from(row.get::<_, i64>(7)?).unwrap_or(0),
        managed: row.get::<_, i64>(8)? != 0,
    })
}

fn strength(decision: Decision) -> u8 {
    match decision {
        Decision::Deny => 3,
        Decision::Allow => 2,
        Decision::Ask => 1,
    }
}

fn valid_action(action: &Action) -> bool {
    !action.tool_id.is_empty()
        && action.tool_id.len() <= MAX_RULE_TOOL_BYTES
        && !action.canonical_target.is_empty()
        && action.canonical_target.len() <= MAX_RULE_TARGET_BYTES
        && !action.effect.is_empty()
        && action.effect.len() <= 64
}

fn validate_rule(draft: &RuleDraft) -> Result<(), FoundationError> {
    if draft.tool_id.is_empty() || draft.tool_id.len() > MAX_RULE_TOOL_BYTES {
        return Err(FoundationError::Malformed(format!(
            "rule tool_id must be 1..={MAX_RULE_TOOL_BYTES} bytes"
        )));
    }
    if draft.target.is_empty() || draft.target.len() > MAX_RULE_TARGET_BYTES {
        return Err(FoundationError::Malformed(format!(
            "rule target must be 1..={MAX_RULE_TARGET_BYTES} bytes"
        )));
    }
    if draft.target.contains('\n') || draft.target.contains('\u{0}') {
        return Err(FoundationError::Malformed(
            "rule target must be a single canonical line".into(),
        ));
    }
    // An allow/ask `glob:` ending in `*` is a prefix rule in disguise and is
    // refused: it would bless arbitrary compound suffixes. Managed denies
    // may stay broad because they only ever remove authority.
    if draft.decision != Decision::Deny
        && draft.target.starts_with("glob:")
        && draft.target.ends_with('*')
    {
        return Err(FoundationError::Malformed(
            "allow rules may not use an open-ended `*` suffix; use an exact target or a closed glob"
                .into(),
        ));
    }
    if let Some(expires) = draft.expires_ms {
        if expires <= now_ms() {
            return Err(FoundationError::Malformed(
                "rule expiry is already in the past".into(),
            ));
        }
    }
    Ok(())
}

/// Exact or whole-string glob matching. No implicit prefix/substring form
/// exists, so a short rule can never bless a longer compound command.
pub fn rule_matches(rule: &Rule, action: &Action, now_ms: i64) -> bool {
    if rule.expires_ms.is_some_and(|expires| expires <= now_ms) {
        return false;
    }
    if rule.tool_id != action.tool_id && rule.tool_id != "*" {
        return false;
    }
    match rule.target.strip_prefix("glob:") {
        Some(pattern) => glob_match(pattern.as_bytes(), action.canonical_target.as_bytes()),
        None => rule.target == action.canonical_target,
    }
}

/// Iterative `*`/`?` glob matcher (O(pattern × target) with a backtrack
/// pointer, never recursive).
fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut resume) = (None, 0usize);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            resume = t;
        } else if let Some(star_pos) = star {
            p = star_pos + 1;
            resume += 1;
            t = resume;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn new_rule_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = blake3::Hasher::new();
    hasher.update(&now_ms().to_le_bytes());
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&counter.to_le_bytes());
    let digest = hasher.finalize();
    format!("rule-{}", &digest.to_hex()[..24])
}

/// Map one typed capability to the canonical action the evaluator sees.
/// Canonical normalization happens here (trusted daemon adapter), never in a
/// string widget.
pub fn action_for_capability(capability: &faktor_core::capability::Capability) -> Action {
    use faktor_core::capability::Capability;
    match capability {
        Capability::ReadWorkspace { path } => Action {
            tool_id: "read".into(),
            canonical_target: normalize_path(path),
            effect: "read".into(),
        },
        Capability::WriteWorkspace { path } => Action {
            tool_id: "write".into(),
            canonical_target: normalize_path(path),
            effect: "write".into(),
        },
        Capability::ReadExternal { path } => Action {
            tool_id: "read_external".into(),
            canonical_target: normalize_path(path),
            effect: "read".into(),
        },
        Capability::WriteExternal { path } => Action {
            tool_id: "write_external".into(),
            canonical_target: normalize_path(path),
            effect: "write".into(),
        },
        Capability::ExecuteShell { command } => Action {
            tool_id: "execute".into(),
            canonical_target: normalize_command(command),
            effect: "execute".into(),
        },
        Capability::Network { destination } => Action {
            tool_id: "network".into(),
            canonical_target: destination.trim().to_string(),
            effect: "network".into(),
        },
        Capability::Mcp { server } => Action {
            tool_id: "mcp".into(),
            canonical_target: server.trim().to_string(),
            effect: "mcp".into(),
        },
        Capability::Git { operation } => Action {
            tool_id: "git".into(),
            canonical_target: operation.trim().to_string(),
            effect: "git".into(),
        },
    }
}

fn normalize_path(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Whitespace collapse only: the raw shell text remains visible, and no
/// interpretation (prefix extraction, wrapper stripping) can happen here.
fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> (tempfile::TempDir, PolicyService) {
        let dir = tempfile::tempdir().unwrap();
        let db = FoundationDb::open(dir.path().join("f.db")).unwrap();
        (dir, PolicyService::new(db))
    }

    fn draft(target: &str, decision: Decision) -> RuleDraft {
        RuleDraft {
            id: None,
            scope: RuleScope::User,
            tool_id: "execute".into(),
            target: target.into(),
            decision,
            expires_ms: None,
        }
    }

    fn action(command: &str) -> Action {
        Action {
            tool_id: "execute".into(),
            canonical_target: normalize_command(command),
            effect: "execute".into(),
        }
    }

    #[test]
    fn unmatched_is_ask_and_exact_rules_apply_once() {
        let (_dir, policy) = service();
        assert_eq!(
            policy.evaluate(&action("ls")).unwrap().decision,
            Decision::Ask
        );
        let rev = policy.policy().unwrap().revision;
        policy
            .upsert_rule(&draft("ls", Decision::Allow), rev)
            .unwrap();
        assert_eq!(
            policy.evaluate(&action("ls")).unwrap().decision,
            Decision::Allow
        );
        // A decorative prefix never approves a compound command.
        assert_eq!(
            policy.evaluate(&action("ls && rm -rf /")).unwrap().decision,
            Decision::Ask
        );
    }

    #[test]
    fn glob_is_whole_string_and_managed_denies_always_win() {
        let (_dir, policy) = service();
        let rev = policy.policy().unwrap().revision;
        policy
            .upsert_rule(&draft("glob:git status*--short", Decision::Allow), rev)
            .unwrap();
        assert_eq!(
            policy
                .evaluate(&action("git status --short"))
                .unwrap()
                .decision,
            Decision::Allow
        );
        assert_eq!(
            policy
                .evaluate(&action("git status; curl evil"))
                .unwrap()
                .decision,
            Decision::Ask
        );
        assert!(policy
            .upsert_rule(
                &draft("glob:git status*", Decision::Allow),
                policy.policy().unwrap().revision
            )
            .is_err());
        policy
            .upsert_managed_deny(&draft("glob:git *", Decision::Deny))
            .unwrap();
        let verdict = policy.evaluate(&action("git status")).unwrap();
        assert_eq!(verdict.decision, Decision::Deny);
        assert!(verdict.reason.contains("managed"));
        // A user rule can never remove the managed deny.
        let rev = policy.policy().unwrap().revision;
        policy
            .upsert_rule(&draft("glob:git status*--short", Decision::Allow), rev)
            .unwrap();
        assert_eq!(
            policy.evaluate(&action("git status")).unwrap().decision,
            Decision::Deny
        );
    }

    #[test]
    fn expiry_revision_conflicts_and_audit_are_durable() {
        let (_dir, policy) = service();
        let rev = policy.policy().unwrap().revision;
        let rule = policy
            .upsert_rule(
                &RuleDraft {
                    expires_ms: Some(now_ms() + 50),
                    ..draft("ls", Decision::Allow)
                },
                rev,
            )
            .unwrap();
        assert_eq!(
            policy
                .upsert_rule(&draft("pwd", Decision::Allow), rev)
                .unwrap_err()
                .to_string(),
            format!(
                "conflict: policy revision changed: expected {rev} but found {}",
                rev + 1
            )
        );
        std::thread::sleep(std::time::Duration::from_millis(60));
        assert_eq!(
            policy.evaluate(&action("ls")).unwrap().decision,
            Decision::Ask
        );
        let rev = policy.policy().unwrap().revision;
        policy.delete_rule(&rule.id, rev).unwrap();
        let verdict = policy
            .evaluate_recorded(&action("pwd"), "dispatch")
            .unwrap();
        assert_eq!(verdict.decision, Decision::Ask);
        let audit = policy.audit_page(None, 10).unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].source, "dispatch");
        assert!(policy.audit_page(Some(audit[0].id), 10).unwrap().is_empty());
    }

    #[test]
    fn capability_mapping_is_canonical_and_never_prefix_based() {
        use faktor_core::capability::Capability;
        let a = action_for_capability(&Capability::ExecuteShell {
            command: "  sh   -c 'echo hi' ".into(),
        });
        assert_eq!(a.canonical_target, "sh -c 'echo hi'");
        let b = action_for_capability(&Capability::WriteWorkspace {
            path: std::path::PathBuf::from("src/lib.rs"),
        });
        assert_eq!(b.tool_id, "write");
        assert_eq!(b.canonical_target, "src/lib.rs");
    }
}
