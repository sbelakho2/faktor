#![allow(clippy::result_large_err)]

//! Typed semantic LSP surface: definition, references, workspace symbols and
//! call hierarchy for Faktor's semantic/index consumers.
//!
//! # Contract
//!
//! - **Capability negotiation only.** A request is issued only when the
//!   `initialize` result advertised the matching provider. An absent/false
//!   provider is [`SemanticError::Unsupported`] — never a fabricated result.
//! - **Every request is bounded and cancellable.** Each method has a caller
//!   supplied deadline ([`SemanticOptions::timeout`], default
//!   [`DEFAULT_SEMANTIC_TIMEOUT`]) and optional
//!   [`CancellationToken`](faktor_core::CancellationToken); cancellation also
//!   sends `$/cancelRequest` for the in-flight id.
//! - **Results are bounded.** Arrays are capped (`MAX_LOCATIONS`,
//!   `MAX_WORKSPACE_SYMBOLS`, `MAX_CALL_HIERARCHY_ITEMS`) with an explicit
//!   `truncated` flag; strings and opaque `data` payloads are length-bounded.
//! - **Caching is revision-keyed.** Definition/references/prepare results are
//!   cached under the tracked document revision
//!   ([`DocumentRevision`] = LSP version + BLAKE3 content hash); call
//!   hierarchy sub-calls are cached under a fingerprint of the exact item
//!   (including opaque `data`). Any `didOpen`/`didChange`/`didClose` clears
//!   the bounded cache because it can change cross-document resolution.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use faktor_core::error::Error;
use faktor_core::CancellationToken;
use serde_json::{json, Value};

use crate::types::{
    Bounded, CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall,
    DocumentRevision, Location, Position, WireError, WorkspaceSymbol, MAX_CALL_HIERARCHY_ITEMS,
    MAX_LOCATIONS, MAX_QUERY_BYTES, MAX_URI_BYTES, MAX_WORKSPACE_SYMBOLS,
};
use crate::{LspClient, RequestFailure};

/// Default deadline for one semantic request (write + response).
pub const DEFAULT_SEMANTIC_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounded cache: maximum entries.
pub const MAX_SEMANTIC_CACHE_ENTRIES: usize = 128;
/// Bounded cache: maximum estimated retained bytes.
pub const MAX_SEMANTIC_CACHE_BYTES: usize = 4 * 1024 * 1024;

pub const METHOD_DEFINITION: &str = "textDocument/definition";
pub const METHOD_REFERENCES: &str = "textDocument/references";
pub const METHOD_WORKSPACE_SYMBOL: &str = "workspace/symbol";
pub const METHOD_PREPARE_CALL_HIERARCHY: &str = "textDocument/prepareCallHierarchy";
pub const METHOD_INCOMING_CALLS: &str = "callHierarchy/incomingCalls";
pub const METHOD_OUTGOING_CALLS: &str = "callHierarchy/outgoingCalls";
pub const METHOD_DOCUMENT_SYMBOL: &str = "textDocument/documentSymbol";

/// One semantic LSP method, used for capability gating and typed errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SemanticMethod {
    Definition,
    References,
    WorkspaceSymbol,
    PrepareCallHierarchy,
    IncomingCalls,
    OutgoingCalls,
    DocumentSymbol,
}

impl SemanticMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Definition => METHOD_DEFINITION,
            Self::References => METHOD_REFERENCES,
            Self::WorkspaceSymbol => METHOD_WORKSPACE_SYMBOL,
            Self::PrepareCallHierarchy => METHOD_PREPARE_CALL_HIERARCHY,
            Self::IncomingCalls => METHOD_INCOMING_CALLS,
            Self::OutgoingCalls => METHOD_OUTGOING_CALLS,
            Self::DocumentSymbol => METHOD_DOCUMENT_SYMBOL,
        }
    }

    fn capability(self) -> Capability {
        match self {
            Self::Definition => Capability::Definition,
            Self::References => Capability::References,
            Self::WorkspaceSymbol => Capability::WorkspaceSymbol,
            Self::PrepareCallHierarchy | Self::IncomingCalls | Self::OutgoingCalls => {
                Capability::CallHierarchy
            }
            Self::DocumentSymbol => Capability::DocumentSymbol,
        }
    }
}

/// Advertised `textDocumentSync.change` kind, used to send a valid
/// `didChange` shape (full text vs one whole-document replacement range).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TextDocumentSyncKind {
    #[default]
    None,
    Full,
    Incremental,
}

/// Typed view of the server's advertised semantic capabilities. Missing or
/// malformed provider entries are `false` — never optimistically enabled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerCapabilities {
    pub definition: bool,
    pub references: bool,
    pub workspace_symbol: bool,
    pub call_hierarchy: bool,
    pub document_symbol: bool,
    pub code_action: bool,
    pub text_document_sync: TextDocumentSyncKind,
    pub server_name: Option<String>,
    pub server_version: Option<String>,
}

