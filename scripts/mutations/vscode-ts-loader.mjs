// On-the-fly TypeScript loader for gates that exercise apps/vscode sources
// under a Node without native type stripping. The repo's selftest imports
// ../src/*.ts directly, so the mutation gates for the VS Code invariants run:
//
//   node --no-warnings --experimental-loader \
//     ./scripts/mutations/vscode-ts-loader.mjs apps/vscode/scripts/selftest.mjs
//
// Resolution of the transpiler is ordered, offline, and fail-closed:
//
//   1. the app's own pinned TypeScript next to the loader — the developer
//      checkout, or the mutation scratch snapshot the support provisioning
//      places at the same relative path;
//   2. the ORIGINAL checkout through FAKTOR_MUTATION_SOURCE_ROOT, which
//      scripts/mutation-run.mjs always exports: a bare scratch copy has no
//      node_modules (it is not committed), so the pinned dependency must
//      resolve read-only from the trusted checkout rather than fail;
//   3. Node's NATIVE type stripping via nextLoad (Node >= 23.6): the selftest
//      is documented to run with no npm install on such hosts;
//   4. otherwise a TYPED environment error (`faktor-mutation-env:`): the gate
//      fails loudly and the mutation runner classifies it as
//      environment-missing. A missing dependency is NEVER a detection.
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

/** The app's pinned TypeScript, or null when no candidate carries it. */
function resolveTypeScript() {
  const candidates = [fileURLToPath(new URL('../../apps/vscode/package.json', import.meta.url))];
  const sourceRoot = process.env.FAKTOR_MUTATION_SOURCE_ROOT;
  if (typeof sourceRoot === 'string' && sourceRoot.trim() !== '') {
    candidates.push(join(sourceRoot, 'apps', 'vscode', 'package.json'));
  }
  for (const candidate of candidates) {
    try {
      return createRequire(candidate)('typescript');
    } catch {
      // This root does not carry the dependency; try the next one.
    }
  }
  return null;
}

const ts = resolveTypeScript();

export async function load(url, context, nextLoad) {
  if (url.endsWith('.ts')) {
    if (ts === null) {
      // No pinned transpiler anywhere: defer to the runtime's native type
      // stripping. On a Node without it (or for non-erasable syntax) this
      // throws and the gate fails as a typed environment error.
      try {
        return await nextLoad(url, context);
      } catch (error) {
        throw new Error(
          `faktor-mutation-env: TypeScript is unavailable for apps/vscode and this Node runtime ` +
            `(${process.version}) cannot strip types natively; install the pinned dependencies ` +
            `(\`npm ci\` in apps/vscode) or use Node >= 23.6 — a missing dependency is an ` +
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
