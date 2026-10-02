// GENERATED FILE - DO NOT EDIT BY HAND.
// Source: crates/protocol/schema/faktor-protocol.schema.json (schema faktor-protocol-schema/v1)
// Regenerate: node scripts/protocol-codegen.mjs --write
// The handwritten behavior/UI code that consumes these DTOs is
// NOT generated; see crates/protocol/schema/CODEGEN.md.


export type ProtocolJson =
  | null
  | boolean
  | number
  | string
  | ProtocolJson[]
  | { [key: string]: ProtocolJson };

export class ProtocolDtoError extends Error {
  readonly path: string;
  readonly detail: string;

  constructor(path: string, detail: string) {
    super(`protocol DTO violation at ${path}: ${detail}`);
    this.name = 'ProtocolDtoError';
    this.path = path;
    this.detail = detail;
  }
}

type ProtocolJsonObject = { [key: string]: ProtocolJson };

function dtoFail(path: string, detail: string): never {
  throw new ProtocolDtoError(path, detail);
}

function dtoDescribe(value: ProtocolJson): string {
  if (value === null) return 'null';
  if (Array.isArray(value)) return 'an array';
  return typeof value;
}

function dtoObject(value: ProtocolJson, path: string): ProtocolJsonObject {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    dtoFail(path, `expected an object, got ${dtoDescribe(value)}`);
  }
  return value as ProtocolJsonObject;
}

function dtoRequired(object: ProtocolJsonObject, key: string, path: string): ProtocolJson {
  if (!Object.prototype.hasOwnProperty.call(object, key)) {
    dtoFail(path, `missing required field ${key}`);
  }
  return object[key] as ProtocolJson;
}

function dtoOptional(object: ProtocolJsonObject, key: string): ProtocolJson | undefined {
  if (!Object.prototype.hasOwnProperty.call(object, key)) return undefined;
  return object[key] as ProtocolJson;
}

function dtoRejectUnknown(
  object: ProtocolJsonObject,
  path: string,
  allowed: readonly string[],
): void {
  for (const key of Object.keys(object)) {
    if (!allowed.includes(key)) {
      dtoFail(path, `unknown field ${key}`);
    }
  }
}

function dtoString(object: ProtocolJsonObject, key: string, path: string): string {
  const value = dtoRequired(object, key, path);
  if (typeof value !== 'string') {
    dtoFail(`${path}.${key}`, `expected a string, got ${dtoDescribe(value)}`);
  }
  return value;
}

function dtoStringElement(value: ProtocolJson, path: string): string {
  if (typeof value !== 'string') {
    dtoFail(path, `expected a string, got ${dtoDescribe(value)}`);
  }
  return value;
}

function dtoBool(object: ProtocolJsonObject, key: string, path: string): boolean {
  const value = dtoRequired(object, key, path);
  if (typeof value !== 'boolean') {
    dtoFail(`${path}.${key}`, `expected a boolean, got ${dtoDescribe(value)}`);
  }
  return value;
}

function dtoI64(object: ProtocolJsonObject, key: string, path: string): number {
  const value = dtoRequired(object, key, path);
  if (typeof value !== 'number' || !Number.isSafeInteger(value)) {
    dtoFail(`${path}.${key}`, `expected a safe integer, got ${dtoDescribe(value)}`);
  }
  return value;
}

function dtoList(object: ProtocolJsonObject, key: string, path: string): ProtocolJson[] {
  const value = dtoRequired(object, key, path);
  if (!Array.isArray(value)) {
    dtoFail(`${path}.${key}`, `expected an array, got ${dtoDescribe(value)}`);
  }
  return value;
}

function dtoNullableString(
  object: ProtocolJsonObject,
  key: string,
  path: string,
): string | null {
  const value = dtoRequired(object, key, path);
  if (value === null) return null;
  if (typeof value !== 'string') {
    dtoFail(`${path}.${key}`, `expected a string or null, got ${dtoDescribe(value)}`);
  }
  return value;
}