impl ServerCapabilities {
    pub fn supports(&self, method: SemanticMethod) -> bool {
        match method.capability() {
            Capability::Definition => self.definition,
            Capability::References => self.references,
            Capability::WorkspaceSymbol => self.workspace_symbol,
            Capability::CallHierarchy => self.call_hierarchy,
            Capability::DocumentSymbol => self.document_symbol,
        }
    }

    pub(crate) fn from_initialize(result: &Value) -> Self {
        let capabilities = result.get("capabilities").unwrap_or(&Value::Null);
        let server_info = result.get("serverInfo");
        let server_name = server_info
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .map(|name| name.chars().take(256).collect());
        let server_version = server_info
            .and_then(|info| info.get("version"))
            .and_then(Value::as_str)
            .map(|version| version.chars().take(256).collect());
        Self {
            definition: provider_enabled(capabilities.get("definitionProvider")),
            references: provider_enabled(capabilities.get("referencesProvider")),
            workspace_symbol: provider_enabled(capabilities.get("workspaceSymbolProvider")),
            call_hierarchy: provider_enabled(capabilities.get("callHierarchyProvider")),
            document_symbol: provider_enabled(capabilities.get("documentSymbolProvider")),
            code_action: provider_enabled(capabilities.get("codeActionProvider")),
            text_document_sync: text_sync_kind(capabilities.get("textDocumentSync")),
            server_name,
            server_version,
        }
    }
}

fn text_sync_kind(value: Option<&Value>) -> TextDocumentSyncKind {
    let raw = match value {
        Some(Value::Number(number)) => number.as_u64(),
        Some(Value::Object(options)) => options.get("change").and_then(Value::as_u64),
        _ => None,
    };
    match raw {
        Some(1) => TextDocumentSyncKind::Full,
        Some(2) => TextDocumentSyncKind::Incremental,
        _ => TextDocumentSyncKind::None,
    }
}

#[derive(Debug, Clone, Copy)]
enum Capability {
    Definition,
    References,
    WorkspaceSymbol,
    CallHierarchy,
    DocumentSymbol,
}

fn provider_enabled(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Bool(true)) | Some(Value::Object(_)))
}

/// Caller controls for one semantic request.
#[derive(Debug, Clone, Copy)]
pub struct SemanticOptions<'a> {
    pub timeout: Duration,
    pub cancellation: Option<&'a CancellationToken>,
}

impl Default for SemanticOptions<'_> {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_SEMANTIC_TIMEOUT,
            cancellation: None,
        }
    }
}

impl<'a> SemanticOptions<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_cancellation(mut self, cancellation: &'a CancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }
}

/// Typed semantic LSP error. Never a bare string: callers can match
/// unsupported, timeout, cancellation, server errors and malformed payloads.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SemanticError {
    #[error("{method} is not supported by the server capabilities")]
    Unsupported { method: String },
    #[error("{method} timed out after {timeout_ms} ms")]
    Timeout { method: String, timeout_ms: u64 },
    #[error("{method} was cancelled")]
    Cancelled { method: String },
    #[error("server error {code} for {method}: {message}")]
    Server {
        method: String,
        code: i64,
        message: String,
        data: Option<Value>,
    },
    #[error("server cancelled {method}")]
    RequestCancelled { method: String },
    #[error("document changed while requesting {method}")]
    ContentModified { method: String },
    #[error("malformed {method} response: {message}")]
    Malformed { method: String, message: String },
    #[error("{method} response exceeded the {bound} bound")]
    Oversized { method: String, bound: usize },
    #[error("transport failure for {method}: {message}")]
    Transport { method: String, message: String },
}

impl SemanticError {
    pub fn is_unsupported(&self) -> bool {
        matches!(self, Self::Unsupported { .. })
    }

    /// Retryability is explicit: timeouts/transport and the LSP
    /// "content modified, reissue" signal are retryable (all semantic
    /// requests are read-only); unsupported/malformed/oversized never are.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Timeout { .. } | Self::Transport { .. } | Self::ContentModified { .. }
        )
    }
}

pub type DefinitionResult = Bounded<Location>;
pub type ReferencesResult = Bounded<Location>;
pub type WorkspaceSymbols = Bounded<WorkspaceSymbol>;
pub type CallHierarchyItems = Bounded<CallHierarchyItem>;
pub type IncomingCalls = Bounded<CallHierarchyIncomingCall>;
pub type OutgoingCalls = Bounded<CallHierarchyOutgoingCall>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SemanticCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub inserts: u64,
    pub evictions: u64,
    pub entries: usize,
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum CacheKey {
    Definition {
        uri: String,
        revision: DocumentRevision,
        position: Position,
    },
    References {
        uri: String,
        revision: DocumentRevision,
        position: Position,
        include_declaration: bool,
    },
    PrepareCallHierarchy {
        uri: String,
        revision: DocumentRevision,
        position: Position,
    },
    IncomingCalls {
        item: [u8; 32],
    },
    OutgoingCalls {
        item: [u8; 32],
    },
    WorkspaceSymbols {
        query: String,
    },
}

