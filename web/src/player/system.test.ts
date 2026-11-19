import { expect, test } from "bun:test";
import { type SeekStart, sessionActions, stoppedByBrowser } from "./system";

test("Stop from a remote leaves the player, which keeps the place, instead of the browser's own stop", () => {
  const did: string[] = [];
  const handlers = new Map(
    sessionActions({
      actions: { play: () => did.push("play"), seekBy: () => did.push("seekBy"), seekTo: () => did.push("seekTo") },
      pause: () => did.push("pause"),
      onNext: null,
      onPrevious: null,
      onStop: () => did.push("leave"),
    }),
  );
  // Chromium on Android sends a remote's Stop to the Media Session; without a handler it rewinds to 0:00.
  const stop = handlers.get("stop");
  expect(stop).toBeFunction();
  stop?.({ action: "stop" });
  expect(did).toEqual(["leave"]);
});

/** A seek starting at 0:00 in Android's WebView after a remote's Stop, unless `seen` says otherwise. */
function seek(seen: Partial<SeekStart> = {}): SeekStart {
  return { session: false, time: 0, paused: true, started: true, asked: false, ...seen };
}

test("Stop from a remote in a WebView, which has no Media Session and rewinds to 0:00 itself, leaves the player", () => {
  // TV Bro on Android TV: Stop fires `pause` with the time already 0, then `seeking` to 0, and nothing else; while
  // paused, only the `seeking`. Both reach `seeking` paused, at 0, with no seek the player asked for.
  expect(stoppedByBrowser(seek())).toBe(true);
});

test("a seek to 0:00 the player asked for, before the video started, or where the Media Session takes Stop, is no stop", () => {
  // Home, Replay, the seek bar's start, or a chapter at 0:00.
  expect(stoppedByBrowser(seek({ asked: true }))).toBe(false);
  // Chrome and Safari send Stop to the session's handler, and do not rewind.
  expect(stoppedByBrowser(seek({ session: true }))).toBe(false);
  // WebKit reports 0:00 for where it was told to begin before the metadata comes.
  expect(stoppedByBrowser(seek({ started: false }))).toBe(false);
  expect(stoppedByBrowser(seek({ time: 0.01 }))).toBe(false);
  expect(stoppedByBrowser(seek({ paused: false }))).toBe(false);
});
