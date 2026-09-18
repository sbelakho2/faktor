// Control-plane credential handling.
//
// The bearer credential (`x-faktor-control-token`) lives in the OS/IDE secret
// store (`vscode.SecretStorage`), keyed by the cloud endpoint plus the
// organization/account it belongs to. The deprecated plaintext setting
// `faktor.controlToken` is read at most ONCE per resolution: when a plaintext
// value is found and no secret exists, the operator is asked to migrate, and
// on acceptance the value is stored in the secret store and the plaintext
// setting is deleted — the two copies never coexist silently. A declined or
// impossible migration refuses the plaintext value for every cloud call (the
// resolution never carries it); the local daemon password path is separate
// and unchanged.
//
// This module is dependency-free on purpose (no `vscode` import): the secret
// store, the plaintext setting and the remote revoke call are injected, so
// `scripts/selftest.mjs` drives every path with fake rows.

/**
 * The structural subset of `vscode.SecretStorage` this module uses. The
 * VS Code API returns `Thenable`, so the seam accepts any `PromiseLike`.
 */
export interface SecretStorageLike {
  get(key: string): PromiseLike<string | undefined>;
  store(key: string, value: string): PromiseLike<void>;
  delete(key: string): PromiseLike<void>;
}

/** The deprecated plaintext setting, behind one clear seam. */
export interface PlaintextSettingLike {
  read(): string | null;
  clear(): Promise<void>;
}

/** The control-plane credential identity: endpoint + organization/account. */
export interface ControlPlaneScope {
  readonly endpoint: string;
  readonly organization: string;
}

/**
 * The NON-secret auth-session coordinates the logout route names. The daemon
 * route (`POST /native/sso/logout`) revokes "the presented token", but its
 * strict body must name the `{organization, session_id}` that token owns: an
 * opaque session token cannot be reverse-mapped to its session id, so the id
 * is provisioned next to the credential (SSO callback / bootstrap result) and
 * stored as a non-secret coordinate. Only the session id, never the token.
 */
export interface ControlPlaneAuthSession {
  readonly organization: string;
  readonly sessionId: string;
}

/** Non-secret coordinates persisted so sign-out can name the session. */
export const CONTROL_PLANE_AUTH_SESSION_CONFIG = 'faktor.controlPlaneAuthSession';

export function controlPlaneAuthSessionValid(
  session: ControlPlaneAuthSession | null,
): session is ControlPlaneAuthSession {
  return (
    session !== null &&
    session.organization.trim().length > 0 &&
    session.sessionId.trim().length > 0
  );
}

/** True for the secret-store keys this module owns (prefix + coordinates). */
export function isControlPlaneSecretKey(key: string): boolean {
  return key.startsWith(`${CONTROL_PLANE_SECRET_PREFIX}.`);
}

/** The structural subset of `vscode.SecretStorage.onDidChange`. */
export interface SecretStorageChangeLike {
  onDidChange(listener: (event: { key: string }) => void): { dispose(): void };
}

export interface WatchControlPlaneSecretChangesOptions {
  readonly secrets: SecretStorageChangeLike;
  /**
   * Re-resolve the credential and hand it to the running client (the
   * extension re-runs the resolution, so a removed secret clears the token).
   */
  readonly apply: () => Promise<void>;
  /** A failed refresh is reported, never swallowed and never fatal. */
  readonly onError?: (error: unknown) => void;
  /** Relevance filter; defaults to every control-plane secret key. */
  readonly isRelevant?: (key: string) => boolean;
}

/**
 * Forward EXTERNAL secret-store changes (another window, the OS keychain UI,
 * a settings import) to the running client. The platform emits `onDidChange`
 * for every secret write/delete, so a credential rotated outside this window
 * replaces the live token without a restart; a deletion resolves `absent` and
 * clears it. Applies are serialized so a rapid store+delete settles on the
 * LAST state, never on a stale resolution.
 */