#[derive(Debug, Clone)]
enum CachedValue {
    Definition(DefinitionResult),
    References(ReferencesResult),
    PrepareCallHierarchy(CallHierarchyItems),
    IncomingCalls(IncomingCalls),
    OutgoingCalls(OutgoingCalls),
    WorkspaceSymbols(WorkspaceSymbols),
}

struct CacheEntry {
    value: CachedValue,
    bytes: usize,
}

#[derive(Default)]
struct CacheInner {
    map: HashMap<CacheKey, CacheEntry>,
    order: VecDeque<CacheKey>,
    bytes: usize,
    stats: CacheInnerStats,
}

pub(crate) struct SemanticCache {
    inner: Mutex<CacheInner>,
}

impl Default for SemanticCache {
    fn default() -> Self {
        Self {
            inner: Mutex::new(CacheInner::default()),
        }
    }
}

impl SemanticCache {
    fn get(&self, key: &CacheKey) -> Option<CachedValue> {
        let mut inner = crate::recover_lock(&self.inner);
        let cached = inner.map.get(key).map(|entry| entry.value.clone());
        match cached {
            Some(value) => {
                inner.stats.hits += 1;
                Some(value)
            }
            None => {
                inner.stats.misses += 1;
                None
            }
        }
    }

    fn insert(&self, key: CacheKey, value: CachedValue, bytes: usize) {
        if bytes > MAX_SEMANTIC_CACHE_BYTES {
            return;
        }
        let mut inner = crate::recover_lock(&self.inner);
        inner.stats.inserts += 1;
        if let Some(previous) = inner.map.remove(&key) {
            inner.bytes = inner.bytes.saturating_sub(previous.bytes);
        }
        inner.bytes = inner.bytes.saturating_add(bytes);
        inner.map.insert(key.clone(), CacheEntry { value, bytes });
        inner.order.push_back(key);
        while inner.map.len() > MAX_SEMANTIC_CACHE_ENTRIES || inner.bytes > MAX_SEMANTIC_CACHE_BYTES
        {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            if let Some(removed) = inner.map.remove(&oldest) {
                inner.bytes = inner.bytes.saturating_sub(removed.bytes);
                inner.stats.evictions += 1;
            }
        }
    }

    pub(crate) fn clear(&self) {
        let mut inner = crate::recover_lock(&self.inner);
        inner.map.clear();
        inner.order.clear();
        inner.bytes = 0;
    }

    pub(crate) fn stats(&self) -> SemanticCacheStats {
        let inner = crate::recover_lock(&self.inner);
        SemanticCacheStats {
            hits: inner.stats.hits,
            misses: inner.stats.misses,
            inserts: inner.stats.inserts,
            evictions: inner.stats.evictions,
            entries: inner.map.len(),
            bytes: inner.bytes,
        }
    }

    fn definition(&self, key: &CacheKey) -> Option<DefinitionResult> {
        match self.get(key) {
            Some(CachedValue::Definition(value)) => Some(value),
            _ => None,
        }
    }

    fn references(&self, key: &CacheKey) -> Option<ReferencesResult> {
        match self.get(key) {
            Some(CachedValue::References(value)) => Some(value),
            _ => None,
        }
    }

    fn prepare(&self, key: &CacheKey) -> Option<CallHierarchyItems> {
        match self.get(key) {
            Some(CachedValue::PrepareCallHierarchy(value)) => Some(value),
            _ => None,
        }
    }

    fn incoming(&self, key: &CacheKey) -> Option<IncomingCalls> {
        match self.get(key) {
            Some(CachedValue::IncomingCalls(value)) => Some(value),
            _ => None,
        }
    }

    fn outgoing(&self, key: &CacheKey) -> Option<OutgoingCalls> {
        match self.get(key) {
            Some(CachedValue::OutgoingCalls(value)) => Some(value),
            _ => None,
        }
    }

