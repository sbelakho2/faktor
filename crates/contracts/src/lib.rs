//! Frozen-contract generation from the COMPILED vocabulary types.
//!
//! Every frozen contract is derived mechanically from the compiled Rust
//! types, never from a hand-maintained list:
//!
//! * **Declaration order and serde names** come from the `serde::Deserialize`
//!   implementation the compiler built for the enum (a spy `Deserializer`
//!   captures the slice serde passes to `deserialize_enum`, which serde
//!   generated in declaration order with every `#[serde(rename)]` applied).
//! * **Representative values** come from an exhaustive `match` over the enum:
//!   adding, removing or renaming a variant breaks compilation, so the
//!   representative set can never silently drift from the type.
//! * **Internally tagged discriminants** (`#[serde(tag = "...")]`) are
//!   discovered from the serialized representatives and their declaration
//!   order is read back from serde's own unknown-variant report (a strict
//!   parser that fails loudly if the report shape ever changes).
//!
//! The canonical files live under `docs/contracts/` and are compared byte
//! for byte by `check`; any rename or reorder is a deliberate contract
//! change that must be written with `write`.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The canonical per-vocabulary file schema.
pub const CONTRACT_SCHEMA: &str = "faktor-frozen-contract/v1";
/// The digest manifest schema.
pub const MANIFEST_SCHEMA: &str = "faktor-frozen-contracts-manifest/v1";
/// Generator identity named in the manifest.
pub const GENERATOR: &str = "faktor-contracts";
/// Directory (repo-relative) holding every canonical file.
pub const CONTRACT_DIR: &str = "docs/contracts";
/// Payload sentinel used in representative values: it can never equal a wire
/// tag, so discriminator discovery cannot be fooled by a payload string.
const PROBE: &str = "faktor-contract-probe-value";
/// Unknown tag used to make serde report the variant list.
const PROBE_UNKNOWN: &str = "faktor-contract-probe-unknown";

// --------------------------------------------------------------------- types

/// One frozen variant: the Rust spelling and the compiled serde wire name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractVariant {
    pub name: String,
    pub wire: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// One canonical frozen-contract file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractFile {
    pub schema: String,
    pub vocabulary: String,
    #[serde(rename = "type")]
    pub type_path: String,
    pub representation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<String>,
    pub variants: Vec<ContractVariant>,
}

/// One entry of the generated error-code mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorCodeVariant {
    pub kind: String,
    pub wire: String,
    pub code: String,
    pub http_status: u16,
    pub retryable: bool,
}

/// The compiled `ErrorKind` -> API error envelope mapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorCodeFile {
    pub schema: String,
    pub vocabulary: String,
    #[serde(rename = "type")]
    pub type_path: String,
    pub authority: String,
    pub variants: Vec<ErrorCodeVariant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    pub path: String,
    pub detail: String,
}

#[derive(Debug)]
pub struct CheckReport {
    pub ok: bool,
    pub digest: String,
    pub checked: usize,
    pub mismatches: Vec<Mismatch>,
}

/// The complete generated tree (canonical files plus the digest manifest).
#[derive(Debug)]
pub struct Generated {
    pub files: Vec<(String, Vec<u8>)>,
    pub digest: String,
}

// ------------------------------------------------------------------ serde spy

struct EnumSpy<'a> {
    sink: &'a RefCell<Option<Vec<String>>>,
}

impl<'de, 'a> Deserializer<'de> for EnumSpy<'a> {
    type Error = de::value::Error;

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Self::Error> {
        Err(de::Error::custom(
            "enum spy: the type is not an externally tagged enum",
        ))
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        variants: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Self::Error> {
        *self.sink.borrow_mut() = Some(variants.iter().map(|name| (*name).to_string()).collect());
        Err(de::Error::custom("enum spy: vocabulary captured"))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct identifier ignored_any
    }
}

/// The compiled serde wire names of an externally tagged enum, in
/// DECLARATION ORDER (the slice serde's generated `Deserialize` passes to
/// `deserialize_enum`).
pub fn serde_enum_wire_order<T>() -> Result<Vec<String>, String>
where
    T: for<'de> Deserialize<'de>,
{
    let sink = RefCell::new(None);
    let _ = T::deserialize(EnumSpy { sink: &sink });
    sink.into_inner().ok_or_else(|| {
        format!(
            "{}: serde's Deserialize never reached deserialize_enum (the type is not an \
             externally tagged enum; use the tagged-discriminant extraction)",
            std::any::type_name::<T>()
        )
    })
}

/// Representative values: one `(Rust variant name, value)` entry per variant,
/// produced from an exhaustive `match` (a new/removed/renamed variant makes
/// this fail to compile).
macro_rules! exhaustive_reps {
    ($ty:ty; $($pat:pat => ($name:literal, $value:expr)),+ $(,)?) => {{
        #[allow(dead_code)]
        fn __exhaustiveness(value: $ty) -> &'static str {
            match value {
                $($pat => $name,)+
            }
        }
        vec![$(($name, $value)),+]
    }};
}