export function watchControlPlaneSecretChanges(
  options: WatchControlPlaneSecretChangesOptions,
): { dispose(): void } {
  const relevant = options.isRelevant ?? isControlPlaneSecretKey;
  const report = options.onError ?? (() => undefined);
  let pending: Promise<void> = Promise.resolve();
  const subscription = options.secrets.onDidChange((event) => {
    if (typeof event?.key !== 'string' || !relevant(event.key)) {
      return;
    }
    pending = pending.then(options.apply).catch(report);
  });
  return { dispose: () => subscription.dispose() };
}

/** Secret key prefix; the endpoint and organization are appended encoded. */
export const CONTROL_PLANE_SECRET_PREFIX = 'faktor.controlPlaneToken.v1';

/** The deprecated plaintext setting id (contributed only to warn). */
export const LEGACY_CONTROL_TOKEN_CONFIG = 'faktor.controlToken';

/** Non-secret coordinates persisted so the secret can be found again. */
export const CONTROL_PLANE_ENDPOINT_CONFIG = 'faktor.controlPlaneEndpoint';
export const CONTROL_PLANE_ORGANIZATION_CONFIG = 'faktor.controlPlaneOrganization';

/** One resolution outcome. Only `secret`/`migrated` ever carry a token. */
export type ControlPlaneResolution =
  | {
      readonly kind: 'secret';
      readonly token: string;
      readonly key: string;
      /** A leftover plaintext copy was deleted by this resolution. */
      readonly legacyCleared: boolean;
    }
  | { readonly kind: 'migrated'; readonly token: string; readonly key: string }
  | { readonly kind: 'absent' }
  | { readonly kind: 'legacy-refused'; readonly reason: string };

/** The value the native client may send, or null (never a refused plaintext). */
export function controlPlaneTokenForClient(resolution: ControlPlaneResolution): string | null {
  if (resolution.kind === 'secret' || resolution.kind === 'migrated') {
    return resolution.token;
  }
  return null;
}

export function normalizeControlPlaneEndpoint(endpoint: string): string {
  return endpoint.trim().replace(/\/+$/, '').toLowerCase();
}

export function controlPlaneScopeValid(scope: ControlPlaneScope | null): scope is ControlPlaneScope {
  return (
    scope !== null &&
    normalizeControlPlaneEndpoint(scope.endpoint).length > 0 &&
    scope.organization.trim().length > 0
  );
}

/**
 * The secret key for one (endpoint, organization) pair. Endpoint and
 * organization are non-secret coordinates; the token is the value, never a
 * key component.
 */
export function controlPlaneSecretKey(scope: ControlPlaneScope): string {
  if (!controlPlaneScopeValid(scope)) {
    throw new Error('control-plane scope requires a non-empty endpoint and organization');
  }
  const endpoint = normalizeControlPlaneEndpoint(scope.endpoint);
  const organization = scope.organization.trim();
  return `${CONTROL_PLANE_SECRET_PREFIX}.${encodeURIComponent(endpoint)}.${encodeURIComponent(organization)}`;
}

function nonEmpty(value: string | null | undefined): string | null {
  if (typeof value !== 'string' || value.length === 0) {
    return null;
  }
  return value;
}

/**
 * Store one token in the secret store and delete the deprecated plaintext
 * setting: the two copies must never coexist.
 */
export async function storeControlPlaneToken(
  secrets: SecretStorageLike,
  plaintext: PlaintextSettingLike,
  scope: ControlPlaneScope,
  token: string,
): Promise<string> {
  if (token.trim().length === 0) {
    throw new Error('refusing to store an empty control-plane credential');
  }
  const key = controlPlaneSecretKey(scope);
  await secrets.store(key, token);
  await plaintext.clear();
  return key;
}

export interface ResolveControlPlaneOptions {
  readonly secrets: SecretStorageLike;
  readonly plaintext: PlaintextSettingLike;
  readonly scope: ControlPlaneScope | null;
  /** The deprecated plaintext value, read once by the caller for this call. */
  readonly legacyToken: string | null;
  readonly confirmMigration?: (scope: ControlPlaneScope) => Promise<boolean>;
}

