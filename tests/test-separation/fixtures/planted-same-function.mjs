#!/usr/bin/env node
// TEST-SEPARATION: oracle=external-fixture isolation=separate-process
//
// Planted class (d): the same function computes the expected and actual
// values, so the assertion can never detect a wrong implementation.
import assert from 'node:assert';
import { computeRouteCost } from './sut.mjs';

const request = { distance: 42, weight: 7 };
assert.equal(computeRouteCost(request), computeRouteCost(request));
const expected = computeRouteCost(request);
assert.equal(computeRouteCost(request), expected);
