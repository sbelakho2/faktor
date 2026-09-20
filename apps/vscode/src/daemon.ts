// The Faktor daemon lifecycle (extracted from the launcher-only
// extension.ts):
//
//  1. Resolve the executable. An explicit `binaryPath`/FAKTOR_BIN wins;
//     otherwise, when an INSTALL LAYOUT exists at the install root
//     (`<root>/launcher` beside a `current` pointer or `versions/`), the
//     STABLE BOOTSTRAP LAUNCHER is spawned: it authenticates the activated
//     release through the signed pointer and execs exactly those bytes, so
//     an updater activation actually controls which binary runs. Only when
//     NO install layout is present does resolution fall back to the legacy
//     target/debug|release/faktor-cli lookup.
//  2. Generate a 64-hex FAKTOR_SERVER_PASSWORD and spawn the resolved
//     executable with `serve --port 0` and it in the environment.
//  3. Read stdout line-by-line until the EXACT frozen startup line
//     `/faktor server listening on http:\/\/127\.0\.0\.1:(\d+)/`, resolve
//     the port, and build the Faktor-native bearer claim
//     (`Authorization: Bearer <password>`). Basic/compat auth forms were
//     removed with the auth cutover; there is no downgrade path.
//  4. Track the RELEASE process, not the launcher's pid. The non-unix
//     bootstrap launcher exits as soon as the release proves
//     digest+readiness (bounded by its 30 s launch window), so its pid goes
//     stale the moment the daemon is healthy; on unix `execve` replaces the
//     launcher (the spawned child IS the release). The supervisor resolves
//     the release pid, in order, from:
//       a. the launcher's release-pid handshake line
//          `faktor release started pid=<pid> digest=<64 lowercase hex>`:
//          one frozen line (`crates/updater/src/release.rs` `launch()`,
//          immediately after `command.spawn()`) that precedes any forwarded
//          release stdout, so it is latched before the startup line resolves,
//       b. the platform probe for the process that owns the daemon's
//          listening port (Windows: Get-NetTCPConnection / netstat),
//       c. the spawned child while it is still the live daemon (unix exec,
//          or a legacy resident launcher).
//     The handshake pid is TRUST-BOUND, never latched blindly: its digest is
//     compared against the digest the daemon itself reports through
//     GET /native/health (`version = <pkg>+release.<id>.<digest>`), and a
//     port-probe pid that disagrees with the announced pid also refuses the
//     announcement. A refused announcement is never adopted — `pid` falls
//     back to the probe/child and `stop()` refuses to signal the announced
//     pid (typed `releaseTrustError`).
//     `alive()` probes the resolved pid AND GET /native/health, so a
//     launcher that exited after readiness is never mistaken for daemon
//     death, and `stop()` terminates the release gracefully (SIGTERM, then
//     SIGKILL), never just the launcher: it re-verifies health identity (and
//     the announcement digest) before signaling any non-child pid, so a
//     recycled pid is refused typed instead of SIGTERMing an unrelated
//     process.
//  5. Expose health() against GET /native/health. The daemon's health
//     `version` carries `+release.<id>.<digest>` whenever it was launched
//     through the bootstrap; `releaseDigest` surfaces that digest so a
//     supervisor can observe WHICH artifact is live.
//
// Deliberately dependency-free (node:http/node:child_process only; no axios,
// no vscode import — the caller supplies the workspace root). The daemon
// never prints the password; the lifecycle never logs it.

