// Approval-decision binding for the Edition 8 surface.
//
// A decision is only ever sent for a request id that is present in the
// CURRENT served pending list and that the operator selected explicitly.
// There is deliberately no "first pending" fallback: an Allow click rendered
// for one generation must never approve a different (or newly arrived)
// request. Everything here is pure so the selftest drives the hostile cases.

export interface PendingApprovalLike {
  readonly id: string;
}

/** The bounded id set of the served pending list, never fabricated. */
export function pendingApprovalIds(snapshot: unknown): string[] {
  if (snapshot === null || typeof snapshot !== 'object' || Array.isArray(snapshot)) return [];
  const approvals = (snapshot as { approvals?: unknown }).approvals;
  if (approvals === null || typeof approvals !== 'object' || Array.isArray(approvals)) return [];
  const list = (approvals as { permissions?: unknown }).permissions;
  if (!Array.isArray(list)) return [];
  const ids: string[] = [];
  for (const entry of list.slice(0, 16)) {
    if (entry === null || typeof entry !== 'object' || Array.isArray(entry)) continue;
    const id = (entry as { id?: unknown }).id;
    if (typeof id === 'string' && /^[1-9][0-9]{0,31}$/.test(id) && !ids.includes(id)) {
      ids.push(id);
    }
  }
  return ids;
}

/**
 * The exact live request id a decision may address, or null. A stale id, a
 * missing id, a non-canonical id or an id absent from the served list is a
 * refusal; the caller must not fall back to another pending row.
 */
export function exactPendingApprovalId(
  requested: unknown,
  pending: readonly PendingApprovalLike[],
): string | null {
  if (typeof requested !== 'string' || requested.length === 0 || requested.length > 32) {
    return null;
  }
  if (!/^[1-9][0-9]*$/.test(requested)) return null;
  return pending.some((entry) => entry.id === requested) ? requested : null;
}
