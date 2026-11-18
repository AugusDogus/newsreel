import { expect, test } from "bun:test";
import { command, leaveOnBack } from "./keys";

/** A key press as the starting player hears it, which records whether it was cancelled. */
function press(key: string, { keyCode = 0, metaKey = false } = {}) {
  const event = { key, keyCode, metaKey, code: "", ctrlKey: false, altKey: false, shiftKey: false, cancelled: false, preventDefault() {} };
  event.preventDefault = () => void (event.cancelled = true);
  return event;
}

function leave(event: ReturnType<typeof press>): string[] {
  const backs: string[] = [];
  leaveOnBack(() => backs.push("back"))(event as unknown as KeyboardEvent);
  return backs;
}

test("Back on an LG remote leaves the starting player once and keeps the TV browser from going back as well", () => {
  const event = press("Unidentified", { keyCode: 461 });
  expect(leave(event)).toEqual(["back"]);
  expect(event.cancelled).toBe(true);
});

test("Escape leaves the starting player the same way", () => {
  const event = press("Escape");
  expect(leave(event)).toEqual(["back"]);
  expect(event.cancelled).toBe(true);
});

test("Escape with Command held is left to the browser", () => {
  const event = press("Escape", { metaKey: true });
  expect(leave(event)).toEqual([]);
  expect(event.cancelled).toBe(false);
});

/** A key press with focus on `target`, as the playing player hears it. */
function pressed(key: string, keyCode = 0, target = "div") {
  const element = { closest: (selector: string) => (selector.split(", ").some((s) => s.startsWith(target)) ? {} : null) };
  return { ...press(key, { keyCode }), target: element } as unknown as KeyboardEvent;
}

test("a remote's media keys play, pause, wind, and stop, by name or by a TV's key code, wherever focus is", () => {
  expect(command(pressed("MediaPlayPause"))).toEqual({ kind: "toggle" });
  expect(command(pressed("MediaPlay"))).toEqual({ kind: "play" });
  expect(command(pressed("MediaPause"))).toEqual({ kind: "pause" });
  expect(command(pressed("MediaFastForward"))).toEqual({ kind: "seekBy", seconds: 10 });
  expect(command(pressed("MediaRewind"))).toEqual({ kind: "seekBy", seconds: -10 });
  expect(command(pressed("MediaStop"))).toEqual({ kind: "leave" });
  // LG and Samsung send these key codes with no key name.
  expect(command(pressed("Unidentified", 415))).toEqual({ kind: "play" });
  expect(command(pressed("Unidentified", 19))).toEqual({ kind: "pause" });
  expect(command(pressed("Unidentified", 417))).toEqual({ kind: "seekBy", seconds: 10 });
  expect(command(pressed("Unidentified", 412))).toEqual({ kind: "seekBy", seconds: -10 });
  expect(command(pressed("Unidentified", 413))).toEqual({ kind: "leave" });
  expect(command(pressed("Unidentified", 10252))).toEqual({ kind: "toggle" });
  // A focused button does not take it.
  expect(command(pressed("MediaPlayPause", 0, "button"))).toEqual({ kind: "toggle" });
});