    fn workspace_symbols(&self, key: &CacheKey) -> Option<WorkspaceSymbols> {
        match self.get(key) {
            Some(CachedValue::WorkspaceSymbols(value)) => Some(value),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
struct CacheInnerStats {
    hits: u64,
    misses: u64,
    inserts: u64,
    evictions: u64,
}

/// Convenience methods: defaults can be overridden with the `*_with`
/// variants when a caller needs a shorter deadline or cancellation.
impl LspClient {
    pub fn capabilities(&self) -> ServerCapabilities {
        crate::recover_lock(&self.capabilities).clone()
    }

    /// The revision tracked for an open document, if any. Untracked documents
    /// are not cached (their on-disk revision is unknown to this client).
    pub fn document_revision(&self, uri: &str) -> Option<DocumentRevision> {
        crate::recover_lock(&self.documents)
            .get(uri)
            .map(|document| document.revision)
    }

    pub fn semantic_cache_stats(&self) -> SemanticCacheStats {
        self.semantic_cache.stats()
    }

    pub async fn definition(
        &self,
        uri: &str,
        position: Position,
    ) -> Result<DefinitionResult, SemanticError> {
        self.definition_with(uri, position, SemanticOptions::default())
            .await
    }

    pub async fn definition_with(
        &self,
        uri: &str,
        position: Position,
        options: SemanticOptions<'_>,
    ) -> Result<DefinitionResult, SemanticError> {
        let method = SemanticMethod::Definition;
        validate_uri(method, uri)?;
        let key = self
            .document_revision(uri)
            .map(|revision| CacheKey::Definition {
                uri: uri.to_string(),
                revision,
                position,
            });
        if let Some(key) = key.as_ref() {
            if let Some(cached) = self.semantic_cache.definition(key) {
                return Ok(cached);
            }
        }
        let params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": position.line, "character": position.character },
        });
        let value = self.rpc(method, params, options).await?;
        let result = parse_definition_response(&value).map_err(|error| malformed(method, error))?;
        // An empty result can be a still-indexing server, not a truth to
        // cache; only non-empty answers are retained.
        if let Some(key) = key.filter(|_| !result.is_empty()) {
            let bytes = estimate_locations(&result);
            self.semantic_cache
                .insert(key, CachedValue::Definition(result.clone()), bytes);
        }
        Ok(result)
    }

    pub async fn references(
        &self,
        uri: &str,
        position: Position,
        include_declaration: bool,
    ) -> Result<ReferencesResult, SemanticError> {
        self.references_with(
            uri,
            position,
            include_declaration,
            SemanticOptions::default(),
        )
        .await
    }

    pub async fn references_with(
        &self,
        uri: &str,
        position: Position,
        include_declaration: bool,
        options: SemanticOptions<'_>,
    ) -> Result<ReferencesResult, SemanticError> {
        let method = SemanticMethod::References;
        validate_uri(method, uri)?;
        let key = self
            .document_revision(uri)
            .map(|revision| CacheKey::References {
                uri: uri.to_string(),
                revision,
                position,
                include_declaration,
            });
        if let Some(key) = key.as_ref() {
            if let Some(cached) = self.semantic_cache.references(key) {
                return Ok(cached);
            }
        }
        let params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": position.line, "character": position.character },
            "context": { "includeDeclaration": include_declaration },
        });
        let value = self.rpc(method, params, options).await?;
        let result = parse_references_response(&value).map_err(|error| malformed(method, error))?;
        if let Some(key) = key.filter(|_| !result.is_empty()) {
            let bytes = estimate_locations(&result);
            self.semantic_cache
                .insert(key, CachedValue::References(result.clone()), bytes);
        }
        Ok(result)
    }

    pub async fn workspace_symbols(&self, query: &str) -> Result<WorkspaceSymbols, SemanticError> {
        self.workspace_symbols_with(query, SemanticOptions::default())
            .await
    }

    pub async fn workspace_symbols_with(
        &self,
        query: &str,
        options: SemanticOptions<'_>,
    ) -> Result<WorkspaceSymbols, SemanticError> {
        let method = SemanticMethod::WorkspaceSymbol;
        if query.len() > MAX_QUERY_BYTES {
            return Err(SemanticError::Oversized {
                method: method.as_str().into(),
                bound: MAX_QUERY_BYTES,
            });
        }
        let key = CacheKey::WorkspaceSymbols {
            query: query.to_string(),
        };
        if let Some(cached) = self.semantic_cache.workspace_symbols(&key) {
            return Ok(cached);
        }
        let value = self.rpc(method, json!({ "query": query }), options).await?;
        let result =
            parse_workspace_symbols_response(&value).map_err(|error| malformed(method, error))?;
        if !result.is_empty() {
            self.semantic_cache.insert(
                key,
                CachedValue::WorkspaceSymbols(result.clone()),
                estimate_symbols(&result),
            );
        }
        Ok(result)
    }

    pub async fn prepare_call_hierarchy(
        &self,
        uri: &str,
        position: Position,
    ) -> Result<CallHierarchyItems, SemanticError> {
        self.prepare_call_hierarchy_with(uri, position, SemanticOptions::default())
            .await
    }

    pub async fn prepare_call_hierarchy_with(
        &self,
        uri: &str,
        position: Position,
        options: SemanticOptions<'_>,
    ) -> Result<CallHierarchyItems, SemanticError> {
        let method = SemanticMethod::PrepareCallHierarchy;
        validate_uri(method, uri)?;
        let key = self
            .document_revision(uri)
            .map(|revision| CacheKey::PrepareCallHierarchy {
                uri: uri.to_string(),
                revision,
                position,
            });
        if let Some(key) = key.as_ref() {
            if let Some(cached) = self.semantic_cache.prepare(key) {
                return Ok(cached);
            }
        }
        let params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": position.line, "character": position.character },
        });
        let value = self.rpc(method, params, options).await?;
        let result =
            parse_call_hierarchy_items(&value).map_err(|error| malformed(method, error))?;
        if let Some(key) = key.filter(|_| !result.is_empty()) {
            let bytes = estimate_call_items(&result);
            self.semantic_cache.insert(
                key,
                CachedValue::PrepareCallHierarchy(result.clone()),
                bytes,
            );
        }
        Ok(result)
    }

    pub async fn incoming_calls(
        &self,
        item: &CallHierarchyItem,
    ) -> Result<IncomingCalls, SemanticError> {
        self.incoming_calls_with(item, SemanticOptions::default())
            .await
    }

    pub async fn incoming_calls_with(
        &self,
        item: &CallHierarchyItem,
        options: SemanticOptions<'_>,
    ) -> Result<IncomingCalls, SemanticError> {
        let method = SemanticMethod::IncomingCalls;
        let key = CacheKey::IncomingCalls {
            item: fingerprint_item(item),
        };
        if let Some(cached) = self.semantic_cache.incoming(&key) {
            return Ok(cached);
        }
        let value = self
            .rpc(method, json!({ "item": item.to_wire() }), options)
            .await?;
        let result = parse_incoming_calls(&value).map_err(|error| malformed(method, error))?;
        if !result.is_empty() {
            self.semantic_cache.insert(
                key,
                CachedValue::IncomingCalls(result.clone()),
                estimate_incoming(&result),
            );
        }
        Ok(result)
    }

    pub async fn outgoing_calls(
        &self,
        item: &CallHierarchyItem,
    ) -> Result<OutgoingCalls, SemanticError> {
        self.outgoing_calls_with(item, SemanticOptions::default())
            .await
    }

    pub async fn outgoing_calls_with(
        &self,
        item: &CallHierarchyItem,
        options: SemanticOptions<'_>,
    ) -> Result<OutgoingCalls, SemanticError> {
        let method = SemanticMethod::OutgoingCalls;
        let key = CacheKey::OutgoingCalls {
            item: fingerprint_item(item),
        };
        if let Some(cached) = self.semantic_cache.outgoing(&key) {
            return Ok(cached);
        }
        let value = self
            .rpc(method, json!({ "item": item.to_wire() }), options)
            .await?;
        let result = parse_outgoing_calls(&value).map_err(|error| malformed(method, error))?;
        if !result.is_empty() {
            self.semantic_cache.insert(
                key,
                CachedValue::OutgoingCalls(result.clone()),
                estimate_outgoing(&result),
            );
        }
        Ok(result)
    }

    async fn rpc(
        &self,
        method: SemanticMethod,
        params: Value,
        options: SemanticOptions<'_>,
    ) -> Result<Value, SemanticError> {
        if !self.capabilities().supports(method) {
            return Err(SemanticError::Unsupported {
                method: method.as_str().into(),
            });
        }
        self.request_raw(
            method.as_str(),
            params,
            options.timeout,
            options.cancellation,
        )
        .await
        .map_err(|failure| map_failure(method.as_str(), failure))
    }
}

