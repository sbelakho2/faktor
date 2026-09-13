// Harness-side resolver hook: runs the UNMODIFIED upstream TypeScript client
// (whose NodeNext sources import "./x.js" for "./x.ts") on stock Node >=22
// type stripping. Nothing inside compat/kilo-v756/upstream-sdk is edited.
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { registerHooks } from "node:module";

registerHooks({
  resolve(specifier, context, nextResolve) {
    if (
      context.parentURL &&
      (specifier.startsWith("./") || specifier.startsWith("../")) &&
      specifier.endsWith(".js")
    ) {
      const candidate = new URL(specifier.slice(0, -3) + ".ts", context.parentURL);
      if (existsSync(fileURLToPath(candidate))) {
        return { url: candidate.href, shortCircuit: true };
      }
    }
    return nextResolve(specifier, context);
  },
});
