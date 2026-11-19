import { expect, test } from "bun:test";
import vectors from "../../../crates/core/tests/fixtures/format.json";
import { age, bytes, clock, percent, repairing } from "./format";
import { isFinished } from "./storage";

test("sizes, ages, percents and clocks read as the shared vectors say", () => {
  for (const { bytes: count, text } of vectors.size) expect([count, bytes(count)]).toEqual([count, text]);
  for (const { seconds, text } of vectors.age) expect([seconds, age(0, seconds)]).toEqual([seconds, text]);
  for (const { done, size, web } of vectors.percent) expect([done, size, percent(done / size)]).toEqual([done, size, web]);
  for (const { seconds, text } of vectors.clock) expect([seconds, clock(seconds)]).toEqual([seconds, text]);
});

test("a video is finished where the shared vectors say", () => {
  for (const { position, duration, finished } of vectors.finished) {
    expect([position, duration, isFinished(position, duration)]).toEqual([position, duration, finished]);
  }
});

test("a repair says what the player waits for, and how long it has left", () => {
  const repair = { articles: 1, stage: "waiting" as const, blocking: true, eta: 95 };
  expect(repairing(repair)).toBe("Repairing a damaged part of this video · about 1m 35s left");
  expect(repairing({ ...repair, eta: null })).toBe("Repairing a damaged part of this video once the rest of it arrives.");
  expect(repairing({ ...repair, articles: 2, stage: "preparing", eta: null })).toBe("Repairing damaged parts of this video. Fetching its recovery files.");
  expect(repairing({ ...repair, stage: "preparing" })).toBe("Repairing a damaged part of this video · about 1m 35s left");
  expect(repairing({ ...repair, stage: "rebuilding" })).toBe("Repairing a damaged part of this video · almost done");
});