fn validate_uri(method: SemanticMethod, uri: &str) -> Result<(), SemanticError> {
    if uri.len() > MAX_URI_BYTES {
        return Err(SemanticError::Oversized {
            method: method.as_str().into(),
            bound: MAX_URI_BYTES,
        });
    }
    Ok(())
}

fn malformed(method: SemanticMethod, error: WireError) -> SemanticError {
    SemanticError::Malformed {
        method: method.as_str().into(),
        message: error.0,
    }
}

fn map_failure(method: &str, failure: RequestFailure) -> SemanticError {
    match failure {
        RequestFailure::Exited(message) | RequestFailure::Write(Error { message, .. }) => {
            SemanticError::Transport {
                method: method.into(),
                message,
            }
        }
        RequestFailure::Dropped => SemanticError::Transport {
            method: method.into(),
            message: "lsp connection dropped".into(),
        },
        RequestFailure::Timeout(timeout) => SemanticError::Timeout {
            method: method.into(),
            timeout_ms: timeout.as_millis().min(u64::MAX as u128) as u64,
        },
        RequestFailure::Cancelled => SemanticError::Cancelled {
            method: method.into(),
        },
        RequestFailure::Rpc(error) => {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown JSON-RPC error")
                .to_string();
            match code {
                -32601 => SemanticError::Unsupported {
                    method: method.into(),
                },
                -32600 | -32700 => SemanticError::Malformed {
                    method: method.into(),
                    message,
                },
                -32800 => SemanticError::RequestCancelled {
                    method: method.into(),
                },
                -32801 => SemanticError::ContentModified {
                    method: method.into(),
                },
                _ => SemanticError::Server {
                    method: method.into(),
                    code,
                    message,
                    data: error.get("data").cloned(),
                },
            }
        }
    }
}

fn bounded_array<T>(
    value: &Value,
    limit: usize,
    convert: impl Fn(&Value) -> Result<T, WireError>,
) -> Result<Bounded<T>, WireError> {
    match value {
        Value::Null => Ok(Bounded::empty()),
        Value::Array(items) => {
            let truncated = items.len() > limit;
            let mut converted = Vec::with_capacity(items.len().min(limit));
            for item in items.iter().take(limit) {
                converted.push(convert(item)?);
            }
            Ok(Bounded::new(converted, truncated))
        }
        _ => Err(WireError("expected an array or null".into())),
    }
}

