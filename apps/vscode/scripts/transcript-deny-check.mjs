#!/usr/bin/env node
// Regression: a denied tool call is written by the daemon as its own
// assistant message with a typed tool_result part. The transcript renderer
// must merge that result by toolCallId into the original call and drop the
// result-only message; otherwise the panel shows "read_file done" next to
// an empty "(unnamed tool) exit 1" bubble (a false success for a refusal).

import assert from 'node:assert/strict';

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
          exit_code: 1,
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
assert.equal(entries[0].tools[0].exitCode, 1, 'the denial outcome is attached to the original call');
assert.match(entries[0].tools[0].excerpt ?? '', /permission_denied/, 'the typed denial is visible');
console.log('transcript denial merge: PASS (1 call, exit 1, merged refusal)');
