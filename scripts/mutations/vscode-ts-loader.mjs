// On-the-fly TypeScript loader for gates that exercise apps/vscode sources
// under a Node without native type stripping (this host: Node 20). The repo's
// selftest imports ../src/*.ts directly, so the mutation gates for the VS Code
// invariants run:
//
//   node --no-warnings --experimental-loader \
//     ./scripts/mutations/vscode-ts-loader.mjs apps/vscode/scripts/selftest.mjs
//
// It transpiles each .ts module with the app's own pinned TypeScript, mutating
// nothing on disk.
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const require = createRequire(new URL('../../apps/vscode/package.json', import.meta.url));
const ts = require('typescript');

export async function load(url, context, nextLoad) {
  if (url.endsWith('.ts')) {
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