pub(crate) fn parse_definition_response(value: &Value) -> Result<DefinitionResult, WireError> {
    match value {
        Value::Null => Ok(Bounded::empty()),
        Value::Object(_) => Ok(Bounded::new(vec![Location::from_definition(value)?], false)),
        Value::Array(_) => bounded_array(value, MAX_LOCATIONS, Location::from_definition),
        _ => Err(WireError(
            "definition must be a Location, LocationLink, array or null".into(),
        )),
    }
}

pub(crate) fn parse_references_response(value: &Value) -> Result<ReferencesResult, WireError> {
    match value {
        Value::Null => Ok(Bounded::empty()),
        Value::Array(_) => bounded_array(value, MAX_LOCATIONS, Location::from_reference),
        _ => Err(WireError("references must be an array or null".into())),
    }
}

pub(crate) fn parse_workspace_symbols_response(
    value: &Value,
) -> Result<WorkspaceSymbols, WireError> {
    match value {
        Value::Null => Ok(Bounded::empty()),
        Value::Array(_) => bounded_array(value, MAX_WORKSPACE_SYMBOLS, WorkspaceSymbol::from_value),
        _ => Err(WireError(
            "workspace symbols must be an array or null".into(),
        )),
    }
}

pub(crate) fn parse_call_hierarchy_items(value: &Value) -> Result<CallHierarchyItems, WireError> {
    match value {
        Value::Null => Ok(Bounded::empty()),
        Value::Array(_) => bounded_array(
            value,
            MAX_CALL_HIERARCHY_ITEMS,
            CallHierarchyItem::from_value,
        ),
        _ => Err(WireError(
            "call hierarchy items must be an array or null".into(),
        )),
    }
}

pub(crate) fn parse_incoming_calls(value: &Value) -> Result<IncomingCalls, WireError> {
    match value {
        Value::Null => Ok(Bounded::empty()),
        Value::Array(_) => bounded_array(
            value,
            MAX_CALL_HIERARCHY_ITEMS,
            CallHierarchyIncomingCall::from_value,
        ),
        _ => Err(WireError("incoming calls must be an array or null".into())),
    }
}

pub(crate) fn parse_outgoing_calls(value: &Value) -> Result<OutgoingCalls, WireError> {
    match value {
        Value::Null => Ok(Bounded::empty()),
        Value::Array(_) => bounded_array(
            value,
            MAX_CALL_HIERARCHY_ITEMS,
            CallHierarchyOutgoingCall::from_value,
        ),
        _ => Err(WireError("outgoing calls must be an array or null".into())),
    }
}

fn fingerprint_item(item: &CallHierarchyItem) -> [u8; 32] {
    let wire = item.to_wire();
    match serde_json::to_vec(&wire) {
        Ok(bytes) => *blake3::hash(&bytes).as_bytes(),
        Err(_) => {
            let mut fallback = format!(
                "{}:{}:{}:{}:{}",
                item.uri,
                item.range.start.line,
                item.range.start.character,
                item.name,
                item.kind.as_u32()
            );
            fallback.push_str(&format!(":{}", item.detail.as_deref().unwrap_or("")));
            *blake3::hash(fallback.as_bytes()).as_bytes()
        }
    }
}

fn estimate_locations(result: &DefinitionResult) -> usize {
    result
        .items
        .iter()
        .map(|location| location.uri.len().saturating_add(64))
        .sum::<usize>()
        .saturating_add(64)
}

fn estimate_symbols(result: &WorkspaceSymbols) -> usize {
    result
        .items
        .iter()
        .map(|symbol| {
            symbol
                .name
                .len()
                .saturating_add(symbol.location.uri.len())
                .saturating_add(symbol.container_name.as_ref().map_or(0, String::len))
                .saturating_add(
                    symbol
                        .data
                        .as_ref()
                        .and_then(|data| serde_json::to_vec(data).ok())
                        .map_or(0, |bytes| bytes.len()),
                )
                .saturating_add(96)
        })
        .sum::<usize>()
        .saturating_add(64)
}

fn estimate_call_items(result: &CallHierarchyItems) -> usize {
    result
        .items
        .iter()
        .map(estimate_item)
        .sum::<usize>()
        .saturating_add(64)
}

fn estimate_item(item: &CallHierarchyItem) -> usize {
    item.name
        .len()
        .saturating_add(item.uri.len())
        .saturating_add(item.detail.as_ref().map_or(0, String::len))
        .saturating_add(
            item.data
                .as_ref()
                .and_then(|data| serde_json::to_vec(data).ok())
                .map_or(0, |bytes| bytes.len()),
        )
        .saturating_add(96)
}

fn estimate_incoming(result: &IncomingCalls) -> usize {
    result
        .items
        .iter()
        .map(|call| {
            estimate_item(&call.from).saturating_add(call.from_ranges.len().saturating_mul(24))
        })
        .sum::<usize>()
        .saturating_add(64)
}

