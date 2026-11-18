import { expect, test } from "bun:test";
import { looksLikeTv, type Signals } from "./tv";

/** A 1920 by 1080 screen with a mouse and no touch, but for what `seen` says. */
function tv(seen: Partial<Signals>): boolean {
  return looksLikeTv({ userAgent: MAC, width: 1920, height: 1080, touch: false, pointerless: false, coarseWithoutHover: false, ...seen });
}

const LG = "Mozilla/5.0 (Web0S; Linux/SmartTV) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/108.0.0.0 Safari/537.36";
const SAMSUNG = "Mozilla/5.0 (SMART-TV; LINUX; Tizen 8.0) AppleWebKit/537.36 (KHTML, like Gecko) 108.0.5359.1/8.0 TV Safari/537.36";
const MAC = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";
const IPHONE = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.5 Mobile/15E148 Safari/604.1";

test("the LG and Samsung TV browsers on a 1920 screen are TVs", () => {
  expect(tv({ userAgent: LG })).toBe(true);
  expect(tv({ userAgent: SAMSUNG })).toBe(true);
});

test("a TV browser that reports a 960 pixel wide screen is a TV", () => {
  expect(tv({ userAgent: LG, width: 960, height: 540 })).toBe(true);
});

test("a big screen with no pointer is a TV whatever its browser calls itself", () => {
  expect(tv({ userAgent: MAC, pointerless: true })).toBe(true);
});

test("a desktop browser with a mouse is not a TV, however wide its screen", () => {
  expect(tv({ userAgent: MAC, width: 2560, height: 1440 })).toBe(false);
});

test("a TV name on a touchscreen or a small screen is not enough", () => {
  expect(tv({ userAgent: LG, touch: true })).toBe(false);
  expect(tv({ userAgent: LG, width: 800, height: 450, pointerless: true })).toBe(false);
});

test("a phone is not a TV", () => {
  expect(tv({ userAgent: IPHONE, width: 390, height: 844, touch: true, coarseWithoutHover: true })).toBe(false);
});

/** TV Bro on Android TV, as the emulator's reports: Chrome's reduced user agent, five touch points. */
const TV_BRO = "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/157.0.0.0 Safari/537.36";
const TABLET = "Mozilla/5.0 (Linux; Android 14; K) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/157.0.0.0 Safari/537.36";

test("a browser on a 16:9 screen driven by a pointer that is coarse and cannot hover is a TV, though it claims touch", () => {
  expect(tv({ userAgent: TV_BRO, width: 960, height: 540, touch: true, coarseWithoutHover: true })).toBe(true);
});

test("a tablet's 16:10 or 4:3 screen is not a TV, however it is driven", () => {
  expect(tv({ userAgent: TABLET, width: 1280, height: 800, touch: true, coarseWithoutHover: true })).toBe(false);
  expect(tv({ userAgent: MAC, width: 1024, height: 768, touch: true, coarseWithoutHover: true })).toBe(false);
});

test("Android TV, Google TV, Fire TV, BRAVIA, and SHIELD names are TVs, though their browsers claim touch", () => {
  const named = [
    "Mozilla/5.0 (Linux; Android 12; BRAVIA 4K VH2 Build/STT1.211025.001) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Mobile Safari/537.36",
    "Mozilla/5.0 (Linux; Android 9; AFTMM Build/PS7633) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36",
    "Mozilla/5.0 (Linux; Android 11; SHIELD Android TV Build/RQ1A.210105.003) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36",
    "Mozilla/5.0 (Linux; Android 12; Chromecast) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36 GoogleTV",
  ];
  for (const userAgent of named) expect(tv({ userAgent, touch: true })).toBe(true);
});
