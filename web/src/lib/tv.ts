/** What a browser says about its screen and how it is driven. */
export interface Signals {
  userAgent: string;
  /** The screen's width in CSS pixels. TV browsers report 960, 1280, or 1920. */
  width: number;
  /** The screen's height in CSS pixels: 540, 720, or 1080 on a TV. */
  height: number;
  touch: boolean;
  /** No pointing device at all, as a browser driven only by a remote's arrow keys reports. */
  pointerless: boolean;
  /** The main pointer is coarse and cannot hover, as on a touchscreen, or a remote that Android TV calls one. */
  coarseWithoutHover: boolean;
}

/** The names TV platforms put in their user agent: LG webOS, Samsung Tizen, Hisense VIDAA, and HbbTV sets. */
const TV_PLATFORM = /web0s|webos|tizen|smart-?tv|netcast|hbbtv|vidaa/i;

/**
 * The names Android-based TVs put in their user agent: Android TV and Google TV, Fire TV models
 * (AFTMM, AFTKA, ...), Sony BRAVIA, NVIDIA SHIELD, Chromecast, and Xiaomi's Mi TV. Their browsers
 * report touch points though they have no touchscreen.
 */
const ANDROID_TV = /android ?tv|google ?tv|\baft[a-z0-9]+|bravia|shield|crkey|chromecast|mitv/i;

/** A TV screen is 16:9 and lies flat: 960 by 540, 1280 by 720, or 1920 by 1080 CSS pixels. */
function tvShaped(width: number, height: number): boolean {
  return height > 0 && Math.abs(width / height - 16 / 9) < 0.01;
}

/**
 * A big screen whose browser names a TV, or has no pointer, or is driven by a coarse pointer that
 * cannot hover on a TV-shaped screen. That last is how TV Bro on Android TV looks: its user agent is
 * Chrome's reduced one, "Android 10; K", and it claims five touch points. Tablets, which look the same
 * otherwise, are 16:10 or 4:3.
 */
export function looksLikeTv({ userAgent, width, height, touch, pointerless, coarseWithoutHover }: Signals): boolean {
  if (width < 960) return false;
  if (ANDROID_TV.test(userAgent)) return true;
  if (!touch && (pointerless || TV_PLATFORM.test(userAgent))) return true;
  return coarseWithoutHover && tvShaped(width, height) && !/iphone|ipad|mobile/i.test(userAgent);
}

const CHOICE = "newsreel:tv";

/** `?tv=on` or `?tv=off` in any address settles the question for this browser, and `?tv=auto` asks the signals again. */
function chosen(): boolean | null {
  const asked = new URLSearchParams(location.search).get("tv");
  try {
    if (asked === "on" || asked === "off") localStorage.setItem(CHOICE, asked);
    if (asked === "auto") localStorage.removeItem(CHOICE);
    const kept = localStorage.getItem(CHOICE);
    if (kept === "on" || kept === "off") return kept === "on";
  } catch {
    // A browser that refuses storage still follows the address it was opened with.
  }
  return asked === "on" ? true : asked === "off" ? false : null;
}

let tv: boolean | null = null;

/**
 * This browser is a TV driven by a remote: arrow keys move focus anywhere on the page, Back goes
 * back, and everything is drawn to be read from across a room.
 */
export function isTv(): boolean {
  tv ??=
    chosen() ??
    looksLikeTv({
      userAgent: navigator.userAgent,
      width: screen.width,
      height: screen.height,
      touch: navigator.maxTouchPoints > 0,
      pointerless: matchMedia("(any-pointer: none)").matches,
      coarseWithoutHover: matchMedia("(pointer: coarse) and (hover: none)").matches,
    });
  return tv;
}

/** LG webOS sends Back as key code 461, and Samsung Tizen as 10009. */
const BACK_CODES = new Set([461, 10009]);

/** Back on a remote. Escape on any device, and on a TV also Backspace outside a text field. */
export function isBack(event: KeyboardEvent): boolean {
  if (event.key === "Escape" || event.key === "GoBack" || BACK_CODES.has(event.keyCode)) return true;
  return event.key === "Backspace" && isTv() && !(event.target as Element).closest("input, textarea, [contenteditable]");
}
