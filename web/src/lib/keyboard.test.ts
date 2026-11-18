import { expect, test } from "bun:test";
import { withKey } from "./keyboard";

test("a button names its key on a computer, but not on a TV or a touchscreen, which have no keyboard to press it on", () => {
  expect(withKey("Back", "Esc", { tv: false, hoverWithFinePointer: true })).toBe("Back (Esc)");
  expect(withKey("Back", "Esc", { tv: true, hoverWithFinePointer: true })).toBe("Back");
  expect(withKey("Play", "Space", { tv: false, hoverWithFinePointer: false })).toBe("Play");
});
