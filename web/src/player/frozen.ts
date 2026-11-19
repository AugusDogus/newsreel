/**
 * Whether a video that plays shows anything. A browser can take a file, play its sound, and show
 * no picture, with no error: Safari froze its clock on Charade (1963), whose H.264 had 3-byte NAL
 * lengths, and Chrome on Android never decoded a frame of it. Then the video's clock and its
 * decoded frames stop moving while it says it plays.
 */

/** The video plays, its clock and its frames standing still, this long: it shows nothing. */
export const FROZEN_MS = 6000;
/** Under this many seconds buffered ahead, the video waits for data, which the stall watch covers. */
const FED_S = 1;

/** What the player sees, about once a second. */
export interface Look {
  /** Milliseconds, from any steady clock. */
  now: number;
  /** The playhead, in seconds. */
  time: number;
  /** Frames decoded so far, or null where the browser does not count them. */
  frames: number | null;
  /** Not paused, ended, seeking, or in a hidden page, where browsers stop decoding the picture. */
  playing: boolean;
  /** Seconds buffered ahead of the playhead. */
  ahead: number;
}

/** When the video last moved, and where it was then. Null while it is not meant to move. */
export type Moved = { since: number; time: number; frames: number | null } | null;

/** Where the video last moved after `look`, and whether it has stood still too long. */
export function watch(last: Moved, look: Look): { moved: Moved; frozen: boolean } {
  if (!look.playing || look.ahead < FED_S) return { moved: null, frozen: false };
  const here = { since: look.now, time: look.time, frames: look.frames };
  if (!last || look.time !== last.time || look.frames !== last.frames) return { moved: here, frozen: false };
  return { moved: last, frozen: look.now - last.since >= FROZEN_MS };
}

/** Frames the browser has decoded, or null where it does not say. */
export function decodedFrames(element: HTMLVideoElement): number | null {
  const counted = element as HTMLVideoElement & { webkitDecodedFrameCount?: number };
  if (typeof element.getVideoPlaybackQuality === "function") return element.getVideoPlaybackQuality().totalVideoFrames;
  return counted.webkitDecodedFrameCount ?? null;
}
