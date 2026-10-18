#!/usr/bin/env node
// Regression: a denied tool call is written by the daemon as its own
// assistant message with a typed tool_result part. The transcript renderer
// must merge that result by toolCallId into the original call and drop the
// result-only message; otherwise the panel shows "read_file done" next to
// an empty "(unnamed tool) exit 1" bubble (a false success for a refusal).

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { transcriptFromMessages } from '../src/state.ts';

// Newest-first, exactly as a native message page is served.
const messages = [
  {
    id: '4',
    role: 'assistant',
    seq: 4,
    created_ms: 4,
    parts: [
      {
        kind: 'tool_result',
        data: {
          tool_call_id: 'call_1',
          excerpt: 'tool call denied (permission_denied): permission denied: read_file',
          // The reported defect: a denial carries text but NO exit code.
          exit_code: null,
          artifact: null,
        },
      },
    ],
  },
  {
    id: '3',
    role: 'assistant',
    seq: 3,
    created_ms: 3,
    parts: [
      {
        kind: 'tool_call',
        data: {
          tool_call_id: 'call_1',
          name: 'read_file',
          state: 'completed',
          input: { path: 'lib.rs' },
        },
      },
    ],
  },
];

const entries = transcriptFromMessages(messages);
assert.equal(entries.length, 1, 'the result-only assistant message is merged away');
assert.equal(entries[0].tools.length, 1, 'exactly one tool node survives');
assert.equal(entries[0].tools[0].name, 'read_file', 'the original tool name is retained');
assert.equal(entries[0].tools[0].exitCode, null, 'a denial may carry no exit code');
assert.match(entries[0].tools[0].excerpt ?? '', /permission_denied/, 'the typed denial is visible');
// The RENDERER policy is the actual report row: state=completed + text + no
// exit code must paint the refusal, not "done". Extract the real
// `toolOutcome` from media/chat.js (the shipped webview source) and run it.
const chatSource = readFileSync(new URL('../media/chat.js', import.meta.url), 'utf8');
const start = chatSource.indexOf('function toolOutcome(tool) {');
assert.notEqual(start, -1, 'media/chat.js still defines toolOutcome(tool)');
let depth = 0;
let end = -1;
for (let i = chatSource.indexOf('{', start); i < chatSource.length; i += 1) {
  if (chatSource[i] === '{') depth += 1;
  if (chatSource[i] === '}') {
    depth -= 1;
    if (depth === 0) {
      end = i + 1;
      break;
    }
  }
}
assert.notEqual(end, -1, 'toolOutcome(tool) has a balanced body');
const toolOutcome = new Function(`${chatSource.slice(start, end)}; return toolOutcome;`)();
const denied = entries[0].tools[0];
assert.deepEqual(toolOutcome(denied), { tone: 'fail', label: 'denied' }, 'a denial is never "done"');
const succeeded = { exitCode: null, state: 'completed', excerpt: 'file contents', artifact: null };
assert.deepEqual(toolOutcome(succeeded), { tone: 'pass', label: 'done' }, 'a real no-exit result still completes');
console.log('transcript denial merge: PASS (1 call, no exit code, refused not done)');
