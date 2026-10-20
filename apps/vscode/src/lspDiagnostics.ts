// Host-integrated language-server diagnostics and code actions (F225/F226/F235).
//
// The daemon owns the workspace-scoped rust-analyzer process; this module is
// the VS Code Problems/code-action projection of that ONE authority. Nothing
// is synthesized locally: a disabled route, missing toolchain, timeout or
// malformed payload surfaces as a typed warning and leaves the last coherent
// collection untouched. Applying a code action is the operator's explicit
// Quick Fix selection; the provider never edits a document on its own.

import * as vscode from 'vscode';
import type { Json } from './edition8/contract.ts';
import type { NativeClient } from './nativeClient.ts';
import { NativeApiError } from './nativeClient.ts';

interface WirePosition {
  readonly line: number;
  readonly character: number;
}

interface WireRange {
  readonly start: WirePosition;
  readonly end: WirePosition;
}

interface WireDiagnostic {
  readonly range: WireRange;
  readonly severity?: number;
  readonly code?: string;
  readonly source?: string;
  readonly message: string;
  readonly tags?: readonly number[];
}

interface DiagnosticsPayload {
  readonly ok: boolean;
  readonly source: string;
  readonly uri: string;
  readonly version: number | null;
  readonly sequence: string;
  readonly truncated: boolean;
  readonly diagnostics: readonly WireDiagnostic[];
}

interface WireCodeAction {
  readonly title: string;
  readonly kind?: string;
  readonly isPreferred?: boolean;
  readonly disabledReason?: string;
  readonly edit?: Json;
  readonly command?: Json;
  readonly data?: Json;
}

interface CodeActionsPayload {
  readonly ok: boolean;
  readonly source: string;
  readonly uri: string;
  readonly actions: readonly WireCodeAction[];
  readonly autoApplied: boolean;
}

export interface LspHost {
  readonly client: () => NativeClient | null;
  readonly session: () => string | null;
  readonly workspaceRoot: () => string | null;
}

const MAX_DIAGNOSTICS = 2048;
const MAX_ACTIONS = 128;
const MAX_FIELD = 8 * 1024;

function integer(value: Json | undefined): number | null {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? Math.floor(value) : null;
}

function stringField(value: Json | undefined, max: number): string | null {
  return typeof value === 'string' && value.length <= max ? value : null;
}

function object(value: Json | undefined): Record<string, Json> | null {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    ? (value as Record<string, Json>)
    : null;
}

function wirePosition(value: Json | undefined): vscode.Position | null {
  const entry = object(value);
  if (entry === null) return null;
  const line = integer(entry.line);
  const character = integer(entry.character);
  if (line === null || character === null) return null;
  return new vscode.Position(line, character);
}

function wireRange(value: Json | undefined): vscode.Range | null {
  const entry = object(value);
  if (entry === null) return null;
  const start = wirePosition(entry.start);
  const end = wirePosition(entry.end);
  if (start === null || end === null || end.isBefore(start)) return null;
  return new vscode.Range(start, end);
}

function validateDiagnostics(value: Json, path: string): DiagnosticsPayload {
  const entry = object(value);
  if (entry === null || entry.ok !== true) throw new Error(`${path}: not an ok payload`);
  const source = stringField(entry.source, MAX_FIELD);
  const uri = stringField(entry.uri, MAX_FIELD);
  const sequence = typeof entry.sequence === 'string' ? entry.sequence : null;
  const raw = entry.diagnostics;
  if (source === null || uri === null || sequence === null || !Array.isArray(raw) || raw.length > MAX_DIAGNOSTICS) {
    throw new Error(`${path}: malformed diagnostics payload`);
  }
  const diagnostics: WireDiagnostic[] = [];
  for (const item of raw) {
    const diagnostic = object(item);
    if (diagnostic === null) continue;
    const range = wireRange(diagnostic.range);
    const message = stringField(diagnostic.message, MAX_FIELD);
    if (range === null || message === null) continue;
    const severity = integer(diagnostic.severity);
    const code = stringField(diagnostic.code, MAX_FIELD);
    const diagnosticSource = stringField(diagnostic.source, MAX_FIELD);
    diagnostics.push({
      range: {
        start: { line: range.start.line, character: range.start.character },
        end: { line: range.end.line, character: range.end.character },
      },
      ...(severity !== null && severity >= 1 && severity <= 4 ? { severity } : {}),
      ...(code !== null ? { code } : {}),
      ...(diagnosticSource !== null ? { source: diagnosticSource } : {}),
      message,
      ...(Array.isArray(diagnostic.tags)
        ? { tags: diagnostic.tags.filter((tag): tag is number => typeof tag === 'number').slice(0, 8) }
        : {}),
    });
  }
  return {
    ok: true,
    source,
    uri,
    version: integer(entry.version),
    sequence,
    truncated: entry.truncated === true,
    diagnostics,
  };
}

