import { expect, test } from "bun:test";
import type { Job, Progress } from "../api";
import { READING, waitingOn } from "./Chrome";

const progress: Progress = { size: 1, verified: 0, down_rate: 5_000_000, connections: 30, eta: 120 };
const downloading = (repair: Progress["repair"]): Pick<Job, "state" | "progress"> => ({ state: "downloading", progress: { ...progress, repair } });

/** A resume just before Charade's damaged article waited 80 s on "Buffering the first seconds." while the engine said `repair: blocking, eta 19`. */
test("the start screen's running step says the repair it waits on and its time left, in place of its own words", () => {
  const repair = { articles: 1, stage: "waiting" as const, blocking: true, eta: 19 };
  const shown = waitingOn(READING, 1, downloading(repair));
  expect(shown.map((step) => step.detail)).toEqual([READING[0].detail, "Repairing a damaged part of this video · about 19s left"]);
  expect(shown.map((step) => step.title)).toEqual(READING.map((step) => step.title));
});

test("a repair no read waits on, or a download that is not running, leaves the steps as they are", () => {
  const repair = { articles: 1, stage: "waiting" as const, blocking: false, eta: 19 };
  expect(waitingOn(READING, 1, downloading(repair))).toEqual(READING);
  expect(waitingOn(READING, 1, downloading(null))).toEqual(READING);
  expect(waitingOn(READING, 1, { state: "paused", progress: { ...progress, repair: { ...repair, blocking: true } } })).toEqual(READING);
  expect(waitingOn(READING, 1, null)).toEqual(READING);
});