struct Rep {
    name: &'static str,
    value: Value,
    retryable: Option<bool>,
    code: Option<String>,
}

fn rep<T: Serialize>(
    name: &'static str,
    value: &T,
    retryable: Option<bool>,
    code: Option<String>,
) -> Result<Rep, String> {
    Ok(Rep {
        name,
        value: serde_json::to_value(value).map_err(|e| format!("{name}: serialize: {e}"))?,
        retryable,
        code,
    })
}

// ------------------------------------------------------------- builders

fn variant_from_rep(wire: String, rep: Rep) -> ContractVariant {
    ContractVariant {
        name: rep.name.to_string(),
        wire,
        retryable: rep.retryable,
        code: rep.code,
    }
}

/// The single key of an externally tagged serde value (`"Wire"` or
/// `{"Wire": ...}`).
fn external_wire(value: &Value) -> Option<String> {
    match value {
        Value::String(wire) => Some(wire.clone()),
        Value::Object(map) if map.len() == 1 => map.keys().next().cloned(),
        _ => None,
    }
}

/// Join the compiled declaration order with the exhaustive representatives
/// (matched by wire name, so a mismatched representative list fails loudly).
fn external_contract(
    vocabulary: &str,
    type_path: &str,
    order: Vec<String>,
    reps: Vec<Rep>,
) -> Result<ContractFile, String> {
    let mut by_wire: BTreeMap<String, Rep> = BTreeMap::new();
    for rep in reps {
        let wire = external_wire(&rep.value).ok_or_else(|| {
            format!(
                "{type_path}: representative {} did not serialize to a single external tag: {}",
                rep.name, rep.value
            )
        })?;
        if by_wire.insert(wire.clone(), rep).is_some() {
            return Err(format!(
                "{type_path}: duplicate representative wire variant '{wire}'"
            ));
        }
    }
    if by_wire.len() != order.len() || order.iter().any(|wire| !by_wire.contains_key(wire)) {
        return Err(format!(
            "{type_path}: compiled declaration order {order:?} does not match the exhaustive \
             representative set {:?}",
            by_wire.keys().collect::<Vec<_>>()
        ));
    }
    let variants = order
        .into_iter()
        .map(|wire| {
            let rep = by_wire.remove(&wire).expect("presence checked above");
            variant_from_rep(wire, rep)
        })
        .collect();
    Ok(ContractFile {
        schema: CONTRACT_SCHEMA.to_string(),
        vocabulary: vocabulary.to_string(),
        type_path: type_path.to_string(),
        representation: "external".to_string(),
        tag: None,
        authority: None,
        variants,
    })
}

/// The cross-variant string discriminator of a tagged enum: exactly one key
/// whose value is a non-probe string in EVERY representative.
fn discover_tag_key(values: &[Value], probe: &str) -> Result<String, String> {
    let mut candidates: Option<BTreeSet<String>> = None;
    for value in values {
        let map = value
            .as_object()
            .ok_or_else(|| format!("tagged representative is not a JSON object: {value}"))?;
        let keys: BTreeSet<String> = map
            .iter()
            .filter(|(_, v)| v.as_str().is_some_and(|s| s != probe))
            .map(|(k, _)| k.clone())
            .collect();
        candidates = Some(match candidates {
            None => keys,
            Some(previous) => previous.intersection(&keys).cloned().collect(),
        });
    }
    let candidates = candidates.unwrap_or_default();
    if candidates.len() != 1 {
        return Err(format!(
            "expected exactly one cross-representative string discriminator key, found {candidates:?}"
        ));
    }
    Ok(candidates.into_iter().next().expect("length checked"))
}

/// Strict reader for serde's unknown-variant report, which lists the
/// generated variant vocabulary in declaration order. Any other message
/// shape is an error, never a silent pass.
pub fn parse_declared_order(message: &str, probe: &str) -> Result<Vec<String>, String> {
    let marker = format!("unknown variant `{probe}`");
    let rest = message
        .find(&marker)
        .map(|at| &message[at + marker.len()..])
        .ok_or_else(|| {
            format!("serde did not report `{probe}` as an unknown variant: {message}")
        })?;
    let rest = rest.split(" at line").next().unwrap_or(rest);
    let rest = rest
        .trim_start()
        .strip_prefix(", expected ")
        .ok_or_else(|| format!("serde's unknown-variant report has an unknown shape: {message}"))?;
    let rest = rest.strip_prefix("one of ").unwrap_or(rest);
    let names: Vec<String> = rest
        .split(", ")
        .flat_map(|part| part.split(" or "))
        .map(|part| part.trim().to_string())
        .collect();
    for name in &names {
        let ident = name
            .strip_prefix('`')
            .and_then(|n| n.strip_suffix('`'))
            .unwrap_or("");
        if ident.is_empty()
            || !ident
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(format!(
                "serde's unknown-variant report carries an unparsable entry {name:?}: {message}"
            ));
        }
    }
    if names.is_empty() {
        return Err(format!(
            "serde's unknown-variant report carried no variants: {message}"
        ));
    }
    Ok(names
        .into_iter()
        .map(|name| name.trim_matches('`').to_string())
        .collect())
}