function validateActions(value: Json, path: string): CodeActionsPayload {
  const entry = object(value);
  if (entry === null || entry.ok !== true || entry.autoApplied !== false) {
    throw new Error(`${path}: malformed code-action payload`);
  }
  const source = stringField(entry.source, MAX_FIELD);
  const uri = stringField(entry.uri, MAX_FIELD);
  const raw = entry.actions;
  if (source === null || uri === null || !Array.isArray(raw) || raw.length > MAX_ACTIONS) {
    throw new Error(`${path}: malformed code-action list`);
  }
  const actions: WireCodeAction[] = [];
  for (const item of raw) {
    const action = object(item);
    if (action === null) continue;
    const title = stringField(action.title, MAX_FIELD);
    if (title === null) continue;
    actions.push({
      title,
      ...(typeof action.kind === 'string' ? { kind: action.kind.slice(0, MAX_FIELD) } : {}),
      ...(action.isPreferred === true ? { isPreferred: true } : {}),
      ...(typeof action.disabledReason === 'string'
        ? { disabledReason: action.disabledReason.slice(0, MAX_FIELD) }
        : {}),
      ...(action.edit !== undefined ? { edit: action.edit } : {}),
      ...(action.command !== undefined ? { command: action.command } : {}),
      ...(action.data !== undefined ? { data: action.data } : {}),
    });
  }
  return { ok: true, source, uri, actions, autoApplied: false };
}

function severityOf(value: number | undefined): vscode.DiagnosticSeverity {
  switch (value) {
    case 1:
      return vscode.DiagnosticSeverity.Error;
    case 2:
      return vscode.DiagnosticSeverity.Warning;
    case 3:
      return vscode.DiagnosticSeverity.Information;
    default:
      return vscode.DiagnosticSeverity.Hint;
  }
}

function toDiagnostic(value: WireDiagnostic): vscode.Diagnostic {
  const range = new vscode.Range(
    value.range.start.line,
    value.range.start.character,
    value.range.end.line,
    value.range.end.character,
  );
  const diagnostic = new vscode.Diagnostic(range, value.message, severityOf(value.severity));
  diagnostic.source = value.source ?? 'faktor-lsp';
  if (value.code !== undefined) diagnostic.code = value.code;
  const tags: vscode.DiagnosticTag[] = [];
  for (const tag of value.tags ?? []) {
    if (tag === 1) tags.push(vscode.DiagnosticTag.Unnecessary);
    if (tag === 2) tags.push(vscode.DiagnosticTag.Deprecated);
  }
  if (tags.length > 0) diagnostic.tags = tags;
  return diagnostic;
}

function toServerDiagnostic(value: vscode.Diagnostic): Json {
  return {
    range: {
      start: { line: value.range.start.line, character: value.range.start.character },
      end: { line: value.range.end.line, character: value.range.end.character },
    },
    ...(value.severity >= 1 && value.severity <= 4 ? { severity: value.severity } : {}),
    ...(typeof value.code === 'string' || typeof value.code === 'number' ? { code: String(value.code) } : {}),
    ...(typeof value.source === 'string' ? { source: value.source } : {}),
    message: value.message.slice(0, MAX_FIELD),
    ...(value.tags === undefined ? {} : { tags: value.tags.filter((tag) => tag === 1 || tag === 2) }),
  };
}

function workspaceEditOf(value: Json): vscode.WorkspaceEdit | null {
  const entry = object(value);
  if (entry === null) return null;
  const edit = new vscode.WorkspaceEdit();
  let sawEdit = false;
  const changes = object(entry.changes);
  if (changes !== null) {
    for (const [uri, edits] of Object.entries(changes)) {
      if (!Array.isArray(edits)) continue;
      for (const raw of edits) {
        const item = object(raw);
        if (item === null) continue;
        const range = wireRange(item.range);
        const newText = stringField(item.newText, 1024 * 1024);
        if (range === null || newText === null) continue;
        edit.replace(vscode.Uri.parse(uri), range, newText);
        sawEdit = true;
      }
    }
  }
  const documentChanges = entry.documentChanges;
  if (Array.isArray(documentChanges)) {
    for (const change of documentChanges) {
      const item = object(change);
      const document = object(item?.textDocument ?? null);
      const uri = typeof document?.uri === 'string' ? document.uri : null;
      if (uri === null || !Array.isArray(item?.edits)) continue;
      for (const raw of item.edits) {
        const textEdit = object(raw);
        if (textEdit === null) continue;
        const range = wireRange(textEdit.range);
        const newText = stringField(textEdit.newText, 1024 * 1024);
        if (range === null || newText === null) continue;
        edit.replace(vscode.Uri.parse(uri), range, newText);
        sawEdit = true;
      }
    }
  }
  return sawEdit ? edit : null;
}

function bindingProblem(host: LspHost): string | null {
  if (host.client() === null) return 'Faktor is not running; no language server is reachable.';
  if (host.session() === null) return 'No conversation is bound; diagnostics are session-scoped.';
  if (host.workspaceRoot() === null) return 'No workspace is open; the language server is workspace-scoped.';
  return null;
}

