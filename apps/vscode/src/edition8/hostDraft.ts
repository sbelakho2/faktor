// Host-owned draft state for Revision 8. Context sources and favorites are
// captured in the native host (the daemon has no draft write for them); they
// are handed to the real admission/context routes only when the operator
// explicitly adds or runs.

export interface E8ContextDraftSource {
  readonly kind: 'file' | 'selection' | 'diagnostic' | 'terminal' | 'diff' | 'url' | 'prior_chat';
  readonly uri: string;
  readonly range: string | null;
  readonly version: string | null;
  readonly reason: string;
  readonly text: string | null;
}

const contextDraft: E8ContextDraftSource[] = [];
const favorites = new Set<string>();

export function contextDraftSources(): readonly E8ContextDraftSource[] {
  return [...contextDraft];
}

export function addContextDraft(source: E8ContextDraftSource): void {
  if (contextDraft.some((entry) => entry.kind === source.kind && entry.uri === source.uri && entry.range === source.range)) {
    return;
  }
  contextDraft.push(source);
  if (contextDraft.length > 32) contextDraft.shift();
}

export function clearContextDraft(): void {
  contextDraft.length = 0;
}

export function toggleFavorite(model: string): boolean {
  if (favorites.has(model)) {
    favorites.delete(model);
    return false;
  }
  favorites.add(model);
  return true;
}

export function favoriteModels(): readonly string[] {
  return [...favorites].sort();
}