function dtoNullableI64(
  object: ProtocolJsonObject,
  key: string,
  path: string,
): number | null {
  const value = dtoRequired(object, key, path);
  if (value === null) return null;
  if (typeof value !== 'number' || !Number.isSafeInteger(value)) {
    dtoFail(`${path}.${key}`, `expected a safe integer or null, got ${dtoDescribe(value)}`);
  }
  return value;
}

function dtoNullableI32(
  object: ProtocolJsonObject,
  key: string,
  path: string,
): number | null {
  const value = dtoNullableI64(object, key, path);
  if (value !== null && (value < -2147483648 || value > 2147483647)) {
    dtoFail(`${path}.${key}`, `integer ${value} exceeds i32 range`);
  }
  return value;
}

function dtoOptionalNullableI64(
  object: ProtocolJsonObject,
  key: string,
  path: string,
): number | null {
  const value = dtoOptional(object, key);
  if (value === undefined || value === null) return null;
  if (typeof value !== 'number' || !Number.isSafeInteger(value)) {
    dtoFail(`${path}.${key}`, `expected a safe integer or null, got ${dtoDescribe(value)}`);
  }
  return value;
}

function dtoOptionalNullableString(
  object: ProtocolJsonObject,
  key: string,
  path: string,
): string | null {
  const value = dtoOptional(object, key);
  if (value === undefined || value === null) return null;
  if (typeof value !== 'string') {
    dtoFail(`${path}.${key}`, `expected a string or null, got ${dtoDescribe(value)}`);
  }
  return value;
}

function dtoOptionalNullableJson(object: ProtocolJsonObject, key: string): ProtocolJson | null {
  const value = dtoOptional(object, key);
  return value === undefined ? null : value;
}

function dtoOptionalNullableList(
  object: ProtocolJsonObject,
  key: string,
  path: string,
): ProtocolJson[] | null {
  const value = dtoOptional(object, key);
  if (value === undefined || value === null) return null;
  if (!Array.isArray(value)) {
    dtoFail(`${path}.${key}`, `expected an array or null, got ${dtoDescribe(value)}`);
  }
  return value;
}

// --------------------------------------------------------- DTO shapes

/** One conversation message row (parts nested). (unknown_fields: ignore) */
export interface ProtocolMessage {
  readonly id: string;
  readonly role: string;
  readonly session_id: string;
  readonly seq: number;
  readonly created_ms: number;
  readonly parts: readonly ProtocolPart[];
}

/** A typed message part: text, reasoning, tool call, tool result or summary. (unknown_fields: reject) */
export type ProtocolPart =
  | { readonly type: "text"; readonly text: string }
  | { readonly type: "reasoning"; readonly text: string }
  | { readonly type: "tool_call"; readonly tool_call_id: string; readonly name: string; readonly input: ProtocolJson; readonly state: string }
  | { readonly type: "tool_result"; readonly tool_call_id: string; readonly result: ProtocolToolResultBody }
  | { readonly type: "summary"; readonly text: string };

/** Bounded tool result: excerpt, exit code, artifact pointer and slice hint. (unknown_fields: reject) */
export interface ProtocolToolResultBody {
  readonly excerpt: string;
  readonly exit_code: number | null;
  readonly artifact: string | null;
  readonly slice_hint: string | null;
}

/** Additive paging metadata: applied size, next cursor, has_more, total estimate. (unknown_fields: ignore) */
export interface ProtocolPageMeta {
  readonly size: number;
  readonly cursor: number | null;
  readonly has_more: boolean;
  readonly total_estimate: number | null;
}

/** One bounded page of conversation messages. (unknown_fields: ignore) */
export interface ProtocolMessagesPage {
  readonly session_id: string;
  readonly messages: readonly ProtocolMessage[];
  readonly has_more: boolean;
  readonly next_before: number | null;
  readonly page: ProtocolPageMeta;
}

/** The durable session state projection. (unknown_fields: ignore) */
export interface ProtocolSessionState {
  readonly session_id: string;
  readonly state: string;
  readonly title: string;
  readonly last_event_seq: number;
  readonly agent_state: ProtocolAgentStateView;
  readonly task_ledger: ProtocolJson | null;
}