import * as http from 'node:http';
import * as crypto from 'node:crypto';
import { ChildProcess, execFile, execFileSync, spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

export const STARTUP_LINE = /faktor server listening on http:\/\/127\.0\.0\.1:(\d+)/;

/**
 * The bootstrap launcher's frozen release-pid handshake line. The non-unix
 * launcher writes exactly `faktor release started pid=<release pid>
 * digest=<verified digest>` immediately after `command.spawn()`
 * (`crates/updater/src/release.rs` `launch()`) and before forwarding any
 * release stdout, so it is the supervisor's primary handle on the release
 * pid; the platform port probe and the live spawned child are the fallbacks.
 */
export const RELEASE_PID_LINE = /^faktor release started pid=([1-9]\d*) digest=([0-9a-f]{64})$/;

const DEFAULT_STARTUP_TIMEOUT_MS = 45_000;
const HEALTH_TIMEOUT_MS = 5_000;
const STDERR_TAIL_BYTES = 8 * 1024;
const LAUNCHER_STDOUT_BUFFER_BYTES = 64 * 1024;
const KILL_ESCALATION_MS = 5_000;
const PID_PROBE_TIMEOUT_MS = 5_000;
const PID_PROBE_MAX_BYTES = 256 * 1024;
const MAX_HEALTH_BODY_BYTES = 64 * 1024;
/**
 * How long the supervisor waits for the daemon's own health route to confirm
 * the announced release digest before it stops trying (a daemon that is still
 * bringing its HTTP surface up). Exceeding it leaves the announcement
 * tentatively adopted but UNVERIFIED: `stop()` then demands a live digest
 * match before it will signal that pid.
 */
const ANNOUNCEMENT_VERIFY_MS = 3_000;

export interface DaemonOptions {
  /** Workspace root used to locate target/debug|release/faktor-cli. */
  readonly workspaceRoot: string;
  /** Explicit binary override (config `faktor.binaryPath` or FAKTOR_BIN).
   * Wins over the bootstrap: an explicit operator path is never silently
   * replaced. */
  readonly binaryPath?: string;
  /** Optional `--data-dir` for the daemon (config `faktor.dataDir`). */
  readonly dataDir?: string;
  /** The install root holding the immutable release layout (`current`,
   * `launcher`, `versions/`, `trusted-keys.json`). Defaults to
   * `<dataDir>/install`, or `~/.faktor/install` when no data dir is
   * configured (the daemon's own default data dir). */
  readonly installRoot?: string;
  /** Extra argv appended after `serve --port 0`. */
  readonly extraArgs?: readonly string[];
  readonly startupTimeoutMs?: number;
  readonly env?: NodeJS.ProcessEnv;
  /**
   * Test/embedder seam: resolve the RELEASE process id for a startup port.
   * When set it replaces the platform port probe (the announced handshake
   * pid and the live-child fallback still apply); when absent the default
   * Windows probe runs and unix falls back to the exec'd child pid.
   */
  readonly resolveReleasePid?: (port: number) => Promise<number | null> | number | null;
}

export interface DaemonHealth {
  readonly ok: boolean;
  readonly version: string;
  /** The release digest the running daemon reports (verified by the stable
   * bootstrap launcher). `null` when the daemon was not started through an
   * install layout — an honest absence, never a guess. */
  readonly releaseDigest: string | null;
}

/**
 * A typed refusal to trust a release-pid source. `code` is stable for
 * assertions; `message` names the exact check that failed.
 */
export interface DaemonTrustFailure {
  readonly code:
    | 'release_digest_mismatch'
    | 'release_digest_absent'
    | 'release_pid_probe_mismatch'
    | 'release_digest_unverified'
    | 'stop_identity_unverified';
  readonly message: string;
}

export interface DaemonHandle {
  readonly port: number;
  readonly password: string;
  readonly baseUrl: string;
  /** The password sent as `Authorization: Bearer <password>`. */
  readonly bearerToken: string;
  /** The RELEASE (serving daemon) pid once resolved; never the launcher's.
   * A pid whose announced digest does not match the daemon's own health
   * digest is NOT adopted here. */
  readonly pid: number | undefined;
  /** The spawned launcher/bootstrap pid (diagnostics only; it may exit long
   * before the daemon does). */
  readonly launcherPid: number | undefined;
  /** Non-null when the handshake announcement was refused or is only
   * tentatively adopted (see the code); the announced pid is then never used
   * to signal the process. */
  readonly releaseTrustError: DaemonTrustFailure | null;
  health(): Promise<DaemonHealth>;
  /** Bounded tail of the daemon's stderr (diagnostics only). */
  stderrTail(): string;
  /** True while the RELEASE daemon is alive AND answers /native/health. A
   * launcher exit is never daemon death. */
  alive(): Promise<boolean>;
  /** Graceful, identity-verified shutdown. Resolves once the signal was
   * issued (or refused); `stopRefusal` names a typed refusal. */
  stop(): Promise<void>;
  /** The typed reason stop() refused to signal, or null when it signaled (or
   * there was nothing to signal). Valid after stop() resolves. */
  readonly stopRefusal: DaemonTrustFailure | null;
}

let activeChild: ChildProcess | null = null;
let activeHandle: DaemonHandle | null = null;

export function findBinary(options: DaemonOptions): string {
  const env = options.binaryPath ?? process.env.FAKTOR_BIN;
  if (env && env.length > 0) {
    return env;
  }
  const root = options.workspaceRoot;
  const candidates = [
    join(root, 'target', 'debug', 'faktor-cli'),
    join(root, 'target', 'release', 'faktor-cli'),
  ];
  for (const candidate of candidates) {
    if (existsSync(candidate)) {
      return candidate;
    }
  }
  throw new Error(
    `faktor-cli binary not found (looked for ${candidates.join(', ')}; set FAKTOR_BIN or faktor.binaryPath to override)`,
  );
}

/** The install root this launcher resolves through. */
export function installRootFor(options: DaemonOptions): string {
  if (options.installRoot && options.installRoot.length > 0) {
    return options.installRoot;
  }
  if (options.dataDir && options.dataDir.length > 0) {
    return join(options.dataDir, 'install');
  }
  return join(homedir(), '.faktor', 'install');
}

/**
 * The stable bootstrap launcher when an install layout exists:
 * `<installRoot>/launcher` beside a `current` pointer or a `versions/`
 * directory. `null` when no layout is present, in which case the caller
 * falls back to the legacy binary resolution. The bootstrap itself refuses
 * an unsigned/tampered release or a digest mismatch (exit 3); it never
 * falls back to another binary.
 */
export function bootstrapBinary(options: DaemonOptions): string | null {
  const root = installRootFor(options);
  const launcher = join(root, 'launcher');
  if (!existsSync(launcher)) {
    return null;
  }
  if (!existsSync(join(root, 'current')) && !existsSync(join(root, 'versions'))) {
    return null;
  }
  return launcher;
}

/**
 * The executable to spawn: an explicit operator override first, then the
 * bootstrap launcher when an install layout exists, then the legacy
 * workspace lookup. Exactly one of those decides.
 */
export function resolveExecutable(options: DaemonOptions): string {
  const explicit = options.binaryPath ?? process.env.FAKTOR_BIN;
  if (explicit && explicit.length > 0) {
    return explicit;
  }
  return bootstrapBinary(options) ?? findBinary(options);
}

export async function isRunning(): Promise<boolean> {
  return activeHandle !== null && (await activeHandle.alive());
}

export function currentDaemon(): DaemonHandle | null {
  return activeHandle;
}

/**
 * Start (or return the already running) daemon. The returned handle owns
 * every child process it created; `stop()` is the only shutdown path.
 */
export async function startDaemon(options: DaemonOptions): Promise<DaemonHandle> {
  if (activeHandle) {
    if (await activeHandle.alive()) {
      return activeHandle;
    }
    // The previous daemon is gone (or unreachable): never leave its release
    // behind when a new one is started over it.
    await stopDaemon(activeHandle);
  }
  const bin = resolveExecutable(options);
  const password = crypto.randomBytes(32).toString('hex');
  const args = ['serve', '--port', '0'];
  if (options.dataDir && options.dataDir.length > 0) {
    args.push('--data-dir', options.dataDir);
  }
  if (options.extraArgs) {
    args.push(...options.extraArgs);
  }
  const child = spawn(bin, args, {
    env: { ...process.env, ...options.env, FAKTOR_SERVER_PASSWORD: password },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  const stderr = new BoundedTail(STDERR_TAIL_BYTES);
  child.stderr?.setEncoding('utf8');
  child.stderr?.on('data', (chunk: string) => stderr.push(chunk));
  const handshake = await readStartupHandshake(
    child,
    options.startupTimeoutMs ?? DEFAULT_STARTUP_TIMEOUT_MS,
  );
  const port = handshake.port;
  let stopped = false;
  let stopping: Promise<void> | null = null;
  let stopRefusal: DaemonTrustFailure | null = null;

  // Resolve the RELEASE pid before returning: the injected/platform probe
  // (port ownership) first, then the live child (unix exec, or a legacy
  // resident launcher still wrapping its release). The announced handshake
  // pid is only adopted after it agrees with the probe (when one answers) and
  // with the daemon's own health-reported digest.
  const probeReleasePid = async (): Promise<number | null> => {
    if (options.resolveReleasePid) {
      return normalizePid(await options.resolveReleasePid(port));
    }
    if (process.platform === 'win32') {
      return probeListeningPid(port);
    }
    return null;
  };
  const announcement = handshake.announcement();
  let announced: ReleaseAnnouncement | null = null;
  let releaseTrustError: DaemonTrustFailure | null = null;
  let currentPid: number | null;
  try {
    const probed = await probeReleasePid();
    if (announcement !== null) {
      if (probed !== null && probed !== announcement.pid) {
        releaseTrustError = {
          code: 'release_pid_probe_mismatch',
          message:
            `the release-pid handshake announced pid ${announcement.pid} but the port ${port} probe ` +
            `names pid ${probed}; the announced pid was not adopted`,
        };
      } else {
        const verdict = await verifyAnnouncementDigest(announcement, port, password);
        if (verdict === 'trusted') {
          announced = announcement;
        } else if (verdict === 'unreachable') {
          // The daemon is still bringing its HTTP surface up: adopt the
          // announcement tentatively. stop() re-verifies the digest before
          // signaling it, so a wrong pid can never be used.
          announced = announcement;
        } else {
          releaseTrustError = verdict;
        }
      }
    }
    currentPid = announced?.pid ?? probed ?? null;
  } catch (error) {
    // The resolution seam failed loudly: never leak the spawned launcher.
    stopChild(child);
    throw error;
  }
  if (currentPid === null && childAlive(child)) {
    currentPid = normalizePid(child.pid);
  }

  /**
   * Before ANY non-child pid is signalled: the daemon on this port must still
   * answer /native/health with our connection password (proving it is the
   * daemon we spawned, not a recycled pid), and when the pid came from the
   * handshake its announced digest must still equal the health-reported one.
   */
  const verifyStopTarget = async (target: number): Promise<DaemonTrustFailure | null> => {
    let report: DaemonHealth;
    try {
      report = await health(port, password);
    } catch (error) {
      return {
        code: 'stop_identity_unverified',
        message:
          `refusing to signal pid ${target}: the daemon at port ${port} did not answer /native/health ` +
          `(${error instanceof Error ? error.message : String(error)})`,
      };
    }
    if (!report.ok) {
      return {
        code: 'stop_identity_unverified',
        message: `refusing to signal pid ${target}: the daemon at port ${port} reports unhealthy`,
      };
    }
    if (announced !== null && target === announced.pid) {
      if (report.releaseDigest === null) {
        return {
          code: 'release_digest_absent',
          message:
            `refusing to signal announced pid ${target}: the daemon at port ${port} reports no release ` +
            `digest to confirm it`,
        };
      }
      if (report.releaseDigest !== announced.digest) {
        return {
          code: 'release_digest_mismatch',
          message:
            `refusing to signal announced pid ${target}: the daemon at port ${port} reports digest ` +
            `${report.releaseDigest}, not the announced ${announced.digest}`,
        };
      }
    }
    return null;
  };

  const handle: DaemonHandle = {
    port,
    password,
    baseUrl: `http://127.0.0.1:${port}`,
    bearerToken: password,
    get pid(): number | undefined {
      return currentPid ?? undefined;
    },
    get releaseTrustError(): DaemonTrustFailure | null {
      return releaseTrustError;
    },
    get stopRefusal(): DaemonTrustFailure | null {
      return stopRefusal;
    },
    launcherPid: normalizePid(child.pid) ?? undefined,
    health: () => health(port, password),
    stderrTail: () => stderr.text(),
    alive: async (): Promise<boolean> => {
      if (stopped) {
        return false;
      }
      if (currentPid === null) {
        // No handshake pid was latched before the startup line resolved (the
        // launcher printed none): a later platform probe can still name the
        // release; liveness never throws, the health probe below is the
        // fallback.
        try {
          currentPid = await probeReleasePid();
        } catch {
          currentPid = null;
        }
      }
      if (currentPid !== null && !processAlive(currentPid)) {
        return false;
      }
      try {
        const report = await health(port, password);
        return report.ok;
      } catch {
        return false;
      }
    },
    stop: (): Promise<void> => {
      if (stopping !== null) {
        return stopping;
      }
      stopping = (async () => {
        if (stopped) {
          return;
        }
        stopped = true;
        const childPid = normalizePid(child.pid);
        // "Our child" is only a safe identity while the child is STILL ALIVE:
        // after it exits the pid may have been recycled, so it must pass the
        // same health identity check as any other candidate.
        const childIsAlive = childPid !== null && childAlive(child);
        let target = currentPid;
        let targetIsOurChild =
          childIsAlive && currentPid !== null && currentPid === childPid;
        if (target === null && childIsAlive) {
          target = childPid;
          targetIsOurChild = true;
        }
        if (target === null) {
          // Last-ditch identity on Windows: the process that owns the
          // listening port; it still has to pass the health check below.
          const probed = probeListeningPidSync(port);
          if (probed !== null && processAlive(probed)) {
            const refusal = await verifyStopTarget(probed);
            if (refusal === null) {
              target = probed;
            } else {
              stopRefusal = refusal;
            }
          }
        }
        if (target !== null && !targetIsOurChild) {
          if (!processAlive(target)) {
            target = null;
          } else {
            const refusal = await verifyStopTarget(target);
            if (refusal !== null) {
              stopRefusal = refusal;
              target = null;
            }
          }
        }
        if (target !== null) {
          killPid(target, 'SIGTERM');
          const escalate = setTimeout(() => {
            if (processAlive(target)) {
              killPid(target, 'SIGKILL');
            }
          }, KILL_ESCALATION_MS);
          escalate.unref?.();
        } else if (stopRefusal === null && releaseTrustError !== null) {
          // Nothing was signalled and the only candidate was an already
          // refused announcement: surface that typed refusal as the stop
          // outcome instead of a silent no-op.
          stopRefusal = releaseTrustError;
        }
        // A legacy resident launcher (or an unix exec self) is the same live
        // process only on unix; on Windows this only cleans the wrapper, the
        // release above is the real target.
        stopChild(child);
        if (activeHandle === handle) {
          activeHandle = null;
          activeChild = null;
        }
      })();
      return stopping;
    },
  };
  activeChild = child;
  activeHandle = handle;
  return handle;
}

/**
 * Confirm the handshake announcement against the daemon's own health digest,
 * with a bounded retry while the HTTP surface comes up. `'unreachable'` means
 * no verdict could be obtained; anything else is a typed refusal.
 */
async function verifyAnnouncementDigest(
  announcement: ReleaseAnnouncement,
  port: number,
  password: string,
): Promise<DaemonTrustFailure | 'trusted' | 'unreachable'> {
  const deadline = Date.now() + ANNOUNCEMENT_VERIFY_MS;
  for (;;) {
    let report: DaemonHealth | null = null;
    try {
      report = await health(port, password);
    } catch {
      report = null;
    }
    if (report !== null && report.ok) {
      if (report.releaseDigest === null) {
        return {
          code: 'release_digest_absent',
          message:
            `the daemon at port ${port} reports no release digest while the launcher announced ` +
            `digest ${announcement.digest} for pid ${announcement.pid}; the announced pid was not adopted`,
        };
      }
      if (report.releaseDigest !== announcement.digest) {
        return {
          code: 'release_digest_mismatch',
          message:
            `the daemon at port ${port} reports release digest ${report.releaseDigest}, not the ` +
            `launcher-announced ${announcement.digest} for pid ${announcement.pid}; the announced pid ` +
            'was not adopted',
        };
      }
      return 'trusted';
    }
    if (Date.now() >= deadline) {
      return 'unreachable';
    }
    await new Promise<void>((resolve) => setTimeout(resolve, 100));
  }
}

/** SIGTERM, escalating to SIGKILL only when the child ignores it. */
export function stopDaemon(handle?: DaemonHandle | null): Promise<void> {
  const target = handle ?? activeHandle;
  if (target) {
    return target.stop();
  }
  if (activeChild) {
    stopChild(activeChild);
  }
  return Promise.resolve();
}

function stopChild(child: ChildProcess): void {
  if (child.exitCode !== null || child.signalCode !== null) {
    if (activeChild === child) {
      activeChild = null;
      activeHandle = null;
    }
    return;
  }
  try {
    child.kill('SIGTERM');
  } catch {
    // Already gone.
  }
  const escalate = setTimeout(() => {
    if (child.exitCode === null && child.signalCode === null) {
      try {
        child.kill('SIGKILL');
      } catch {
        // Already gone.
      }
    }
  }, KILL_ESCALATION_MS);
  escalate.unref?.();
  child.once('exit', () => {
    clearTimeout(escalate);
  });
}

/** True when `pid` names a live process (EPERM proves existence). */
export function processAlive(pid: number): boolean {
  if (!Number.isInteger(pid) || pid <= 0) {
    return false;
  }
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === 'EPERM';
  }
}

function childAlive(child: ChildProcess): boolean {
  return child.exitCode === null && child.signalCode === null;
}

function killPid(pid: number, signal: NodeJS.Signals): void {
  try {
    process.kill(pid, signal);
  } catch {
    // Already gone.
  }
}

function normalizePid(value: unknown): number | null {
  return typeof value === 'number' && Number.isInteger(value) && value > 0 ? value : null;
}

/** One latched release-pid handshake: the announced pid AND its digest. */
interface ReleaseAnnouncement {
  readonly pid: number;
  readonly digest: string;
}

interface LaunchHandshake {
  readonly port: number;
  /** The full announced release identity (pid + digest), or null when the
   * launcher printed no handshake line (the line is latched: first parse
   * wins). */
  announcement(): ReleaseAnnouncement | null;
}

/**
 * The release pid announced by the launcher's frozen handshake line. The
 * line is written before the startup line, so the announcement is latched by
 * the time the startup line resolves; the listener is simply left attached
 * (with a bounded buffer) instead of being torn down.
 */
function readStartupHandshake(child: ChildProcess, timeoutMs: number): Promise<LaunchHandshake> {
  return new Promise<LaunchHandshake>((resolvePort, reject) => {
    let buffer = '';
    let settled = false;
    let announcement: ReleaseAnnouncement | null = null;
    const timer = setTimeout(() => {
      if (!settled) {
        settled = true;
        reject(new Error(`timed out waiting for the daemon startup line (${timeoutMs}ms)`));
        stopChild(child);
      }
    }, timeoutMs);
    const finish = (err: Error | null, port?: number): void => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timer);
      if (err) {
        reject(err);
        stopChild(child);
      } else if (port !== undefined) {
        resolvePort({ port, announcement: () => announcement });
      }
    };
    child.on('error', (err) => finish(err));
    child.on('exit', (code, signal) => {
      if (!settled) {
        finish(
          new Error(
            `daemon exited before the startup line (code=${code ?? 'null'} signal=${signal ?? 'null'})`,
          ),
        );
      }
    });
    child.stdout?.setEncoding('utf8');
    child.stdout?.on('data', (chunk: string) => {
      buffer += chunk;
      if (buffer.length > LAUNCHER_STDOUT_BUFFER_BYTES) {
        buffer = buffer.slice(-LAUNCHER_STDOUT_BUFFER_BYTES);
      }
      let idx: number;
      while ((idx = buffer.indexOf('\n')) >= 0) {
        const line = buffer.slice(0, idx).replace(/\r$/, '');
        buffer = buffer.slice(idx + 1);
        if (announcement === null) {
          const pidMatch = RELEASE_PID_LINE.exec(line);
          if (pidMatch) {
            const pid = normalizePid(Number(pidMatch[1]));
            const digest = pidMatch[2]!;
            if (pid !== null) {
              announcement = { pid, digest };
            }
          }
        }
        if (!settled) {
          const match = STARTUP_LINE.exec(line);
          if (match) {
            finish(null, Number(match[1]));
          }
        }
      }
    });
  });
}