impl Generated {
    fn digest_of(files: &[(String, Vec<u8>)]) -> String {
        let mut hasher = Sha256::new();
        for (path, bytes) in files {
            hasher.update(path.as_bytes());
            hasher.update(b"\n");
            hasher.update(bytes);
            hasher.update(b"\n");
        }
        format!("sha256:{}", hex::encode(hasher.finalize()))
    }
}

fn pretty<T: Serialize>(value: &T) -> Result<String, String> {
    let mut text = serde_json::to_string_pretty(value).map_err(|e| format!("serialize: {e}"))?;
    text.push('\n');
    Ok(text)
}

// ------------------------------------------------------------- vocabularies

fn error_kind_rows() -> Vec<(&'static str, faktor_core::error::ErrorKind)> {
    use faktor_core::error::ErrorKind as E;
    use faktor_core::state::AgentState as S;
    exhaustive_reps!(E;
        E::NotFound => ("NotFound", E::NotFound),
        E::Conflict => ("Conflict", E::Conflict),
        E::InvalidState { .. } => ("InvalidState", E::InvalidState { from: S::Idle, to: S::Completed }),
        E::Permission => ("Permission", E::Permission),
        E::Timeout => ("Timeout", E::Timeout),
        E::Cancelled => ("Cancelled", E::Cancelled),
        E::Store => ("Store", E::Store),
        E::Network => ("Network", E::Network),
        E::Provider { .. } => ("Provider", E::Provider { code: "code".to_string(), retryable: true }),
        E::Malformed => ("Malformed", E::Malformed),
        E::Oversized => ("Oversized", E::Oversized),
        E::RateLimited => ("RateLimited", E::RateLimited),
        E::Deadlock => ("Deadlock", E::Deadlock),
        E::Internal => ("Internal", E::Internal),
    )
}

/// The frozen `ErrorKind` vocabulary.
pub fn error_kind_contract() -> Result<ContractFile, String> {
    let order = serde_enum_wire_order::<faktor_core::error::ErrorKind>()?;
    let reps = error_kind_rows()
        .into_iter()
        .map(|(name, kind)| rep(name, &kind, Some(kind.is_retryable()), None))
        .collect::<Result<Vec<_>, _>>()?;
    external_contract("error_kind", "faktor_core::error::ErrorKind", order, reps)
}

