// The host-side agent-steer guard. The runtime refuses an empty or oversized
// inline note (`faktor_session::MAX_CHILD_CONTROL_NOTE_CHARS`); the host
// refuses the same shapes first with the same 500-character bound, so an
// oversized value never reaches the daemon. Dependency-free (no vscode
// import) so scripts/selftest.mjs can drive it directly.

/** Mirrors the runtime's steering note bound (chars). */
export const MAX_STEER_CHARS = 500;

/**
 * Trims one inline steer note and returns it, or a typed refusal reason.
 * The host control path runs THIS function before the daemon (which
 * re-validates) ever sees the value; the refusal is loud and typed, never a
 * silent truncation or coercion.
 */
export function normalizeSteerText(
  raw: unknown,
): { readonly ok: true; readonly text: string } | { readonly ok: false; readonly reason: string } {
  if (typeof raw !== 'string') {
    return { ok: false, reason: 'the steer note must be a non-empty string' };
  }
  const trimmed = raw.trim();
  if (trimmed.length === 0) {
    return { ok: false, reason: 'the steer note must be a non-empty string' };
  }
  if (trimmed.length > MAX_STEER_CHARS) {
    return {
      ok: false,
      reason: `the steer note exceeds ${MAX_STEER_CHARS} character bound`,
    };
  }
  return { ok: true, text: trimmed };
}