fn estimate_outgoing(result: &OutgoingCalls) -> usize {
    result
        .items
        .iter()
        .map(|call| {
            estimate_item(&call.to).saturating_add(call.from_ranges.len().saturating_mul(24))
        })
        .sum::<usize>()
        .saturating_add(64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{SymbolKind, SymbolTag, MAX_DATA_BYTES};

    fn location_json(line: u64) -> Value {
        json!({
            "uri": "file:///src/lib.rs",
            "range": {
                "start": { "line": line, "character": 4 },
                "end": { "line": line, "character": 12 }
            }
        })
    }

    #[test]
    fn transport_and_jsonrpc_errors_map_to_typed_variants() {
        assert_eq!(
            map_failure(
                METHOD_DEFINITION,
                RequestFailure::Rpc(json!({"code": -32601, "message": "method not found"}))
            ),
            SemanticError::Unsupported {
                method: METHOD_DEFINITION.into()
            }
        );
        let modified = map_failure(
            METHOD_REFERENCES,
            RequestFailure::Rpc(json!({"code": -32801, "message": "changed"})),
        );
        assert_eq!(
            modified,
            SemanticError::ContentModified {
                method: METHOD_REFERENCES.into()
            }
        );
        assert!(modified.is_retryable());
        assert_eq!(
            map_failure(
                METHOD_WORKSPACE_SYMBOL,
                RequestFailure::Rpc(json!({"code": -32800, "message": "cancelled"}))
            ),
            SemanticError::RequestCancelled {
                method: METHOD_WORKSPACE_SYMBOL.into()
            }
        );
        assert_eq!(
            map_failure(
                METHOD_DEFINITION,
                RequestFailure::Rpc(json!({"code": -32602, "message": "bad params"}))
            ),
            SemanticError::Server {
                method: METHOD_DEFINITION.into(),
                code: -32602,
                message: "bad params".into(),
                data: None,
            }
        );
        assert_eq!(
            map_failure(
                METHOD_DEFINITION,
                RequestFailure::Timeout(Duration::from_millis(1500))
            ),
            SemanticError::Timeout {
                method: METHOD_DEFINITION.into(),
                timeout_ms: 1500,
            }
        );
        assert_eq!(
            map_failure(METHOD_DEFINITION, RequestFailure::Cancelled),
            SemanticError::Cancelled {
                method: METHOD_DEFINITION.into()
            }
        );
        assert_eq!(
            map_failure(
                METHOD_DEFINITION,
                RequestFailure::Write(Error::timeout("deadline"))
            ),
            SemanticError::Transport {
                method: METHOD_DEFINITION.into(),
                message: "deadline".into(),
            }
        );
    }

    #[test]
    fn provider_capabilities_are_never_fabricated() {
        let result = json!({"capabilities": {}});
        let caps = ServerCapabilities::from_initialize(&result);
        for method in [
            SemanticMethod::Definition,
            SemanticMethod::References,
            SemanticMethod::WorkspaceSymbol,
            SemanticMethod::PrepareCallHierarchy,
            SemanticMethod::IncomingCalls,
            SemanticMethod::OutgoingCalls,
            SemanticMethod::DocumentSymbol,
        ] {
            assert!(!caps.supports(method), "{method:?} must be unsupported");
        }
        // Object forms (options) are supported; false/strings are not.
        let result = json!({"capabilities": {
            "definitionProvider": {"workDoneProgress": true},
            "referencesProvider": false,
            "workspaceSymbolProvider": "yes",
            "callHierarchyProvider": true,
            "documentSymbolProvider": true
        }});
        let caps = ServerCapabilities::from_initialize(&result);
        assert!(caps.definition);
        assert!(!caps.references);
        assert!(!caps.workspace_symbol);
        assert!(caps.call_hierarchy);
        assert!(caps.document_symbol);
        assert_eq!(caps.text_document_sync, TextDocumentSyncKind::None);

        let numeric = ServerCapabilities::from_initialize(&json!({
            "capabilities": { "textDocumentSync": 2 }
        }));
        assert_eq!(
            numeric.text_document_sync,
            TextDocumentSyncKind::Incremental
        );
        let options = ServerCapabilities::from_initialize(&json!({
            "capabilities": { "textDocumentSync": { "change": 1, "openClose": true } }
        }));
        assert_eq!(options.text_document_sync, TextDocumentSyncKind::Full);
    }

    #[test]
    fn definition_parses_locations_and_links() {
        let plain = json!({
            "uri": "file:///a.rs",
            "range": {"start": {"line": 3, "character": 1}, "end": {"line": 3, "character": 9}}
        });
        let parsed = parse_definition_response(&plain).unwrap();
        assert_eq!(parsed.items.len(), 1);
        assert_eq!(parsed.items[0].selection_range, parsed.items[0].range);
        assert!(parsed.items[0].origin_selection_range.is_none());

        let link = json!({
            "targetUri": "file:///b.rs",
            "targetRange": {"start": {"line": 1, "character": 0}, "end": {"line": 5, "character": 1}},
            "targetSelectionRange": {"start": {"line": 2, "character": 4}, "end": {"line": 2, "character": 8}},
            "originSelectionRange": {"start": {"line": 9, "character": 2}, "end": {"line": 9, "character": 6}}
        });
        let parsed = parse_definition_response(&json!([link])).unwrap();
        assert_eq!(parsed.items[0].uri, "file:///b.rs");
        assert_eq!(parsed.items[0].selection_range.start.line, 2);
        assert_eq!(
            parsed.items[0].origin_selection_range.unwrap().start.line,
            9
        );
    }

    #[test]
    fn hostile_results_are_malformed_not_panics() {
        let bad = [
            json!("not a location"),
            json!({"uri": 42, "range": {}}),
            json!({"uri": "file:///a", "range": {
                "start": {"line": 9, "character": 0},
                "end": {"line": 1, "character": 0}
            }}),
            json!({"uri": "file:///a", "range": {
                "start": {"line": -1, "character": 0},
                "end": {"line": 1, "character": 0}
            }}),
        ];
        for value in bad {
            assert!(
                parse_definition_response(&value).is_err(),
                "hostile value must be refused: {value}"
            );
        }
        assert!(parse_references_response(&json!({"uri": "x"})).is_err());
        assert!(parse_workspace_symbols_response(&json!([{"name": "x"}])).is_err());
        assert!(parse_incoming_calls(&json!([[1, 2]])).is_err());
    }

    #[test]
    fn oversized_arrays_are_capped_and_flagged() {
        let items: Vec<Value> = (0..(MAX_LOCATIONS + 17))
            .map(|line| location_json(line as u64))
            .collect();
        let parsed = parse_references_response(&json!(items)).unwrap();
        assert_eq!(parsed.items.len(), MAX_LOCATIONS);
        assert!(parsed.truncated);
        assert_eq!(parsed.items[0].range.start.line, 0);

        let symbols: Vec<Value> = (0..(MAX_WORKSPACE_SYMBOLS + 1))
            .map(|i| {
                json!({
                    "name": format!("symbol_{i}"),
                    "kind": 12,
                    "location": { "uri": "file:///a.rs" }
                })
            })
            .collect();
        let parsed = parse_workspace_symbols_response(&json!(symbols)).unwrap();
        assert_eq!(parsed.items.len(), MAX_WORKSPACE_SYMBOLS);
        assert!(parsed.truncated);
        assert!(parsed.items[0].location.range.is_none());
    }

    #[test]
    fn opaque_data_and_strings_are_bounded() {
        let mut symbol = json!({
            "name": "x",
            "kind": 12,
            "location": { "uri": "file:///a.rs" }
        });
        symbol["data"] = Value::String("d".repeat(MAX_DATA_BYTES + 1));
        assert!(WorkspaceSymbol::from_value(&symbol).is_err());

        let long_uri = format!("file:///{}", "a".repeat(MAX_URI_BYTES));
        let value = json!({
            "uri": long_uri,
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}
        });
        assert!(Location::from_reference(&value).is_err());
    }

    #[test]
    fn symbol_kinds_and_tags_roundtrip_unknown_values() {
        assert_eq!(SymbolKind::from_u32(23), SymbolKind::Struct);
        assert_eq!(SymbolKind::from_u32(999), SymbolKind::Unknown(999));
        assert_eq!(SymbolKind::Unknown(999).as_u32(), 999);
        let encoded = serde_json::to_value(SymbolKind::Unknown(999)).unwrap();
        assert_eq!(encoded, json!(999));
        let decoded: SymbolKind = serde_json::from_value(json!(26)).unwrap();
        assert_eq!(decoded, SymbolKind::TypeParameter);
        assert_eq!(SymbolTag::from(1), SymbolTag::Deprecated);
    }

    #[test]
    fn cache_is_bounded_and_evicts_fifo() {
        let cache = SemanticCache::default();
        for i in 0..(MAX_SEMANTIC_CACHE_ENTRIES + 32) {
            let key = CacheKey::WorkspaceSymbols {
                query: format!("q{i}"),
            };
            cache.insert(
                key,
                CachedValue::WorkspaceSymbols(WorkspaceSymbols::empty()),
                16,
            );
        }
        let stats = cache.stats();
        assert_eq!(stats.entries, MAX_SEMANTIC_CACHE_ENTRIES);
        assert_eq!(stats.evictions, 32);
        assert!(stats.bytes <= MAX_SEMANTIC_CACHE_BYTES);
    }

    #[test]
    fn cache_never_stores_a_single_entry_past_the_byte_cap() {
        let cache = SemanticCache::default();
        cache.insert(
            CacheKey::WorkspaceSymbols { query: "q".into() },
            CachedValue::WorkspaceSymbols(WorkspaceSymbols::empty()),
            MAX_SEMANTIC_CACHE_BYTES + 1,
        );
        assert_eq!(cache.stats().entries, 0);
    }
}
