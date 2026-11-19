import { useEffect, useState } from "react";
import type { PlayerActions } from "./actions";
import type { VideoStore } from "./video";
import { useVideo } from "./video";

interface NowPlaying {
  title: string;
  /** The release the video belongs to. */
  album: string;
  actions: PlayerActions;
  video: VideoStore;
  /** Plays the next or previous video of the release, where there is one. */
  onNext: (() => void) | null;
  onPrevious: (() => void) | null;
  /** Leaves the player, which keeps the place, as Back does. */
  onStop: () => void;
}

/** What each of the system's now-playing controls does. */
export function sessionActions({
  actions,
  pause,
  onNext,
  onPrevious,
  onStop,
}: {
  actions: Pick<PlayerActions, "play" | "seekBy" | "seekTo">;
  pause: () => void;
  onNext: (() => void) | null;
  onPrevious: (() => void) | null;
  onStop: () => void;
}): [MediaSessionAction, MediaSessionActionHandler | null][] {
  return [
    ["play", () => actions.play()],
    ["pause", pause],
    ["seekbackward", (details) => actions.seekBy(-(details.seekOffset ?? 10))],
    ["seekforward", (details) => actions.seekBy(details.seekOffset ?? 10)],
    ["seekto", (details) => details.seekTime !== undefined && actions.seekTo(details.seekTime)],
    ["previoustrack", onPrevious],
    ["nexttrack", onNext],
    // Chromium on Android sends a TV remote's Stop here, not to the page, and on its own rewinds the video to 0:00.
    ["stop", onStop],
  ];
}

/** A browser that does not know an action throws rather than ignore its handler. */
function setHandler(session: MediaSession, action: MediaSessionAction, handler: MediaSessionActionHandler | null): void {
  try {
    session.setActionHandler(action, handler);
  } catch {
    // Nothing sends an action the browser does not know.
  }
}

/** Tells the system what is playing, and answers its media keys and now-playing controls. */
export function useMediaSession({ title, album, actions, video, onNext, onPrevious, onStop }: NowPlaying): void {
  const paused = useVideo(video, (state) => state.paused);
  useEffect(() => {
    if (!("mediaSession" in navigator)) return;
    const session = navigator.mediaSession;
    session.metadata = new MediaMetadata({ title, album });
    const handlers = sessionActions({ actions, pause: () => video.element.pause(), onNext, onPrevious, onStop });
    for (const [action, handler] of handlers) setHandler(session, action, handler);
    return () => {
      session.metadata = null;
      for (const [action] of handlers) setHandler(session, action, null);
    };
  }, [title, album, actions, video, onNext, onPrevious, onStop]);

  useEffect(() => {
    if ("mediaSession" in navigator) navigator.mediaSession.playbackState = paused ? "paused" : "playing";
  }, [paused]);
}

/** What the player sees as a seek starts. */
export interface SeekStart {
  /** The browser has a Media Session, whose `stop` handler gets a remote's Stop. */
  session: boolean;
  /** Where the seek goes, in seconds. */
  time: number;
  paused: boolean;
  /** A frame has been shown. */
  started: boolean;
  /** The player asked for a seek that has not landed yet. */
  asked: boolean;
}

/**
 * Whether a seek is the browser's own answer to a remote's Stop. Android's WebView (TV Bro and other TV browsers) has
 * no Media Session: it answers Stop itself, pausing the video and seeking it to 0:00, and tells the page nothing else.
 * Play would then start over. A paused seek to exactly 0:00 that the player did not ask for, once the video started, is
 * that, since nothing else in a browser without a Media Session moves a video.
 */
export function stoppedByBrowser({ session, time, paused, started, asked }: SeekStart): boolean {
  return !session && time === 0 && paused && started && !asked;
}

/**
 * Leaves the player, as Back does, when the browser stopped the video itself. The place was saved when it paused, and
 * the rewind to 0:00, being no play, saves nothing.
 */
export function useBrowserStop(video: VideoStore, actions: Pick<PlayerActions, "asked">, onStop: () => void): void {
  useEffect(() => {
    const { element } = video;
    const seeking = () => {
      const seek = {
        session: "mediaSession" in navigator,
        time: element.currentTime,
        paused: element.paused,
        started: video.snapshot().started,
        asked: actions.asked,
      };
      if (stoppedByBrowser(seek)) onStop();
    };
    element.addEventListener("seeking", seeking);
    return () => element.removeEventListener("seeking", seeking);
  }, [video, actions, onStop]);
}

/** Keeps the screen awake while the video plays. The lock is given back on pause and when the tab hides. */
export function useWakeLock(video: VideoStore): void {
  const playing = useVideo(video, (state) => !state.paused && !state.ended);
  useEffect(() => {
    if (!playing || !("wakeLock" in navigator)) return;
    let lock: WakeLockSentinel | null = null;
    let released = false;
    const take = () => {
      if (document.visibilityState !== "visible") return;
      navigator.wakeLock
        .request("screen")
        .then((taken) => {
          lock = taken;
          if (released) void taken.release();
        })
        .catch(() => {});
    };
    take();
    document.addEventListener("visibilitychange", take);
    return () => {
      released = true;
      document.removeEventListener("visibilitychange", take);
      void lock?.release();
    };
  }, [playing]);
}

/** Safari's own names for AirPlay. */
interface AirPlayElement extends HTMLVideoElement {
  webkitShowPlaybackTargetPicker?: () => void;
}

/**
 * Opens Safari's AirPlay picker, or null where there is no AirPlay screen to send to. Only a
 * file the browser plays itself can go to one, which `enabled` says.
 */
export function useAirPlay(element: HTMLVideoElement, enabled: boolean): (() => void) | null {
  const [available, setAvailable] = useState(false);
  useEffect(() => {
    const changed = (event: Event) => setAvailable((event as Event & { availability?: string }).availability === "available");
    element.addEventListener("webkitplaybacktargetavailabilitychanged", changed);
    return () => element.removeEventListener("webkitplaybacktargetavailabilitychanged", changed);
  }, [element]);
  const picker = (element as AirPlayElement).webkitShowPlaybackTargetPicker;
  return enabled && available && picker ? () => picker.call(element) : null;
}