/** The agent state machine view folded into a session state row. (unknown_fields: ignore) */
export interface ProtocolAgentStateView {
  readonly state: string;
  readonly label: string;
  readonly active: boolean;
  readonly terminal: boolean;
}

/** One durable typed binary attachment: BLAKE3 digest (64 lowercase hex), mime, optional original filename and decompressed size. Decoders ignore unknown fields (the native additive-response contract); the daemon's own request DTO rejects them. (unknown_fields: ignore) */
export interface ProtocolAttachmentId {
  readonly digest: string;
  readonly mime: string;
  readonly filename: string | null;
  readonly size: number;
}

/** One durable attachment REFERENCE (native additive-response contract): the surrogate reference id the retrieval routes address, plus the CAS digest and the exact presentation metadata (mime, optional filename, decompressed size). Decoders ignore unknown fields. (unknown_fields: ignore) */
export interface ProtocolAttachmentRef {
  readonly ref_id: number;
  readonly digest: string;
  readonly mime: string;
  readonly filename: string | null;
  readonly size: number;
}

/** Strict request body of POST /native/session/{id}/attachments: canonical standard base64 bytes plus the declared mime. (unknown_fields: reject) */
export interface ProtocolAttachmentUpload {
  readonly mime: string;
  readonly filename: string | null;
  readonly data_base64: string;
}

/** One durable task run projection (native task-run list/state reads; additive: decoders ignore unknown fields). (unknown_fields: ignore) */
export interface ProtocolTaskRun {
  readonly task_id: number;
  readonly run_id: string;
  readonly mode: string;
  readonly state: string;
  readonly goal: string | null;
  readonly item_ids: readonly string[];
  readonly model: string | null;
}

/** Response of a native task-run start: the durable run identity and its current state (additive: unknown fields ignored). (unknown_fields: ignore) */
export interface ProtocolTaskRunStarted {
  readonly task_id: number;
  readonly run_id: string;
  readonly state: string;
}

/** Response of a native task-run cancel (additive: unknown fields ignored). (unknown_fields: ignore) */
export interface ProtocolTaskRunCancelled {
  readonly run_id: string;
  readonly cancelled: boolean;
}

/** One wire work item of a native task-run start (the orchestrator's own JSON vocabulary). (unknown_fields: reject) */
export interface ProtocolTaskRunWorkItem {
  readonly id: string;
  readonly kind: string;
  readonly summary: string | null;
  readonly depends_on: readonly string[] | null;
  readonly acceptance_checks: readonly string[] | null;
  readonly ownership: ProtocolJson | null;
  readonly required_capabilities: ProtocolJson | null;
}

/** Strict request body of POST /native/session/{id}/task-runs. The daemon validates every member strictly; money accepts a decimal string or a lossless JSON integer and is documented as json here. `submission_id` is the required client submission UUID of the logical start (idempotency key: a repeated key replays the original run receipt byte-for-byte). (unknown_fields: reject) */
export interface ProtocolTaskRunStartRequest {
  readonly goal: string;
  readonly submission_id: string;
  readonly criteria: readonly string[] | null;
  readonly work_items: readonly ProtocolTaskRunWorkItem[] | null;
  readonly ownership: ProtocolJson | null;
  readonly model: string | null;
  readonly max_tokens: number | null;
  readonly max_cost_micro: ProtocolJson | null;
  readonly mutation_mode: string | null;
  readonly routing_mode: string | null;
  readonly files: readonly string[] | null;
  readonly attachments: readonly ProtocolAttachmentId[] | null;
  readonly completion_contract: ProtocolJson | null;
}

/** Strict request body of POST /native/session/{id}/prompt. `submission_id` is the required client submission UUID of the logical prompt (idempotency key: a repeated key with the equal normalized body returns the original receipt byte-for-byte; a different body under the same key is a typed conflict). The daemon validates every member strictly (`deny_unknown_fields`; an unknown field or typo is a 400). (unknown_fields: reject) */
export interface ProtocolSessionPromptRequest {
  readonly session_id: string;
  readonly submission_id: string;
  readonly prompt: string;
  readonly files: readonly string[] | null;
}

