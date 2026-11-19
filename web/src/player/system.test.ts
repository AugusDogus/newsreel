import { expect, test } from "bun:test";
import { sessionActions } from "./system";

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
