// Exact micro-USD money for the Faktor Native Protocol.
//
// The daemon serializes monetary `*_micro` fields as decimal STRINGS because
// a JavaScript `number` is only integer-exact up to 2^53-1 while the wire
// range is u64 up to i64::MAX. This module is the ONE money authority of the
// extension: money is a `bigint` (exact arithmetic, exact decimal rendering,
// never a floating-point operation), the string form and the legacy number
// form are BOTH accepted, and a legacy number that is not exactly
// representable (above 2^53-1) is refused by the validators rather than
// silently rounded — the sender must serialize it as a decimal string.
//
// Rendering is canonical decimal only: `bigint.toString()` never emits
// scientific notation, so a displayed amount is always the exact wire value.

/** An exact non-negative micro-USD amount (the protocol's money domain). */
export type MicroMoney = bigint;

export const MICRO_ZERO: MicroMoney = 0n;

/**
 * The protocol's credit-amount bound (grant/input amounts): i64::MAX
 * micro-USD. The daemon refuses a larger amount typed, so the client never
 * constructs one.
 */
export const MICRO_I64_MAX: MicroMoney = 9223372036854775807n;

/**
 * The full money domain the daemon serializes: u64 micro-USD. Response folds
 * are u64 (a sum of i64::MAX-bounded grants can exceed i64::MAX), so parsing
 * accepts the whole range exactly — display and arithmetic stay exact.
 */
export const MICRO_U64_MAX: MicroMoney = 18446744073709551615n;

/** 2^53-1: the largest integer a JSON `number` represents exactly. */
export const MICRO_MAX_SAFE_NUMBER: MicroMoney = 9007199254740991n;

/** Micro-USD units in one USD (display scaling only, never a wire value). */
const MICRO_PER_USD = 1_000_000n;

/** Four display decimals, scaled by this factor. */
const DISPLAY_SCALE = 10_000n;

/** A canonical decimal integer string (optional leading zeros tolerated). */
const DECIMAL_PATTERN = /^[0-9]+$/;

/**
 * Parse one decimal-string money value. Returns null for anything that is
 * not a plain non-negative decimal integer within the protocol range
 * (no sign, no fraction, no exponent, no whitespace, no separators).
 */
export function microFromDecimal(text: string): MicroMoney | null {
  if (!DECIMAL_PATTERN.test(text)) {
    return null;
  }
  const value = BigInt(text);
  return value <= MICRO_U64_MAX ? value : null;
}

/**
 * Convert one legacy JSON `number` money value. Returns null unless it is a
 * non-negative integer exactly representable as a double (`Number.isSafeInteger`,
 * i.e. <= 2^53-1): a larger number has ALREADY lost precision by the time the
 * client sees it, so it must be refused, never rounded.
 */
export function microFromNumber(value: number): MicroMoney | null {
  if (!Number.isSafeInteger(value) || value < 0) {
    return null;
  }
  return BigInt(value);
}

/** The canonical decimal wire/display form (never scientific notation). */
export function microToString(value: MicroMoney): string {
  return value.toString();
}

/** Exact micro-unit display: `1234567µ$`. */
export function microText(value: MicroMoney): string {
  return `${value.toString()}\u00b5$`;
}

/**
 * Exact USD display with four decimals (the legacy `toFixed(4)` rendering,
 * but computed in bigint with round-half-up, so no float ever touches money).
 * `1234567n` -> `"1.2346"`.
 */
export function microUsdText(value: MicroMoney): string {
  const negative = value < MICRO_ZERO;
  const magnitude = negative ? -value : value;
  let whole = magnitude / MICRO_PER_USD;
  const remainder = magnitude % MICRO_PER_USD;
  let fraction = (remainder * DISPLAY_SCALE + MICRO_PER_USD / 2n) / MICRO_PER_USD;
  if (fraction >= DISPLAY_SCALE) {
    whole += 1n;
    fraction -= DISPLAY_SCALE;
  }
  return `${negative ? '-' : ''}${whole.toString()}.${fraction
    .toString()
    .padStart(4, '0')}`;
}

/**
 * The request-body projection of one exact amount: a plain JSON number while
 * that is lossless (<= 2^53-1, byte-compatible with the legacy daemon), else
 * the exact decimal string. Never a silently rounded number.
 */
export function microWireValue(value: MicroMoney): number | string {
  return value <= MICRO_MAX_SAFE_NUMBER ? Number(value) : value.toString();
}

/** Exact sum of money values (aggregations never touch a float). */
export function sumMicro(values: Iterable<MicroMoney>): MicroMoney {
  let total = MICRO_ZERO;
  for (const value of values) {
    total += value;
  }
  return total;
}

/**
 * The server's saturating balance rule: grants + refunds − consumed, floored
 * at zero (never a negative balance, computed exactly).
 */
export function microBalance(
  granted: MicroMoney,
  refunded: MicroMoney,
  consumed: MicroMoney,
): MicroMoney {
  const credits = granted + refunded;
  return credits < consumed ? MICRO_ZERO : credits - consumed;
}

/**
 * TRUE when a limit NAME denotes a money limit. The entitlement snapshot is a
 * name->value map whose names carry their unit: every `*_micro` name is money
 * and every other name is a plain counter. Both are exact integers; the
 * suffix decides only how the value is rendered.
 */
export function isMicroLimitName(name: string): boolean {
  return name.endsWith('_micro');
}