/** Response of a native prompt: the accepted in-session run identity and queued flag (additive: unknown fields ignored). A submission-keyed replay returns the stored receipt byte-for-byte. (unknown_fields: ignore) */
export interface ProtocolSessionPromptReceipt {
  readonly op_id: string;
  readonly run_id: string;
  readonly accepted: boolean;
  readonly queued: boolean;
}

// ------------------------------------------------------------ defaults

function dtoDefaultProtocolPageMeta(): ProtocolPageMeta {
  return {
    size: 0,
    cursor: null,
    has_more: false,
    total_estimate: null,
  };
}

// ---------------------------------------------------------- validators

export function validateProtocolMessage(value: ProtocolJson, path = "ProtocolMessage"): ProtocolMessage {
  const object = dtoObject(value, path);
  const required = ["id","role","session_id","seq","created_ms","parts"];
  for (const key of required) {
    dtoRequired(object, key, path);
  }
  return {
    id: dtoString(object, "id", path),
    role: dtoString(object, "role", path),
    session_id: dtoString(object, "session_id", path),
    seq: dtoI64(object, "seq", path),
    created_ms: dtoI64(object, "created_ms", path),
    parts: dtoList(object, "parts", path).map((item, index) => validateProtocolPart(item, path + ".parts[" + index + "]")),
  };
}

export function validateProtocolPart(value: ProtocolJson, path = "ProtocolPart"): ProtocolPart {
  const object = dtoObject(value, path);
  const tag = dtoString(object, "type", path);
  switch (tag) {
    case "text": {
      dtoRejectUnknown(object, path, ["type","text"]);
      return {
        type: "text",
        text: dtoString(object, "text", path),
      };
    }
    case "reasoning": {
      dtoRejectUnknown(object, path, ["type","text"]);
      return {
        type: "reasoning",
        text: dtoString(object, "text", path),
      };
    }
    case "tool_call": {
      dtoRejectUnknown(object, path, ["type","tool_call_id","name","input","state"]);
      return {
        type: "tool_call",
        tool_call_id: dtoString(object, "tool_call_id", path),
        name: dtoString(object, "name", path),
        input: dtoRequired(object, "input", path),
        state: dtoString(object, "state", path),
      };
    }
    case "tool_result": {
      dtoRejectUnknown(object, path, ["type","tool_call_id","result"]);
      return {
        type: "tool_result",
        tool_call_id: dtoString(object, "tool_call_id", path),
        result: validateProtocolToolResultBody(dtoRequired(object, "result", path), path + ".result"),
      };
    }
    case "summary": {
      dtoRejectUnknown(object, path, ["type","text"]);
      return {
        type: "summary",
        text: dtoString(object, "text", path),
      };
    }
    default:
      dtoFail(path, "unknown Part type " + tag);
      return null as unknown as ProtocolPart;
  }
}

export function validateProtocolToolResultBody(value: ProtocolJson, path = "ProtocolToolResultBody"): ProtocolToolResultBody {
  const object = dtoObject(value, path);
  const required = ["excerpt","exit_code","artifact","slice_hint"];
  for (const key of required) {
    dtoRequired(object, key, path);
  }
  dtoRejectUnknown(object, path, ["excerpt","exit_code","artifact","slice_hint"]);
  return {
    excerpt: dtoString(object, "excerpt", path),
    exit_code: dtoNullableI32(object, "exit_code", path),
    artifact: dtoNullableString(object, "artifact", path),
    slice_hint: dtoNullableString(object, "slice_hint", path),
  };
}

