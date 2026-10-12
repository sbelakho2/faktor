// Pure session-switch policy: the idempotence and request-shape rules of one
// explicit conversation switch. Dependency-free so scripts/selftest.mjs can
// drive it directly and extension.ts keeps the decision OUT of the mutation
// path — a switch to the already-active conversation is a no-op that must
// never stop/re-start the stream or drop session-scoped state.

/** The decided action for one explicit switch request. */
export type SessionSwitchDecision =
  | { readonly action: 'noop'; readonly reason: string }
  | { readonly action: 'switch'; readonly sessionId: string };

/**
 * Decide one explicit switch from the active conversation id and the raw
 * requested value. A blank/non-string request and the already-active
 * conversation (after trimming) are no-ops with their precise reason; every
 * other id is a switch — the daemon is the authority on existence (the id
 * always comes from a served listing) and answers a vanished conversation
 * typed, which the host surfaces.
 */
export function sessionSwitchDecision(
  activeSessionId: string | null,
  requestedSessionId: unknown,
): SessionSwitchDecision {
  if (typeof requestedSessionId !== 'string') {
    return { action: 'noop', reason: 'no conversation was requested' };
  }
  const requested = requestedSessionId.trim();
  if (requested.length === 0) {
    return { action: 'noop', reason: 'the requested conversation id is empty' };
  }
  if (activeSessionId !== null && requested === activeSessionId) {
    return { action: 'noop', reason: 'the conversation is already active' };
  }
  return { action: 'switch', sessionId: requested };
}