/**
 * The pid that owns the daemon's listening port. Windows has no cheap
 * node-native socket-owner lookup, so the probe shells out to the system
 * tools; every other platform is covered by the exec'd child pid.
 */
async function probeListeningPid(port: number): Promise<number | null> {
  if (process.platform !== 'win32') {
    return null;
  }
  const script =
    `(Get-NetTCPConnection -LocalPort ${port} -State Listen -ErrorAction Stop | ` +
    `Select-Object -First 1 -ExpandProperty OwningProcess)`;
  const fromPowerShell = await runCommand(
    'powershell.exe',
    ['-NoProfile', '-NonInteractive', '-Command', script],
    PID_PROBE_TIMEOUT_MS,
    PID_PROBE_MAX_BYTES,
  );
  const pid = parsePidOutput(fromPowerShell);
  if (pid !== null) {
    return pid;
  }
  const netstat = await runCommand(
    'netstat.exe',
    ['-ano', '-p', 'tcp'],
    PID_PROBE_TIMEOUT_MS,
    PID_PROBE_MAX_BYTES,
  );
  return parseNetstatListenerPid(netstat, port);
}

/** The last-ditch synchronous probe used by stop() when no pid was resolved. */
function probeListeningPidSync(port: number): number | null {
  if (process.platform !== 'win32') {
    return null;
  }
  try {
    const out = execFileSync('netstat.exe', ['-ano', '-p', 'tcp'], {
      timeout: PID_PROBE_TIMEOUT_MS,
      maxBuffer: PID_PROBE_MAX_BYTES,
      windowsHide: true,
    }).toString('utf8');
    return parseNetstatListenerPid(out, port);
  } catch {
    return null;
  }
}

