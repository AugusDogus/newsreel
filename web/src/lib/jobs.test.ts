import { expect, test } from "bun:test";
import type { Job } from "../api";
import { vanished } from "./jobs";

const ID = "c1788396bbc3adce38d1bb09d0020e3f7182a811";

test("a download the engine no longer has is gone once the engine answers, and unknown while it does not", () => {
  const job: Pick<Job, "id"> = { id: ID };
  const live = (jobs: Pick<Job, "id">[]) => ({ link: "live" as const, jobs });
  expect(vanished(live([job]), ID)).toBe(false);
  expect(vanished(live([job]), ID.slice(0, 6))).toBe(false);
  // Removed with `newsreel rm`, then the engine restarted: its snapshot leaves the play's download out.
  expect(vanished(live([]), ID)).toBe(true);
  expect(vanished({ link: "lost", jobs: [] }, ID)).toBe(false);
  expect(vanished({ link: "connecting", jobs: [] }, ID)).toBe(false);
});