export function validateProtocolPageMeta(value: ProtocolJson, path = "ProtocolPageMeta"): ProtocolPageMeta {
  const object = dtoObject(value, path);
  const required = ["size","cursor","has_more"];
  for (const key of required) {
    dtoRequired(object, key, path);
  }
  return {
    size: dtoI64(object, "size", path),
    cursor: dtoNullableI64(object, "cursor", path),
    has_more: dtoBool(object, "has_more", path),
    total_estimate: dtoOptionalNullableI64(object, "total_estimate", path),
  };
}

export function validateProtocolMessagesPage(value: ProtocolJson, path = "ProtocolMessagesPage"): ProtocolMessagesPage {
  const object = dtoObject(value, path);
  const required = ["session_id","messages","has_more","next_before"];
  for (const key of required) {
    dtoRequired(object, key, path);
  }
  return {
    session_id: dtoString(object, "session_id", path),
    messages: dtoList(object, "messages", path).map((item, index) => validateProtocolMessage(item, path + ".messages[" + index + "]")),
    has_more: dtoBool(object, "has_more", path),
    next_before: dtoNullableI64(object, "next_before", path),
    page: object["page"] === undefined
        ? dtoDefaultProtocolPageMeta()
        : validateProtocolPageMeta(object["page"] as ProtocolJson, path + ".page"),
  };
}

export function validateProtocolSessionState(value: ProtocolJson, path = "ProtocolSessionState"): ProtocolSessionState {
  const object = dtoObject(value, path);
  const required = ["session_id","state","title","last_event_seq","agent_state","task_ledger"];
  for (const key of required) {
    dtoRequired(object, key, path);
  }
  return {
    session_id: dtoString(object, "session_id", path),
    state: dtoString(object, "state", path),
    title: dtoString(object, "title", path),
    last_event_seq: dtoI64(object, "last_event_seq", path),
    agent_state: validateProtocolAgentStateView(dtoRequired(object, "agent_state", path), path + ".agent_state"),
    task_ledger: dtoRequired(object, "task_ledger", path),
  };
}

export function validateProtocolAgentStateView(value: ProtocolJson, path = "ProtocolAgentStateView"): ProtocolAgentStateView {
  const object = dtoObject(value, path);
  const required = ["state","label","active","terminal"];
  for (const key of required) {
    dtoRequired(object, key, path);
  }
  return {
    state: dtoString(object, "state", path),
    label: dtoString(object, "label", path),
    active: dtoBool(object, "active", path),
    terminal: dtoBool(object, "terminal", path),
  };
}

export function validateProtocolAttachmentId(value: ProtocolJson, path = "ProtocolAttachmentId"): ProtocolAttachmentId {
  const object = dtoObject(value, path);
  const required = ["digest","mime","filename","size"];
  for (const key of required) {
    dtoRequired(object, key, path);
  }
  return {
    digest: dtoString(object, "digest", path),
    mime: dtoString(object, "mime", path),
    filename: dtoNullableString(object, "filename", path),
    size: dtoI64(object, "size", path),
  };
}

export function validateProtocolAttachmentRef(value: ProtocolJson, path = "ProtocolAttachmentRef"): ProtocolAttachmentRef {
  const object = dtoObject(value, path);
  const required = ["ref_id","digest","mime","filename","size"];
  for (const key of required) {
    dtoRequired(object, key, path);
  }
  return {
    ref_id: dtoI64(object, "ref_id", path),
    digest: dtoString(object, "digest", path),
    mime: dtoString(object, "mime", path),
    filename: dtoNullableString(object, "filename", path),
    size: dtoI64(object, "size", path),
  };
}

export function validateProtocolAttachmentUpload(value: ProtocolJson, path = "ProtocolAttachmentUpload"): ProtocolAttachmentUpload {
  const object = dtoObject(value, path);
  const required = ["mime","data_base64"];
  for (const key of required) {
    dtoRequired(object, key, path);
  }
  dtoRejectUnknown(object, path, ["mime","filename","data_base64"]);
  return {
    mime: dtoString(object, "mime", path),
    filename: dtoOptionalNullableString(object, "filename", path),
    data_base64: dtoString(object, "data_base64", path),
  };
}