/**
 * Resolve the credential for the configured scope:
 *   1. the secret store wins (and any leftover plaintext copy is deleted, so
 *      both never linger);
 *   2. otherwise a plaintext setting is read once and migrated with an
 *      explicit operator confirmation (store the secret, then delete the
 *      plaintext);
 *   3. a declined migration REFUSES the plaintext value — it is never
 *      returned for a cloud call.
 */
export async function resolveControlPlaneToken(
  options: ResolveControlPlaneOptions,
): Promise<ControlPlaneResolution> {
  const legacy = nonEmpty(options.legacyToken);
  if (!controlPlaneScopeValid(options.scope)) {
    if (legacy !== null) {
      return {
        kind: 'legacy-refused',
        reason:
          'the deprecated plaintext faktor.controlToken setting cannot be sent: no control-plane endpoint/organization ' +
          'is configured for the secret-store key; run "Faktor: Sign in to Control Plane" to migrate it, or remove the setting',
      };
    }
    return { kind: 'absent' };
  }
  const scope = options.scope;
  const key = controlPlaneSecretKey(scope);
  const stored = nonEmpty(await options.secrets.get(key));
  if (stored !== null) {
    if (legacy !== null) {
      await options.plaintext.clear();
      return { kind: 'secret', token: stored, key, legacyCleared: true };
    }
    return { kind: 'secret', token: stored, key, legacyCleared: false };
  }
  if (legacy === null) {
    return { kind: 'absent' };
  }
  const confirm = options.confirmMigration;
  if (confirm === undefined || !(await confirm(scope))) {
    return {
      kind: 'legacy-refused',
      reason:
        'the plaintext faktor.controlToken setting was not migrated to the OS secret store; it is refused for ' +
        'cloud calls and never sent (run "Faktor: Sign in to Control Plane" to store it securely)',
    };
  }
  await options.secrets.store(key, legacy);
  await options.plaintext.clear();
  return { kind: 'migrated', token: legacy, key };
}

/** One logout outcome: local deletion is unconditional; remote is best-effort. */
export interface ControlPlaneLogoutResult {
  readonly key: string | null;
  readonly hadSecret: boolean;
  readonly remoteRevoked: boolean;
  readonly remoteError: string | null;
  readonly legacyCleared: boolean;
}

/**
 * Sign out: attempt the control plane's revoke/delete-session route when the
 * daemon is reachable AND the session id is known, then delete the local
 * secret unconditionally (a failed or impossible remote revoke is reported,
 * never swallowed, and never keeps the local copy).
 */
export async function logoutControlPlane(options: {
  readonly secrets: SecretStorageLike;
  readonly plaintext: PlaintextSettingLike;
  readonly scope: ControlPlaneScope | null;
  /** The non-secret session coordinates the route body must name. */
  readonly session?: ControlPlaneAuthSession | null;
  readonly revoke?: (token: string, session: ControlPlaneAuthSession) => Promise<void>;
}): Promise<ControlPlaneLogoutResult> {
  let key: string | null = null;
  let hadSecret = false;
  let remoteRevoked = false;
  let remoteError: string | null = null;
  if (controlPlaneScopeValid(options.scope)) {
    key = controlPlaneSecretKey(options.scope);
    const token = nonEmpty(await options.secrets.get(key));
    if (token !== null) {
      hadSecret = true;
      const session = options.session ?? null;
      if (options.revoke !== undefined && controlPlaneAuthSessionValid(session)) {
        try {
          await options.revoke(token, session);
          remoteRevoked = true;
        } catch (error) {
          remoteError = error instanceof Error ? error.message : String(error);
        }
      } else if (options.revoke !== undefined) {
        remoteError =
          'no control-plane auth-session id is known for this credential, so the remote session could not be named ' +
          'and was not revoked (the local secret is still deleted)';
      }
      await options.secrets.delete(key);
    }
  }
  await options.plaintext.clear();
  return { key, hadSecret, remoteRevoked, remoteError, legacyCleared: true };
}
