import test from "node:test";
import assert from "node:assert/strict";
import { computeTotal, cartTotal } from "../src/cart.js";

describe("cart", () => {
  test("totals add up", () => {
    const items = [10, 20];
    assert.equal(computeTotal(items), 30);
    assert.equal(cartTotal(items), 30);
  });
});