export function registerLspDiagnostics(context: vscode.ExtensionContext, host: LspHost): vscode.Disposable {
  const collection = vscode.languages.createDiagnosticCollection('faktor-lsp');
  context.subscriptions.push(collection);
  const inflight = new Set<string>();

  const refresh = async (document: vscode.TextDocument): Promise<void> => {
    if (document.uri.scheme !== 'file') return;
    const problem = bindingProblem(host);
    if (problem !== null) {
      void vscode.window.showWarningMessage(problem);
      return;
    }
    if (document.isDirty) {
      void vscode.window.showWarningMessage(
        'Save the file before requesting daemon diagnostics; an unsaved buffer would not match the served range.',
      );
      return;
    }
    const root = host.workspaceRoot();
    const relative = vscode.workspace.asRelativePath(document.uri, false);
    if (root === null || relative.startsWith('..')) {
      void vscode.window.showWarningMessage('The file is outside the bound workspace root.');
      return;
    }
    const key = document.uri.toString();
    if (inflight.has(key)) return;
    inflight.add(key);
    try {
      const client = host.client();
      const session = host.session();
      if (client === null || session === null) return;
      const payload = await client.edition5Json<DiagnosticsPayload>(
        'GET',
        `/native/lsp/diagnostics?session=${encodeURIComponent(session)}&path=${encodeURIComponent(relative)}`,
        null,
        validateDiagnostics,
      );
      const diagnostics = payload.diagnostics.map(toDiagnostic);
      if (diagnostics.length === 0) {
        collection.delete(document.uri);
        void vscode.window.setStatusBarMessage(
          `Faktor LSP: ${payload.source} published no diagnostics${payload.truncated ? ' (truncated)' : ''}`,
          3000,
        );
      } else {
        collection.set(document.uri, diagnostics);
      }
    } catch (error) {
      const message =
        error instanceof NativeApiError
          ? `${error.code}: ${error.message}`
          : error instanceof Error
            ? error.message
            : String(error);
      void vscode.window.showWarningMessage(`Faktor LSP diagnostics unavailable: ${message}`);
    } finally {
      inflight.delete(key);
    }
  };

  const provider = vscode.languages.registerCodeActionsProvider(
    [{ scheme: 'file', language: 'rust' }, { scheme: 'file', language: 'typescript' }, { scheme: 'file', language: 'python' }],
    {
      async provideCodeActions(document, range, actionContext) {
        if (document.isDirty) return [];
        const problem = bindingProblem(host);
        if (problem !== null) return [];
        const client = host.client();
        const session = host.session();
        if (client === null || session === null) return [];
        const relative = vscode.workspace.asRelativePath(document.uri, false);
        if (relative.startsWith('..')) return [];
        const diagnostics =
          actionContext.diagnostics.length > 0
            ? actionContext.diagnostics
            : collection.get(document.uri) ?? [];
        try {
          const payload = await client.edition5Json<CodeActionsPayload>(
            'POST',
            '/native/lsp/code-actions',
            {
              session,
              path: relative,
              range: {
                start: { line: range.start.line, character: range.start.character },
                end: { line: range.end.line, character: range.end.character },
              },
              diagnostics: diagnostics.slice(0, 64).map(toServerDiagnostic),
            },
            validateActions,
          );
          const actions: vscode.CodeAction[] = [];
          for (const entry of payload.actions) {
            if (entry.disabledReason !== undefined) continue;
            const edit = entry.edit === undefined ? null : workspaceEditOf(entry.edit);
            if (edit === null) continue;
            const action = new vscode.CodeAction(entry.title, vscode.CodeActionKind.QuickFix);
            action.isPreferred = entry.isPreferred === true;
            action.edit = edit;
            action.diagnostics = diagnostics.filter((diagnostic) =>
              diagnostic.range.isEqual(range) || range.contains(diagnostic.range.start),
            );
            actions.push(action);
          }
          return actions;
        } catch (error) {
          const message =
            error instanceof NativeApiError
              ? `${error.code}: ${error.message}`
              : error instanceof Error
                ? error.message
                : String(error);
          void vscode.window.showWarningMessage(`Faktor LSP code actions unavailable: ${message}`);
          return [];
        }
      },
    },
    { providedCodeActionKinds: [vscode.CodeActionKind.QuickFix] },
  );

  const refreshCommand = vscode.commands.registerCommand('faktor.refreshLspDiagnostics8', async () => {
    const editor = vscode.window.activeTextEditor;
    if (editor === undefined) {
      void vscode.window.showWarningMessage('No active editor exists; no diagnostics were requested.');
      return;
    }
    await refresh(editor.document);
  });
  // Re-open of a file clears its host collection first: a stale diagnostic is
  // never presented as current after the document changed.
  const closed = vscode.workspace.onDidCloseTextDocument((document) => {
    collection.delete(document.uri);
  });
  return vscode.Disposable.from(collection, provider, refreshCommand, closed);
}