function runCommand(
  program: string,
  args: readonly string[],
  timeoutMs: number,
  maxBytes: number,
): Promise<string | null> {
  return new Promise<string | null>((resolveRun) => {
    execFile(
      program,
      args.slice(),
      { timeout: timeoutMs, maxBuffer: maxBytes, windowsHide: true },
      (error, stdout) => {
        if (typeof stdout === 'string' && stdout.length > 0) {
          resolveRun(stdout);
          return;
        }
        resolveRun(error ? null : '');
      },
    );
  });
}

/** One bare pid line (PowerShell prints `\r\n` and may pad). */
function parsePidOutput(output: string | null): number | null {
  if (!output) {
    return null;
  }
  for (const raw of output.split(/\r?\n/)) {
    const line = raw.trim();
    if (/^\d+$/.test(line)) {
      const pid = normalizePid(Number(line));
      if (pid !== null) {
        return pid;
      }
    }
  }
  return null;
}

/** The owning pid of `127.0.0.1:<port>` (or `[::1]:<port>`) in LISTENING. */
function parseNetstatListenerPid(output: string | null, port: number): number | null {
  if (!output) {
    return null;
  }
  for (const raw of output.split(/\r?\n/)) {
    const columns = raw.trim().split(/\s+/);
    if (
      columns.length >= 5 &&
      columns[0]!.toUpperCase() === 'TCP' &&
      columns[1]!.endsWith(`:${port}`) &&
      columns[3]!.toUpperCase() === 'LISTENING'
    ) {
      const pid = normalizePid(Number(columns[4]));
      if (pid !== null) {
        return pid;
      }
    }
  }
  return null;
}

