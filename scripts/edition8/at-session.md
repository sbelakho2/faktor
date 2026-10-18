# Edition 8 assistive-technology session protocol

This is the manual protocol behind the `edition8-assistive` gate. The gate's
automated half (role, accessible name, keyboard reachability for all 200
controls in both hosts) always runs. The AT half is certified only by a
recorded human session; otherwise `tests/edition8/assistive.mjs` records a
typed `not_certified` boundary with the probe evidence.

## Required sessions

| AT | Platform | Host | Required flows |
|---|---|---|---|
| NVDA (current stable) | Windows 11 | VS Code | W07 approval, W14 restore/fork, W16 recovery, A39 compact |
| VoiceOver | macOS | VS Code and JetBrains | W07, W14, W16, A39 |
| Orca | Ubuntu | VS Code | W07, W14, W16, A39 |

NVDA + Orca are the only two certified AT products for VS Code because they
are the platforms the family supports; VoiceOver covers both hosts on macOS.
A host/platform without a session stays a typed boundary, never a pass.

## Per-session checks

1. Start the host with the Edition 8 fixture state; enable the screen reader's
   speech and braille output.
2. Traverse W07/W14/W16/A39 with keyboard only (Tab/Shift-Tab, Enter/Space,
   Escape). Record the announcement transcript and focus path.
3. Assert every state change is announced once; token-by-token streaming is
   not announced; an approval is announced without stealing focus.
4. Assert focus stays visible, is never trapped, and is not hidden under
   sticky content; a disappearing item moves to the nearest stable sibling or
   heading; closing a detail returns to the exact trigger; IME composition
   never submits.
5. Save the transcript, focus path, host build, AT version, flow result and
   evidence clip/sha256 as `tests/edition8/assistive/{at}/{flow}.json`.
6. Re-run `node tests/edition8/assistive.mjs`; a session record with
   `status: certified` replaces the corresponding typed boundary.

## What is already automated

`apps/vscode/scripts/edition8-control-exercise.mjs` asserts one accessible
name, a native role and a focusable element per control in the real rendered
DOM; `Edition8Evidence.kt` asserts the same through the real Swing
`AccessibleContext` for all 200 controls. The visual matrix separately asserts
a live status region, a focus ring, no unnamed controls and no sub-24 px
targets at 240/320/480 + 200% + high contrast.
