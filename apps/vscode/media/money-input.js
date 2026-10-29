// Per-run cost budget input policy, shared by the built-in webview and the
// selftest (the same dual-format pattern as composer-state.js).
//
// Audit item 12: the dollar-labeled budget field must parse with EXACT
// integer arithmetic — `5`, `5.0`, `5.00` and `$5.00` all mean five
// dollars and convert to exactly "5000000" micro-USD. Negative values,
// exponents, non-finite words, overflowing amounts and ambiguous
// separators (thousands grouping, a locale decimal comma, more than six
// fraction digits) are TYPED refusals, never silently coerced. Raw
// micro-USD entry stays behind the explicit advanced surface (the
// "raw micro-USD" checkbox); it is never the default interpretation of a
// dollar-labeled field.
//
// Loaded as a classic script in the webview (defines `FaktorMoneyInput`)
// and importable from Node for tests via `module.exports`. No floats: the
// whole computation is BigInt from the matched digit strings.
(function (root) {
  'use strict';

  /** The exact-integer bound the per-run composer fields mirror (2^53-1). */
  var MAX_MICRO = 9007199254740991n;

  var DOLLAR = /^\$?([0-9]+)(?:\.([0-9]{1,6}))?$/;
  var RAW_MICRO = /^[0-9]+$/;

  function clean(value) {
    return String(value === null || value === undefined ? '' : value).trim();
  }

  /**
   * The typed refusal for anything that is not one exact dollar amount.
   * Every reason names the exact problem (the draft is always kept).
   */
  function dollarRefusal(text) {
    if (text.length === 0) {
      return 'the cost budget is empty; type a dollar amount such as $5.00';
    }
    if (text.indexOf('-') !== -1) {
      return 'a cost budget cannot be negative';
    }
    if (text.indexOf('+') === 0) {
      return 'a cost budget cannot carry a leading + sign';
    }
    if (text.indexOf(',') !== -1 || text.indexOf(' ') !== -1 || text.indexOf('_') !== -1) {
      return 'grouped separators are ambiguous; write the amount as $12.34';
    }
    if (/[eE]/.test(text) || /[nN][aA][nN]/.test(text) || /inf/i.test(text)) {
      return 'exponents and non-finite values are not exact amounts; write plain digits';
    }
    if (text.indexOf('.') !== -1) {
      var dot = text.indexOf('.');
      var fraction = text.slice(dot + 1);
      if (fraction.length > 6) {
        return 'a dollar amount holds at most six decimal places (micro-USD precision)';
      }
      if (!/^[0-9]+$/.test(fraction)) {
        return 'the decimal part must be digits only; write the amount as $12.34';
      }
    }
    if (!DOLLAR.test(text)) {
      return 'write the cost budget as a dollar amount such as $5.00';
    }
    return 'the cost budget is not an exact dollar amount';
  }

  /**
   * Parse the dollar-labeled field into EXACT micro-USD (decimal string).
   * `5`, `5.0`, `5.00`, `$5`, `$5.00` → `{ micro: "5000000" }`. Anything
   * else → `{ reason }` (never a number, never a rounding).
   */
  function parseDollarMicro(value) {
    var text = clean(value);
    if (text.length === 0) {
      return { reason: 'the cost budget is empty; type a dollar amount such as $5.00' };
    }
    var match = DOLLAR.exec(text);
    if (match === null) {
      return { reason: dollarRefusal(text) };
    }
    var whole = BigInt(match[1]);
    var fraction = match[2] === undefined ? '' : match[2];
    var micro = whole * 1000000n + BigInt(fraction === '' ? '0' : fraction.padEnd(6, '0'));
    if (micro > MAX_MICRO) {
      return {
        reason: 'the cost budget exceeds the exact integer range of the per-run field (max 2^53-1 micro-USD)',
      };
    }
    return { micro: micro.toString() };
  }

  /**
   * Parse the ADVANCED raw micro-USD surface (explicit checkbox): a plain
   * non-negative decimal integer within the same exact range. This is the
   * only path where a bare integer is micro, and it is never implied by the
   * dollar-labeled field.
   */
  function parseRawMicro(value) {
    var text = clean(value);
    if (text.length === 0) {
      return { reason: 'the cost budget is empty; type a micro-USD amount such as 5000000' };
    }
    if (!RAW_MICRO.test(text)) {
      if (text.indexOf('-') === 0) {
        return { reason: 'a cost budget cannot be negative' };
      }
      return { reason: 'raw micro-USD is a plain non-negative integer (no decimals, signs or separators)' };
    }
    var micro = BigInt(text);
    if (micro > MAX_MICRO) {
      return {
        reason: 'the cost budget exceeds the exact integer range of the per-run field (max 2^53-1 micro-USD)',
      };
    }
    return { micro: micro.toString() };
  }

  /**
   * The one entry point the composer uses: `raw` is true when the explicit
   * advanced "raw micro-USD" checkbox is on. Empty input stays empty (the
   * setting default applies) and is NOT an error.
   */
  function parseBudgetCost(value, raw) {
    var text = clean(value);
    if (text.length === 0) {
      return { empty: true };
    }
    return raw === true ? parseRawMicro(text) : parseDollarMicro(text);
  }

  var api = {
    parseBudgetCost: parseBudgetCost,
    parseDollarMicro: parseDollarMicro,
    parseRawMicro: parseRawMicro,
  };
  if (typeof module !== 'undefined' && module.exports) {
    module.exports = api;
  }
  if (root) {
    root.FaktorMoneyInput = api;
  }
})(typeof globalThis !== 'undefined' ? globalThis : null);