function health(port: number, password: string): Promise<DaemonHealth> {
  return new Promise<DaemonHealth>((resolveHealth, reject) => {
    const req = http.request(
      {
        host: '127.0.0.1',
        port,
        path: '/native/health',
        method: 'GET',
        headers: { Authorization: `Bearer ${password}` },
        timeout: HEALTH_TIMEOUT_MS,
      },
      (res) => {
        const chunks: Buffer[] = [];
        let size = 0;
        res.on('data', (chunk: Buffer) => {
          size += chunk.length;
          if (size > MAX_HEALTH_BODY_BYTES) {
            res.destroy();
            reject(new Error('health check failed: response body exceeded the bound'));
            return;
          }
          chunks.push(chunk);
        });
        res.on('end', () => {
          const status = res.statusCode ?? 0;
          if (status === 401) {
            reject(new Error('health check failed: 401 unauthorized'));
            return;
          }
          if (status !== 200) {
            reject(new Error(`health check failed: HTTP ${status || 'unknown'}`));
            return;
          }
          let parsed: unknown;
          try {
            parsed = JSON.parse(Buffer.concat(chunks).toString('utf8'));
          } catch {
            reject(new Error('health check failed: response was not JSON'));
            return;
          }
          if (
            typeof parsed !== 'object' ||
            parsed === null ||
            typeof (parsed as { ok?: unknown }).ok !== 'boolean' ||
            typeof (parsed as { version?: unknown }).version !== 'string'
          ) {
            reject(new Error('health check failed: unexpected response shape'));
            return;
          }
          const version = (parsed as { version: string }).version;
          resolveHealth({
            ok: (parsed as { ok: boolean }).ok,
            version,
            // The daemon folds the bootstrap-verified release digest into the
            // health version (`<pkg>+release.<id>.<digest>`). Absence is an
            // honest null, never a guess.
            releaseDigest: releaseDigestOf(version),
          });
        });
      },
    );
    req.on('timeout', () => {
      req.destroy(new Error('health check failed: timeout'));
    });
    req.on('error', (err) => reject(err));
    req.end();
  });
}

/** The release digest folded into a daemon health version, if any. */
export function releaseDigestOf(version: string): string | null {
  const match = /\+release\.([A-Za-z0-9._+-]+)\.([0-9a-f]{64})$/.exec(version);
  return match ? match[2]! : null;
}

/** A byte-bounded ring buffer (no unbounded stderr in RAM). */
class BoundedTail {
  private readonly chunks: string[] = [];
  private size = 0;
  private readonly limit: number;

  constructor(limit: number) {
    this.limit = limit;
  }

  push(chunk: string): void {
    this.chunks.push(chunk);
    this.size += chunk.length;
    while (this.size > this.limit && this.chunks.length > 1) {
      const dropped = this.chunks.shift();
      if (dropped === undefined) {
        break;
      }
      this.size -= dropped.length;
    }
    if (this.chunks.length === 1 && this.chunks[0]!.length > this.limit) {
      this.chunks[0] = this.chunks[0]!.slice(-this.limit);
      this.size = this.chunks[0]!.length;
    }
  }

  text(): string {
    return this.chunks.join('');
  }
}
