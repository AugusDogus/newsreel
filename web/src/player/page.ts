import { useEffect } from "react";
import { leaveOnBack } from "./keys";

/** What every player page does: names the browser tab after what plays, and goes back on Esc until the video plays. */
export function usePlayerPage(name: string | null, ready: boolean, back: () => void): void {
  useEffect(() => {
    document.title = name ? `${name} · newsreel` : "newsreel";
    return () => void (document.title = "newsreel");
  }, [name]);
  useEffect(() => {
    if (ready) return;
    const pressed = leaveOnBack(back);
    addEventListener("keydown", pressed);
    return () => removeEventListener("keydown", pressed);
  }, [ready, back]);
}
