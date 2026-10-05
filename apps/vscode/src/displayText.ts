// Display-text hardening shared by every panel projection.
//
// Two adversarial classes are handled here:
// - UTF-16 truncation must never split a surrogate pair: a cut mid-pair
//   renders as U+FFFD and corrupts the visible line;
// - C0/C1 control characters (except tab/newline, which some panels keep as
//   line structure) and explicit bidi embedding/override controls are
//   stripped: they are invisible layout/spoofing vectors, never part of a
//   human label. RTL letters and shaping are untouched.

const BIDI_CONTROLS = /[\u200e\u200f\u202a-\u202e\u2066-\u2069]/g;
const CONTROL_CHARS = /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/g;

/** Strip invisible control/override characters from a display string. */
export function stripDisplayControls(value: string): string {
  return value.replace(BIDI_CONTROLS, '').replace(CONTROL_CHARS, '');
}

/** Slice at `max` UTF-16 units, never splitting a surrogate pair. */
export function safeSlice(value: string, max: number): string {
  if (max <= 0) {
    return '';
  }
  if (value.length <= max) {
    return value;
  }
  const preceding = value.charCodeAt(max - 1);
  const next = value.charCodeAt(max);
  const splitsPair =
    preceding >= 0xd800 && preceding <= 0xdbff && next >= 0xdc00 && next <= 0xdfff;
  return value.slice(0, splitsPair ? max - 1 : max);
}

/** True when the string contains no unpaired surrogate code unit. */
export function hasLoneSurrogate(value: string): boolean {
  for (let index = 0; index < value.length; index += 1) {
    const unit = value.charCodeAt(index);
    if (unit >= 0xd800 && unit <= 0xdbff) {
      const next = index + 1 < value.length ? value.charCodeAt(index + 1) : -1;
      if (!(next >= 0xdc00 && next <= 0xdfff)) {
        return true;
      }
      index += 1;
    } else if (unit >= 0xdc00 && unit <= 0xdfff) {
      return true;
    }
  }
  return false;
}
