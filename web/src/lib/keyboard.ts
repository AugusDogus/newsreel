import { isTv } from "./tv";

/** What says whether a person can press a key: a TV is driven by its remote, and a touchscreen
 * offers no keyboard until a field asks for one. A mouse or trackpad that hovers stands for a computer. */
export interface Keyed {
  tv: boolean;
  hoverWithFinePointer: boolean;
}

function here(): Keyed {
  return { tv: isTv(), hoverWithFinePointer: matchMedia("(any-hover: hover) and (any-pointer: fine)").matches };
}

/** `label` with the key that does the same in brackets, where there is a keyboard to press it on. */
export function withKey(label: string, key: string, keyed: Keyed = here()): string {
  return !keyed.tv && keyed.hoverWithFinePointer ? `${label} (${key})` : label;
}
