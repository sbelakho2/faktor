// On-the-fly TypeScript loader for gates that exercise apps/vscode sources
// under a Node without native type stripping. The repo's selftest imports
// ../src/*.ts directly, so the mutation gates for the VS Code invariants run:
//
//   node --no-warnings --experimental-loader \
//     ./scripts/mutations/vscode-ts-loader.mjs apps/vscode/scripts/selftest.mjs
//
// Resolution of the transpiler is ordered, offline, version-checked and
// fail-closed:
//
//   1. the app's own pinned TypeScript next to the loader — the developer
//      checkout, or the mutation scratch snapshot the support provisioning
//      places at the same relative path;
//   2. the ORIGINAL checkout through FAKTOR_MUTATION_SOURCE_ROOT, which
//      scripts/mutation-run.mjs always exports: a bare scratch copy has no
//      node_modules (it is not committed), so the pinned dependency must
//      resolve read-only from the trusted checkout rather than fail;
//      a resolved TypeScript must satisfy the `typescript` range the SAME
//      apps/vscode/package.json declares — a mismatched transpiler is
//      refused with a typed environment error instead of silently changing
//      the gate's semantics;
//   3. Node's NATIVE type stripping via nextLoad (Node >= 23.6): the selftest
//      is documented to run with no npm install on such hosts;
//   4. otherwise a TYPED environment error (`faktor-mutation-env:`) naming
//      the missing package, the pinned range and the exact install command:
//      the gate fails loudly and the mutation runner classifies it as
//      environment-missing. A missing dependency is NEVER a detection.
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const INSTALL_COMMAND = 'npm ci  (run inside apps/vscode, or: npm ci --prefix apps/vscode)';
const NATIVE_STRIP_MAJOR = 23;
const NATIVE_STRIP_MINOR = 6;

/** TRUE when this runtime can strip erasable TypeScript natively. */
function nodeStripsTypes() {
  const [major, minor] = process.versions.node.split('.').map((part) => Number(part));
  return (
    major > NATIVE_STRIP_MAJOR || (major === NATIVE_STRIP_MAJOR && minor >= NATIVE_STRIP_MINOR)
  );
}

/** The candidate apps/vscode roots in resolution order (scratch, then source). */
function appRoots() {
  const roots = [fileURLToPath(new URL('../../apps/vscode', import.meta.url))];
  const sourceRoot = process.env.FAKTOR_MUTATION_SOURCE_ROOT;
  if (typeof sourceRoot === 'string' && sourceRoot.trim() !== '') {
    roots.push(join(sourceRoot, 'apps', 'vscode'));
  }
  return roots;
}

/** The `typescript` range declared by `root`'s package.json, or null. */
function pinnedRange(root) {
  try {
    const pkg = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'));
    const range = pkg?.devDependencies?.typescript;
    return typeof range === 'string' && range.trim() !== '' ? range.trim() : null;
  } catch {
    return null;
  }
}

/** `[major, minor, patch]` of a version string, or null when unparseable. */
function parseVersion(text) {
  const match = /^v?(\d+)\.(\d+)\.(\d+)/.exec(String(text).trim());
  return match === null ? null : match.slice(1).map((part) => Number(part));
}

function atLeast(version, floor) {
  for (let i = 0; i < 3; i += 1) {
    if (version[i] !== floor[i]) return version[i] > floor[i];
  }
  return true;
}

/**
 * `true`/`false` when `version` provably satisfies `range`; `null` when the
 * range uses a form this loader cannot check (the caller fails closed).
 * Supported: `x.y.z`, `=x.y.z`, `^x.y.z`, `~x.y.z`, `>=x.y.z`.
 */
function satisfiesRange(versionText, rangeText) {
  const version = parseVersion(versionText);
  const range = String(rangeText).trim();
  const match = /^(?:([\^~=]|>=))?(\d+)\.(\d+)\.(\d+)$/.exec(range);
  if (version === null || match === null) return null;
  const [, operator, rawMajor, rawMinor, rawPatch] = match;
  const floor = [rawMajor, rawMinor, rawPatch].map((part) => Number(part));
  if (!atLeast(version, floor)) return false;
  const major = Number(rawMajor);
  const minor = Number(rawMinor);
  if (operator === '^') {
    if (major > 0) return version[0] === major;
    if (minor > 0) return version[0] === 0 && version[1] === minor;
    return version[0] === 0 && version[1] === 0 && version[2] === floor[2];
  }
  if (operator === '~') {
    return version[0] === major && version[1] === minor;
  }
  if (operator === undefined || operator === '=') {
    return version[0] === floor[0] && version[1] === floor[1] && version[2] === floor[2];
  }
  return true; // `>=`
}