export function validateProtocolTaskRun(value: ProtocolJson, path = "ProtocolTaskRun"): ProtocolTaskRun {
  const object = dtoObject(value, path);
  const required = ["task_id","run_id","mode","state","goal","item_ids","model"];
  for (const key of required) {
    dtoRequired(object, key, path);
  }
  return {
    task_id: dtoI64(object, "task_id", path),
    run_id: dtoString(object, "run_id", path),
    mode: dtoString(object, "mode", path),
    state: dtoString(object, "state", path),
    goal: dtoNullableString(object, "goal", path),
    item_ids: dtoList(object, "item_ids", path).map((item, index) => dtoStringElement(item, path + ".item_ids[" + index + "]")),
    model: dtoNullableString(object, "model", path),
  };
}

export function validateProtocolTaskRunStarted(value: ProtocolJson, path = "ProtocolTaskRunStarted"): ProtocolTaskRunStarted {
  const object = dtoObject(value, path);
  const required = ["task_id","run_id","state"];
  for (const key of required) {
    dtoRequired(object, key, path);
  }
  return {
    task_id: dtoI64(object, "task_id", path),
    run_id: dtoString(object, "run_id", path),
    state: dtoString(object, "state", path),
  };
}

export function validateProtocolTaskRunCancelled(value: ProtocolJson, path = "ProtocolTaskRunCancelled"): ProtocolTaskRunCancelled {
  const object = dtoObject(value, path);
  const required = ["run_id","cancelled"];
  for (const key of required) {
    dtoRequired(object, key, path);
  }
  return {
    run_id: dtoString(object, "run_id", path),
    cancelled: dtoBool(object, "cancelled", path),
  };
}

export function validateProtocolTaskRunWorkItem(value: ProtocolJson, path = "ProtocolTaskRunWorkItem"): ProtocolTaskRunWorkItem {
  const object = dtoObject(value, path);
  const required = ["id","kind"];
  for (const key of required) {
    dtoRequired(object, key, path);
  }
  dtoRejectUnknown(object, path, ["id","kind","summary","depends_on","acceptance_checks","ownership","required_capabilities"]);
  return {
    id: dtoString(object, "id", path),
    kind: dtoString(object, "kind", path),
    summary: dtoOptionalNullableString(object, "summary", path),
    depends_on: dtoOptionalNullableList(object, "depends_on", path)?.map((item, index) => dtoStringElement(item, path + ".depends_on[" + index + "]")) ?? null,
    acceptance_checks: dtoOptionalNullableList(object, "acceptance_checks", path)?.map((item, index) => dtoStringElement(item, path + ".acceptance_checks[" + index + "]")) ?? null,
    ownership: dtoOptionalNullableJson(object, "ownership"),
    required_capabilities: dtoOptionalNullableJson(object, "required_capabilities"),
  };
}

export function validateProtocolTaskRunStartRequest(value: ProtocolJson, path = "ProtocolTaskRunStartRequest"): ProtocolTaskRunStartRequest {
  const object = dtoObject(value, path);
  const required = ["goal","submission_id"];
  for (const key of required) {
    dtoRequired(object, key, path);
  }
  dtoRejectUnknown(object, path, ["goal","submission_id","criteria","work_items","ownership","model","max_tokens","max_cost_micro","mutation_mode","routing_mode","files","attachments","completion_contract"]);
  return {
    goal: dtoString(object, "goal", path),
    submission_id: dtoString(object, "submission_id", path),
    criteria: dtoOptionalNullableList(object, "criteria", path)?.map((item, index) => dtoStringElement(item, path + ".criteria[" + index + "]")) ?? null,
    work_items: dtoOptionalNullableList(object, "work_items", path)?.map((item, index) => validateProtocolTaskRunWorkItem(item, path + ".work_items[" + index + "]")) ?? null,
    ownership: dtoOptionalNullableJson(object, "ownership"),
    model: dtoOptionalNullableString(object, "model", path),
    max_tokens: dtoOptionalNullableI64(object, "max_tokens", path),
    max_cost_micro: dtoOptionalNullableJson(object, "max_cost_micro"),
    mutation_mode: dtoOptionalNullableString(object, "mutation_mode", path),
    routing_mode: dtoOptionalNullableString(object, "routing_mode", path),
    files: dtoOptionalNullableList(object, "files", path)?.map((item, index) => dtoStringElement(item, path + ".files[" + index + "]")) ?? null,
    attachments: dtoOptionalNullableList(object, "attachments", path)?.map((item, index) => validateProtocolAttachmentId(item, path + ".attachments[" + index + "]")) ?? null,
    completion_contract: dtoOptionalNullableJson(object, "completion_contract"),
  };
}