/// The compiled `ErrorKind` -> API error envelope mapping.
pub fn error_codes_contract() -> Result<ErrorCodeFile, String> {
    let order = serde_enum_wire_order::<faktor_core::error::ErrorKind>()?;
    let rows = error_kind_rows();
    let mut by_wire: BTreeMap<String, (&'static str, ErrorCodeVariant)> = BTreeMap::new();
    for (name, kind) in rows {
        let api = faktor_protocol::error::from_core(&faktor_core::error::Error::new(
            kind.clone(),
            "frozen-contract probe",
        ));
        let value = serde_json::to_value(&kind).map_err(|e| format!("{name}: serialize: {e}"))?;
        let wire = external_wire(&value)
            .ok_or_else(|| format!("ErrorKind::{name} did not serialize to a single tag"))?;
        let variant = ErrorCodeVariant {
            kind: name.to_string(),
            wire: wire.clone(),
            code: api.code.to_string(),
            http_status: api.http_status,
            retryable: api.retryable,
        };
        if by_wire.insert(wire.clone(), (name, variant)).is_some() {
            return Err(format!("duplicate ErrorKind wire variant '{wire}'"));
        }
    }
    if by_wire.len() != order.len() || order.iter().any(|wire| !by_wire.contains_key(wire)) {
        return Err(format!(
            "error-code mapping does not cover the compiled ErrorKind vocabulary {order:?}"
        ));
    }
    Ok(ErrorCodeFile {
        schema: CONTRACT_SCHEMA.to_string(),
        vocabulary: "core_error_codes".to_string(),
        type_path: "faktor_core::error::ErrorKind".to_string(),
        authority: "faktor_protocol::error::from_core".to_string(),
        variants: order
            .into_iter()
            .map(|wire| by_wire.remove(&wire).expect("presence checked").1)
            .collect(),
    })
}

/// The frozen `ProviderErrorKind` vocabulary.
pub fn provider_error_kind_contract() -> Result<ContractFile, String> {
    use faktor_provider::ProviderErrorKind as P;
    let rows = exhaustive_reps!(P;
        P::Network => ("Network", P::Network),
        P::Timeout => ("Timeout", P::Timeout),
        P::RateLimited => ("RateLimited", P::RateLimited),
        P::BadRequest => ("BadRequest", P::BadRequest),
        P::Auth => ("Auth", P::Auth),
        P::Server => ("Server", P::Server),
        P::Cancelled => ("Cancelled", P::Cancelled),
        P::Malformed => ("Malformed", P::Malformed),
    );
    let order = serde_enum_wire_order::<P>()?;
    let reps = rows
        .into_iter()
        .map(|(name, kind)| rep(name, &kind, Some(kind.retryable()), None))
        .collect::<Result<Vec<_>, _>>()?;
    external_contract(
        "provider_error_kind",
        "faktor_provider::ProviderErrorKind",
        order,
        reps,
    )
}

/// The frozen `AgentState` vocabulary.
pub fn agent_state_contract() -> Result<ContractFile, String> {
    use faktor_core::state::AgentState as S;
    let rows = exhaustive_reps!(S;
        S::Idle => ("Idle", S::Idle),
        S::Preparing => ("Preparing", S::Preparing),
        S::BuildingContext => ("BuildingContext", S::BuildingContext),
        S::WaitingForModel => ("WaitingForModel", S::WaitingForModel),
        S::Streaming => ("Streaming", S::Streaming),
        S::ToolRequested => ("ToolRequested", S::ToolRequested),
        S::WaitingForPermission => ("WaitingForPermission", S::WaitingForPermission),
        S::ExecutingTool => ("ExecutingTool", S::ExecutingTool),
        S::Validating => ("Validating", S::Validating),
        S::UpdatingMemory => ("UpdatingMemory", S::UpdatingMemory),
        S::ReadyForNextTurn => ("ReadyForNextTurn", S::ReadyForNextTurn),
        S::Completed => ("Completed", S::Completed),
        S::Cancelled => ("Cancelled", S::Cancelled),
        S::FailedRecoverable => ("FailedRecoverable", S::FailedRecoverable),
        S::FailedPermanent => ("FailedPermanent", S::FailedPermanent),
        S::NeedsUserInput => ("NeedsUserInput", S::NeedsUserInput),
        S::Suspended => ("Suspended", S::Suspended),
    );
    let order = serde_enum_wire_order::<S>()?;
    let reps = rows
        .into_iter()
        .map(|(name, state)| rep(name, &state, None, None))
        .collect::<Result<Vec<_>, _>>()?;
    external_contract("agent_state", "faktor_core::state::AgentState", order, reps)
}

/// The frozen `TaskState` vocabulary.
pub fn task_state_contract() -> Result<ContractFile, String> {
    use faktor_core::state::TaskState as T;
    let rows = exhaustive_reps!(T;
        T::Pending => ("Pending", T::Pending),
        T::Planning => ("Planning", T::Planning),
        T::Running => ("Running", T::Running),
        T::Waiting => ("Waiting", T::Waiting),
        T::Blocked => ("Blocked", T::Blocked),
        T::NeedsVerification => ("NeedsVerification", T::NeedsVerification),
        T::Verifying => ("Verifying", T::Verifying),
        T::VerifiedComplete => ("VerifiedComplete", T::VerifiedComplete),
        T::Failed => ("Failed", T::Failed),
        T::Cancelled => ("Cancelled", T::Cancelled),
    );
    let order = serde_enum_wire_order::<T>()?;
    let reps = rows
        .into_iter()
        .map(|(name, state)| rep(name, &state, None, None))
        .collect::<Result<Vec<_>, _>>()?;
    external_contract("task_state", "faktor_core::state::TaskState", order, reps)
}

/// The frozen `EventKind` vocabulary.
pub fn event_kind_contract() -> Result<ContractFile, String> {
    use faktor_core::event::EventKind as E;
    let rows = exhaustive_reps!(E;
        E::SessionCreated => ("SessionCreated", E::SessionCreated),
        E::PromptReceived => ("PromptReceived", E::PromptReceived),
        E::ContextPrepared => ("ContextPrepared", E::ContextPrepared),
        E::ModelStarted => ("ModelStarted", E::ModelStarted),
        E::ModelChunkReceived => ("ModelChunkReceived", E::ModelChunkReceived),
        E::ToolRequested => ("ToolRequested", E::ToolRequested),
        E::ToolStarted => ("ToolStarted", E::ToolStarted),
        E::FileChanged => ("FileChanged", E::FileChanged),
        E::ToolCompleted => ("ToolCompleted", E::ToolCompleted),
        E::ToolCancelled => ("ToolCancelled", E::ToolCancelled),
        E::CheckpointCreated => ("CheckpointCreated", E::CheckpointCreated),
        E::ContextCompacted => ("ContextCompacted", E::ContextCompacted),
        E::CompactRejected => ("CompactRejected", E::CompactRejected),
        E::SubagentStarted => ("SubagentStarted", E::SubagentStarted),
        E::SubagentCompleted => ("SubagentCompleted", E::SubagentCompleted),
        E::TurnCompleted => ("TurnCompleted", E::TurnCompleted),
        E::PermissionGranted => ("PermissionGranted", E::PermissionGranted),
        E::PermissionDenied => ("PermissionDenied", E::PermissionDenied),
        E::PermissionExpired => ("PermissionExpired", E::PermissionExpired),
        E::PromptAdmitted => ("PromptAdmitted", E::PromptAdmitted),
        E::PhaseChanged => ("PhaseChanged", E::PhaseChanged),
        E::ReplayStarted => ("ReplayStarted", E::ReplayStarted),
        E::CrashDetected => ("CrashDetected", E::CrashDetected),
        E::RecoveryApplied => ("RecoveryApplied", E::RecoveryApplied),
        E::SessionEnded => ("SessionEnded", E::SessionEnded),
        E::Suspended => ("Suspended", E::Suspended),
        E::Resumed => ("Resumed", E::Resumed),
        E::Failed => ("Failed", E::Failed),
    );
    let order = serde_enum_wire_order::<E>()?;
    let reps = rows
        .into_iter()
        .map(|(name, kind)| rep(name, &kind, None, None))
        .collect::<Result<Vec<_>, _>>()?;
    external_contract("event_kind", "faktor_core::event::EventKind", order, reps)
}

/// The frozen `ReasonCode` vocabulary (`FailureClass`/`ReasonCode` in the
/// audit; the core type is `ReasonCode`).
pub fn reason_code_contract() -> Result<ContractFile, String> {
    use faktor_core::state::ReasonCode as R;
    let rows = exhaustive_reps!(R;
        R::CheckFailed => ("CheckFailed", R::CheckFailed),
        R::CheckUnavailable => ("CheckUnavailable", R::CheckUnavailable),
        R::ReviewBlocked => ("ReviewBlocked", R::ReviewBlocked),
        R::BudgetExceeded => ("BudgetExceeded", R::BudgetExceeded),
        R::SpendOverBudget => ("SpendOverBudget", R::SpendOverBudget),
        R::ChangeBudgetExceeded => ("ChangeBudgetExceeded", R::ChangeBudgetExceeded),
        R::Stalled => ("Stalled", R::Stalled),
        R::LoopDetected => ("LoopDetected", R::LoopDetected),
        R::Cancelled => ("Cancelled", R::Cancelled),
        R::CriteriaMissing => ("CriteriaMissing", R::CriteriaMissing),
        R::CriteriaInconsistent => ("CriteriaInconsistent", R::CriteriaInconsistent),
        R::PatchRevertPatch => ("PatchRevertPatch", R::PatchRevertPatch),
        R::RepeatedEvidenceSet => ("RepeatedEvidenceSet", R::RepeatedEvidenceSet),
        R::UnattributedChange => ("UnattributedChange", R::UnattributedChange),
    );
    let order = serde_enum_wire_order::<R>()?;
    let reps = rows
        .into_iter()
        .map(|(name, reason)| rep(name, &reason, None, Some(reason.code().to_string())))
        .collect::<Result<Vec<_>, _>>()?;
    external_contract("reason_code", "faktor_core::state::ReasonCode", order, reps)
}

/// The native DTO enum discriminators the IDE clients parse
/// (`Part`'s `type` tag).
pub fn native_dto_contract() -> Result<ContractFile, String> {
    use faktor_protocol::native::{Part, ToolResultBody};
    let rows = exhaustive_reps!(Part;
        Part::Text { .. } => ("Text", Part::Text { text: PROBE.to_string() }),
        Part::Reasoning { .. } => ("Reasoning", Part::Reasoning { text: PROBE.to_string() }),
        Part::ToolCall { .. } => ("ToolCall", Part::ToolCall {
            tool_call_id: PROBE.to_string(),
            name: PROBE.to_string(),
            input: Value::Null,
            state: PROBE.to_string(),
        }),
        Part::ToolResult { .. } => ("ToolResult", Part::ToolResult {
            tool_call_id: PROBE.to_string(),
            result: ToolResultBody {
                excerpt: PROBE.to_string(),
                exit_code: None,
                artifact: None,
                slice_hint: None,
            },
        }),
        Part::Summary { .. } => ("Summary", Part::Summary { text: PROBE.to_string() }),
    );
    let values = rows
        .iter()
        .map(|(name, part)| {
            serde_json::to_value(part).map_err(|e| format!("Part::{name}: serialize: {e}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let tag = discover_tag_key(&values, PROBE)?;
    let probe_json = serde_json::json!({ tag.clone(): PROBE_UNKNOWN });
    let report = match serde_json::from_value::<Part>(probe_json) {
        Ok(_) => {
            return Err(
                "faktor_protocol::native::Part: the unknown-tag probe unexpectedly parsed"
                    .to_string(),
            )
        }
        Err(err) => err.to_string(),
    };
    let order = parse_declared_order(&report, PROBE_UNKNOWN)?;

    let mut by_tag: BTreeMap<String, &'static str> = BTreeMap::new();
    for ((name, _), value) in rows.iter().zip(values.iter()) {
        let wire = value
            .get(&tag)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("Part::{name}: serialized value carries no discriminator"))?;
        if wire == PROBE {
            return Err(format!(
                "Part::{name}: discriminator collided with the probe"
            ));
        }
        if by_tag.insert(wire.to_string(), name).is_some() {
            return Err(format!("Part: duplicate discriminator '{wire}'"));
        }
    }
    if by_tag.len() != order.len() || order.iter().any(|wire| !by_tag.contains_key(wire)) {
        return Err(format!(
            "faktor_protocol::native::Part: compiled discriminator order {order:?} does not \
             match the exhaustive representative set {:?}",
            by_tag.keys().collect::<Vec<_>>()
        ));
    }
    Ok(ContractFile {
        schema: CONTRACT_SCHEMA.to_string(),
        vocabulary: "native_dto_tags".to_string(),
        type_path: "faktor_protocol::native::Part".to_string(),
        representation: "internal".to_string(),
        tag: Some(tag),
        authority: None,
        variants: order
            .into_iter()
            .map(|wire| {
                let name = by_tag.remove(&wire).expect("presence checked");
                ContractVariant {
                    name: name.to_string(),
                    wire,
                    retryable: None,
                    code: None,
                }
            })
            .collect(),
    })
}

// ----------------------------------------------------------- render / check

/// Render every canonical file plus the digest manifest.
pub fn render_all() -> Result<Generated, String> {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let contracts: [(&str, Result<ContractFile, String>); 7] = [
        ("agent-state.json", agent_state_contract()),
        ("error-kind.json", error_kind_contract()),
        ("event-kind.json", event_kind_contract()),
        ("native-dto-tags.json", native_dto_contract()),
        ("provider-error-kind.json", provider_error_kind_contract()),
        ("reason-code.json", reason_code_contract()),
        ("task-state.json", task_state_contract()),
    ];
    for (name, contract) in contracts {
        let contract = contract?;
        files.push((
            format!("{CONTRACT_DIR}/{name}"),
            pretty(&contract)?.into_bytes(),
        ));
    }
    files.push((
        format!("{CONTRACT_DIR}/error-codes.json"),
        pretty(&error_codes_contract()?)?.into_bytes(),
    ));
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let digest = Generated::digest_of(&files);
    let manifest = Manifest {
        schema: MANIFEST_SCHEMA,
        generator: GENERATOR,
        algorithm: "sha256",
        digest: digest.clone(),
        files: files
            .iter()
            .map(|(path, bytes)| ManifestFile {
                path: path.clone(),
                sha256: format!("sha256:{}", hex::encode(Sha256::digest(bytes))),
            })
            .collect(),
    };
    files.push((
        format!("{CONTRACT_DIR}/manifest.json"),
        pretty(&manifest)?.into_bytes(),
    ));
    Ok(Generated { files, digest })
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestFile {
    path: String,
    sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    schema: &'static str,
    generator: &'static str,
    algorithm: &'static str,
    digest: String,
    files: Vec<ManifestFile>,
}

fn describe_diff(path: &str, expected: &[u8], actual: &[u8]) -> String {
    let expected_text = String::from_utf8_lossy(expected);
    let actual_text = String::from_utf8_lossy(actual);
    let expected_lines: Vec<&str> = expected_text.lines().collect();
    let actual_lines: Vec<&str> = actual_text.lines().collect();
    for index in 0..expected_lines.len().max(actual_lines.len()) {
        let generated = expected_lines.get(index).copied().unwrap_or("<absent>");
        let frozen = actual_lines.get(index).copied().unwrap_or("<absent>");
        if generated != frozen {
            return format!(
                "{path}: first divergence at line {}:\n  compiled: {generated}\n  frozen:   {frozen}",
                index + 1
            );
        }
    }
    format!(
        "{path}: differs only in bytes (compiled {} vs frozen {} bytes)",
        expected.len(),
        actual.len()
    )
}

/// Compare the compiled vocabularies against the frozen files under `root`.
pub fn check(root: &Path) -> Result<CheckReport, String> {
    let generated = render_all()?;
    let mut mismatches = Vec::new();
    for (rel, expected) in &generated.files {
        let path = root.join(rel);
        match std::fs::read(&path) {
            Ok(actual) if actual == *expected => {}
            Ok(actual) => mismatches.push(Mismatch {
                path: rel.clone(),
                detail: describe_diff(rel, expected, &actual),
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                mismatches.push(Mismatch {
                    path: rel.clone(),
                    detail: format!("{rel}: frozen file is missing (run `write`)"),
                });
            }
            Err(err) => return Err(format!("{rel}: read: {err}")),
        }
    }
    let dir = root.join(CONTRACT_DIR);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let expected: BTreeSet<&str> = generated
            .files
            .iter()
            .map(|(path, _)| path.as_str())
            .collect();
        let mut stale = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".json") {
                continue;
            }
            let rel = format!("{CONTRACT_DIR}/{name}");
            if !expected.contains(rel.as_str()) {
                stale.push(rel);
            }
        }
        stale.sort();
        for rel in stale {
            mismatches.push(Mismatch {
                path: rel.clone(),
                detail: format!("{rel}: not part of the compiled contract vocabulary"),
            });
        }
    }
    Ok(CheckReport {
        ok: mismatches.is_empty(),
        digest: generated.digest,
        checked: generated.files.len(),
        mismatches,
    })
}

/// Write the canonical files under `root` and remove stale frozen files.
pub fn write(root: &Path) -> Result<String, String> {
    let generated = render_all()?;
    let dir = root.join(CONTRACT_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let expected: BTreeSet<&str> = generated
        .files
        .iter()
        .map(|(path, _)| path.as_str())
        .collect();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".json") {
                continue;
            }
            let rel = format!("{CONTRACT_DIR}/{name}");
            if !expected.contains(rel.as_str()) {
                std::fs::remove_file(entry.path())
                    .map_err(|e| format!("{rel}: remove stale: {e}"))?;
            }
        }
    }
    for (rel, bytes) in &generated.files {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(generated.digest)
}

/// The repository root (the workspace that contains `crates/`).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/contracts has a workspace root")
        .to_path_buf()
}

/// The canonical files' repo-relative paths.
pub fn frozen_paths() -> Result<Vec<String>, String> {
    Ok(render_all()?
        .files
        .into_iter()
        .map(|(path, _)| path)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum ScratchBase {
        Alpha,
        Beta,
        Gamma,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum ScratchReordered {
        Gamma,
        Alpha,
        Beta,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum ScratchRenamed {
        Alpha,
        #[serde(rename = "beta_v2")]
        Beta,
        Gamma,
    }

    #[test]
    fn spy_reads_the_compiled_declaration_order() {
        assert_eq!(
            serde_enum_wire_order::<ScratchBase>().unwrap(),
            vec!["alpha", "beta", "gamma"]
        );
    }

    #[test]
    fn compiled_reorders_and_renames_change_the_wire_order() {
        let base = serde_enum_wire_order::<ScratchBase>().unwrap();
        let reordered = serde_enum_wire_order::<ScratchReordered>().unwrap();
        let renamed = serde_enum_wire_order::<ScratchRenamed>().unwrap();
        assert_ne!(base, reordered, "a compiled reorder must change the order");
        assert_eq!(reordered, vec!["gamma", "alpha", "beta"]);
        assert_eq!(renamed, vec!["alpha", "beta_v2", "gamma"]);
    }

    #[test]
    fn declared_order_parser_is_strict() {
        assert_eq!(
            parse_declared_order(
                "unknown variant `probe`, expected `a` or `b` at line 1 column 2",
                "probe"
            )
            .unwrap(),
            vec!["a", "b"]
        );
        assert_eq!(
            parse_declared_order(
                "unknown variant `probe`, expected one of `text`, `tool_call`, `summary`",
                "probe"
            )
            .unwrap(),
            vec!["text", "tool_call", "summary"]
        );
        assert!(
            parse_declared_order(
                "unknown variant `probe`, expected `a` or `b`",
                "someone-else"
            )
            .is_err(),
            "a report about another value must not parse"
        );
        assert!(
            parse_declared_order(
                "invalid type: string \"x\", expected internally tagged enum Part",
                "probe"
            )
            .is_err(),
            "a structural error must not parse as a variant list"
        );
        assert!(
            parse_declared_order("unknown variant `probe`, expected `a b`", "probe").is_err(),
            "an unparsable entry must fail loudly"
        );
    }

    #[test]
    fn real_vocabularies_match_the_compiled_types() {
        let event = event_kind_contract().unwrap();
        assert_eq!(event.variants.len(), 28);
        assert_eq!(event.variants[0].name, "SessionCreated");
        assert_eq!(event.variants[0].wire, "session_created");
        assert_eq!(event.variants[27].name, "Failed");
        assert_eq!(event.variants[27].wire, "failed");

        let agent = agent_state_contract().unwrap();
        assert_eq!(agent.variants.len(), 17);
        assert_eq!(agent.variants[0].wire, "idle");
        assert_eq!(agent.variants[16].wire, "suspended");

        let task = task_state_contract().unwrap();
        assert_eq!(task.variants.len(), 10);
        assert_eq!(task.variants[7].wire, "verified_complete");

        let reason = reason_code_contract().unwrap();
        assert_eq!(reason.variants.len(), 14);
        assert!(reason
            .variants
            .iter()
            .all(|variant| variant.code.as_deref() == Some(variant.wire.as_str())));

        let provider = provider_error_kind_contract().unwrap();
        assert_eq!(provider.variants.len(), 8);
        let retryable: Vec<&str> = provider
            .variants
            .iter()
            .filter(|v| v.retryable == Some(true))
            .map(|v| v.wire.as_str())
            .collect();
        assert_eq!(
            retryable,
            vec!["network", "timeout", "rate_limited", "server"],
            "provider vocabulary: {:?}",
            provider
                .variants
                .iter()
                .map(|v| (v.wire.as_str(), v.retryable))
                .collect::<Vec<_>>()
        );

        let errors = error_kind_contract().unwrap();
        assert_eq!(errors.variants.len(), 14);
        assert_eq!(errors.variants[0].wire, "not_found");
        assert_eq!(errors.variants[8].wire, "provider");

        let codes = error_codes_contract().unwrap();
        assert_eq!(codes.variants.len(), 14);
        assert_eq!(codes.variants[0].code, "not_found");
        assert_eq!(codes.variants[0].http_status, 404);
        assert_eq!(codes.variants[12].code, "deadlock");
        assert_eq!(codes.variants[10].http_status, 413);

        let native = native_dto_contract().unwrap();
        assert_eq!(native.tag.as_deref(), Some("type"));
        assert_eq!(
            native
                .variants
                .iter()
                .map(|v| v.wire.as_str())
                .collect::<Vec<_>>(),
            vec!["text", "reasoning", "tool_call", "tool_result", "summary"]
        );
    }

    #[test]
    fn tagged_discriminators_follow_the_compiled_declaration_order() {
        #[derive(Serialize, Deserialize, Debug)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Tagged {
            Alpha { a: String },
            Beta { b: String },
            Gamma { c: String },
        }

        #[derive(Serialize, Deserialize, Debug)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum TaggedReordered {
            Gamma { c: String },
            Alpha { a: String },
            Beta { b: String },
        }

        fn compiled_order<T>(reps: Vec<T>) -> Vec<String>
        where
            T: Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
        {
            let values: Vec<Value> = reps
                .iter()
                .map(|value| serde_json::to_value(value).expect("serialize"))
                .collect();
            let tag = discover_tag_key(&values, PROBE).expect("discriminator");
            let report = serde_json::from_value::<T>(serde_json::json!({ tag: PROBE_UNKNOWN }))
                .expect_err("the probe tag is unknown")
                .to_string();
            parse_declared_order(&report, PROBE_UNKNOWN).expect("serde variant report")
        }

        assert_eq!(
            compiled_order(vec![
                Tagged::Alpha { a: PROBE.into() },
                Tagged::Beta { b: PROBE.into() },
                Tagged::Gamma { c: PROBE.into() },
            ]),
            vec!["alpha", "beta", "gamma"]
        );
        assert_eq!(
            compiled_order(vec![
                TaggedReordered::Gamma { c: PROBE.into() },
                TaggedReordered::Alpha { a: PROBE.into() },
                TaggedReordered::Beta { b: PROBE.into() },
            ]),
            vec!["gamma", "alpha", "beta"],
            "a compiled reorder of a tagged enum must change the frozen order"
        );
    }

    #[test]
    fn protocol_schema_matches_the_compiled_frozen_contracts() {
        use faktor_protocol::schema::Shape;
        let native = native_dto_contract().unwrap();
        let schema = faktor_protocol::schema::canonical();
        let part = schema
            .types
            .iter()
            .find(|def| def.name == "Part")
            .expect("the protocol schema describes Part");
        match &part.shape {
            Shape::TaggedEnum {
                tag,
                rename_all,
                variants,
            } => {
                assert_eq!(Some(*tag), native.tag.as_deref());
                assert_eq!(*rename_all, "snake_case");
                assert_eq!(
                    variants.iter().map(|v| v.tag).collect::<Vec<_>>(),
                    native
                        .variants
                        .iter()
                        .map(|v| v.wire.as_str())
                        .collect::<Vec<_>>(),
                    "the client-facing schema tags must match the compiled Part discriminators"
                );
            }
            other => panic!("Part must be a tagged enum in the schema: {other:?}"),
        }
        let codes = error_codes_contract().unwrap();
        assert_eq!(schema.constants.error_codes.len(), codes.variants.len());
        for (schema_row, frozen) in schema
            .constants
            .error_codes
            .iter()
            .zip(codes.variants.iter())
        {
            assert_eq!(
                schema_row.core_kind, frozen.wire,
                "the schema's error tag must equal the compiled serde wire name"
            );
            assert_eq!(schema_row.code, frozen.code);
            assert_eq!(schema_row.http_status, frozen.http_status);
            assert_eq!(schema_row.retryable, frozen.retryable);
        }
    }

    #[test]
    fn external_contract_rejects_mismatched_representatives() {
        #[derive(Serialize)]
        #[serde(rename_all = "snake_case")]
        #[allow(dead_code)]
        enum Fake {
            Alpha,
            Beta,
        }
        let order = vec!["alpha".to_string(), "beta".to_string()];
        // Missing representative.
        let err = external_contract(
            "fake",
            "test::Fake",
            order.clone(),
            vec![rep("Alpha", &Fake::Alpha, None, None).unwrap()],
        )
        .unwrap_err();
        assert!(err.contains("does not match"), "{err}");
        // Duplicate representative.
        let err = external_contract(
            "fake",
            "test::Fake",
            order,
            vec![
                rep("Alpha", &Fake::Alpha, None, None).unwrap(),
                rep("AlphaAgain", &Fake::Alpha, None, None).unwrap(),
            ],
        )
        .unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
    }
}
