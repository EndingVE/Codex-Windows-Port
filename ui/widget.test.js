// node --test ui/widget.test.js — pure helpers of the floating widget.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const { widgetDuration } = require("./widget.js");

const S = 1000, M = 60 * S, H = 60 * M, D = 24 * H;

test("drops zero units instead of printing them", () => {
  assert.equal(widgetDuration(3 * D), "3d");
  assert.equal(widgetDuration(3 * D + 20 * M), "3d"); // minutes never shown with days
  assert.equal(widgetDuration(2 * H), "2h");
  assert.equal(widgetDuration(5 * M), "5m");
});

test("keeps the second unit when it is non-zero", () => {
  assert.equal(widgetDuration(5 * D + 10 * H), "5d 10h");
  assert.equal(widgetDuration(2 * H + 10 * M), "2h 10m");
});

test("sub-minute and invalid input", () => {
  assert.equal(widgetDuration(42 * S), "42s");
  assert.equal(widgetDuration(0), "0s");
  assert.equal(widgetDuration(-5 * S), "0s");
  assert.equal(widgetDuration(NaN), "0s");
});