export function validateProtocolSessionPromptRequest(value: ProtocolJson, path = "ProtocolSessionPromptRequest"): ProtocolSessionPromptRequest {
  const object = dtoObject(value, path);
  const required = ["session_id","submission_id","prompt"];
  for (const key of required) {
    dtoRequired(object, key, path);
  }
  dtoRejectUnknown(object, path, ["session_id","submission_id","prompt","files"]);
  return {
    session_id: dtoString(object, "session_id", path),
    submission_id: dtoString(object, "submission_id", path),
    prompt: dtoString(object, "prompt", path),
    files: dtoOptionalNullableList(object, "files", path)?.map((item, index) => dtoStringElement(item, path + ".files[" + index + "]")) ?? null,
  };
}

export function validateProtocolSessionPromptReceipt(value: ProtocolJson, path = "ProtocolSessionPromptReceipt"): ProtocolSessionPromptReceipt {
  const object = dtoObject(value, path);
  const required = ["op_id","run_id","accepted","queued"];
  for (const key of required) {
    dtoRequired(object, key, path);
  }
  return {
    op_id: dtoString(object, "op_id", path),
    run_id: dtoString(object, "run_id", path),
    accepted: dtoBool(object, "accepted", path),
    queued: dtoBool(object, "queued", path),
  };
}

// ------------------------------------------------- error constants + envelope

export interface ProtocolErrorEnvelope {
  readonly code: string;
  readonly message: string;
  readonly retryable: boolean;
}

export const PROTOCOL_ERROR_CODES: readonly string[] = [
  "not_found",
  "conflict",
  "invalid_state",
  "permission_denied",
  "timeout",
  "cancelled",
  "store_error",
  "network_error",
  "provider_error",
  "malformed",
  "oversized",
  "rate_limited",
  "deadlock",
  "internal_error",
];

export const PROTOCOL_ERROR_HTTP_STATUS: { readonly [code: string]: number } = {
  "not_found": 404,
  "conflict": 409,
  "invalid_state": 409,
  "permission_denied": 403,
  "timeout": 504,
  "cancelled": 499,
  "store_error": 500,
  "network_error": 502,
  "provider_error": 502,
  "malformed": 400,
  "oversized": 413,
  "rate_limited": 429,
  "deadlock": 409,
  "internal_error": 500,
};

export const PROTOCOL_ERROR_RETRYABLE: { readonly [code: string]: boolean } = {
  "not_found": false,
  "conflict": false,
  "invalid_state": false,
  "permission_denied": false,
  "timeout": true,
  "cancelled": false,
  "store_error": true,
  "network_error": true,
  "provider_error": true,
  "malformed": false,
  "oversized": false,
  "rate_limited": true,
  "deadlock": true,
  "internal_error": false,
};

/** Parse the daemon\'s typed error envelope `{error:{code,message,retryable}}`.
 * Returns null for any non-conforming body (callers keep their own
 * http_error fallback); a non-boolean retryable reads as false. */
export function parseProtocolErrorEnvelope(value: ProtocolJson): ProtocolErrorEnvelope | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
  const error = (value as ProtocolJsonObject)["error"];
  if (typeof error !== "object" || error === null || Array.isArray(error)) return null;
  const object = error as ProtocolJsonObject;
  const code = object["code"];
  const message = object["message"];
  if (typeof code !== "string" || typeof message !== "string") return null;
  return { code, message, retryable: object["retryable"] === true };
}