/**
 * Resolve the pinned transpiler. Returns `{ ts }` on success or
 * `{ refusal }` with a typed, actionable environment error.
 */
function resolveTypeScript() {
  const reasons = [];
  const roots = appRoots();
  let pinned = null;
  for (const root of roots) {
    pinned = pinned ?? pinnedRange(root);
  }
  for (const root of roots) {
    let ts = null;
    try {
      ts = createRequire(join(root, 'package.json'))('typescript');
    } catch {
      // This root does not carry the dependency; try the next one.
    }
    if (ts === null) continue;
    const range = pinnedRange(root);
    if (range === null) {
      // The dependency resolved but no pin is readable next to it: accept
      // the app tree's own package (the resolved module is the app's).
    } else {
      const satisfied = satisfiesRange(ts.version, range);
      if (satisfied === null) {
        return {
          refusal:
            `faktor-mutation-env: the mutation gate resolved typescript@${ts.version} from ` +
            `${root} but apps/vscode pins typescript@"${range}", a range form this gate ` +
            `loader cannot verify; refusing to run with an unverified transpiler — update ` +
            `scripts/mutations/vscode-ts-loader.mjs or pin a caret/tilde/exact/>= range`,
        };
      }
      if (!satisfied) {
        reasons.push(`resolved typescript@${ts.version} at ${root} does not satisfy the pinned range ${range}`);
        continue;
      }
    }
    return { ts };
  }
  const pinText = pinned !== null ? `typescript@"${pinned}"` : 'typescript';
  if (nodeStripsTypes()) {
    // Documented no-install path: a modern runtime strips the erasable
    // TypeScript natively; nothing to resolve.
    return { ts: null };
  }
  const detail =
    reasons.length > 0
      ? `the checkout carries only mismatched transpilers (${reasons.join('; ')})`
      : pinned === null
        ? 'no apps/vscode/package.json carries a readable `typescript` pin'
        : 'no apps/vscode tree on this host carries node_modules/typescript';
  return {
    refusal:
      `faktor-mutation-env: the pinned apps/vscode dependency ${pinText} is unavailable to the ` +
      `mutation gate and this Node runtime (${process.versions.node}) cannot strip types natively ` +
      `(needs >= ${NATIVE_STRIP_MAJOR}.${NATIVE_STRIP_MINOR}); install the pinned toolchain with ` +
      `\`${INSTALL_COMMAND}\` so the gate resolves it read-only from the checkout — ${detail}. ` +
      `A missing dependency is an environment failure, never a detection`,
  };
}

const resolution = resolveTypeScript();
const ts = resolution.ts ?? null;

export async function load(url, context, nextLoad) {
  if (url.endsWith('.ts')) {
    if (ts === null) {
      if (resolution.refusal !== undefined) {
        throw new Error(resolution.refusal);
      }
      // No pinned transpiler anywhere: defer to the runtime's native type
      // stripping. On a Node without it (or for non-erasable syntax) this
      // throws and the gate fails as a typed environment error.
      try {
        return await nextLoad(url, context);
      } catch (error) {
        throw new Error(
          `faktor-mutation-env: TypeScript is unavailable for apps/vscode and this Node runtime ` +
            `(${process.versions.node}) could not load the module natively; install the pinned ` +
            `dependencies with \`${INSTALL_COMMAND}\` or use Node >= ` +
            `${NATIVE_STRIP_MAJOR}.${NATIVE_STRIP_MINOR} — a missing dependency is an ` +
            `environment failure, never a detection (${error.message})`,
        );
      }
    }
    const file = fileURLToPath(url);
    const out = ts.transpileModule(readFileSync(file, 'utf8'), {
      compilerOptions: {
        module: ts.ModuleKind.ESNext,
        target: ts.ScriptTarget.ES2022,
        esModuleInterop: true,
        isolatedModules: true,
      },
      fileName: file,
    }).outputText;
    return { format: 'module', source: out, shortCircuit: true };
  }
  return nextLoad(url, context);
}
