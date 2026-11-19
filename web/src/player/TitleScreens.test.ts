import { expect, test } from "bun:test";
import { steps, which } from "./TitleScreens";

/** "Found 10 sources" then "source 1 of 6" read as a contradiction: six is how many of the ten this play tries. */
test("a play names the source it tries among the best it tries, not among all it found", () => {
  expect(which({ rank: 1, of: 6 })).toBe("source 1 of the best 6");
  expect(which({ rank: 1, of: 1 })).toBe("the best source");
});

/** A play of a release picked from the title's list said "Found 9 sources" and "Playing the best source". */
test("a play of the source the person picked names it as theirs and counts no others", () => {
  const attempt = { video: "tt0056923", title: null, episode: null, found: 9, trying: null, passed: [], named: {} };
  const titles = (picked: boolean, step: number, at = attempt) => steps(at, picked, step).map((s) => s.title).slice(0, 2);
  expect(titles(true, 0, { ...attempt, found: null })).toEqual(["Finding the source you picked", "Trying the source you picked"]);
  expect(titles(true, 1)).toEqual(["Found the source you picked", "Trying the source you picked"]);
  expect(titles(true, 3)).toEqual(["Found the source you picked", "Playing the source you picked"]);
  expect(titles(false, 1)).toEqual(["Found 9 sources", "Trying sources"]);
});
