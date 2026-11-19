import { expect, test } from "bun:test";
import { FROZEN_MS, type Look, type Moved, watch } from "./frozen";

/** The video plays at 600 s with 5 s buffered and 14,400 frames decoded, unless `seen` says otherwise. */
function look(now: number, seen: Partial<Look> = {}): Look {
  return { now, time: 600, frames: 14_400, playing: true, waiting: false, repairing: false, ahead: 5, ...seen };
}

function run(looks: Look[]): boolean[] {
  let moved: Moved = null;
  return looks.map((next) => {
    const seen = watch(moved, next);
    moved = seen.moved;
    return seen.frozen;
  });
}

test("a video that says it plays while its clock and frames stand still is frozen after a few seconds", () => {
  expect(run([look(0), look(1000), look(FROZEN_MS - 1), look(FROZEN_MS)])).toEqual([false, false, false, true]);
  // Chrome on Android with Charade: nothing decoded, the clock at the start, data buffered.
  expect(run([look(0, { frames: 0, time: 0 }), look(FROZEN_MS, { frames: 0, time: 0 })])).toEqual([false, true]);
});

test("a clock or frames that move, a pause, a hidden page, or a wait for data is not frozen", () => {
  const moving = [look(0), look(3000, { time: 603 }), look(6000, { time: 606 }), look(9000, { time: 609 })];
  expect(run(moving)).toEqual([false, false, false, false]);
  // Frames that move are a picture, whatever the clock says.
  expect(run([look(0), look(4000, { frames: 14_500 }), look(8000, { frames: 14_600 })])).toEqual([false, false, false]);
  expect(run([look(0), look(4000, { playing: false }), look(8000), look(8000 + FROZEN_MS - 1)])).toEqual([false, false, false, false]);
  expect(run([look(0), look(FROZEN_MS, { ahead: 0.3 })])).toEqual([false, false]);
  // A browser that does not count frames goes by the clock.
  expect(run([look(0, { frames: null }), look(FROZEN_MS, { frames: null })])).toEqual([false, true]);
});

test("a video that waits for data, with seconds buffered ahead, is not frozen", () => {
  // Chrome fed a quarter as fast as it plays (two plays sharing the link): it waits, readyState 2, with one 2 s fragment
  // buffered, until the next arrives 8 s later. Its clock and frames stand still the while.
  const starved = (now: number) => look(now, { time: 7.9, frames: 200, ahead: 2.12, waiting: true });
  expect(run([starved(0), starved(4000), starved(FROZEN_MS), starved(8000)])).toEqual([false, false, false, false]);
});

test("a video whose download waits on a repair is not frozen", () => {
  const repairing = (now: number) => look(now, { repairing: true });
  expect(run([repairing(0), repairing(FROZEN_MS), repairing(2 * FROZEN_MS)])).toEqual([false, false, false]);
});
