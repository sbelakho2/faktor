import test from "node:test";
import assert from "node:assert/strict";
import { computeTotal, cartTotal } from "../src/cart.js";

describe("cart", () => {
  let items: number[] = [];
  test("totals add up", () => {
    assert.equal(computeTotal(items), 30);
    assert.equal(cartTotal(items), 30);
  });
});
