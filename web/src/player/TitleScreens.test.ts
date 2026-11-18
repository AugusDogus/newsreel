import { expect, test } from "bun:test";
import { which } from "./TitleScreens";

/** "Found 10 sources" then "source 1 of 6" read as a contradiction: six is how many of the ten this play tries. */
test("a play names the source it tries among the best it tries, not among all it found", () => {
  expect(which({ rank: 1, of: 6 })).toBe("source 1 of the best 6");
  expect(which({ rank: 1, of: 1 })).toBe("the best source");
});
