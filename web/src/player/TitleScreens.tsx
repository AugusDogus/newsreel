import type { ReactNode } from "react";
import type { ApiError, Job, Release } from "../api";
import { bytes, count, sentence } from "../lib/format";
import { readable } from "../lib/release";
import { Link } from "../router";
import { CODECS, RESOLUTIONS } from "../search/ResultRow";
import { Picture } from "../titles/Picture";
import { CHECKS, skipText } from "../titles/words";
import { isTv } from "../lib/tv";
import { Button, buttonClass } from "../ui/Button";
import { READING, type Step, Steps, waitingOn } from "./Chrome";
import type { Attempt, Passed } from "./titleSession";
import { SHORT_ID } from "../wire";

/** The title's picture behind the screens before playback, dark enough for text at any point of it. */
function Backdrop({ src }: { src: string | null }) {
  return (
    <div aria-hidden="true" className="absolute inset-0 overflow-hidden bg-radial-[at_15%_95%] from-raised to-black to-70%">
      <div className="absolute inset-0 opacity-40">
        <Picture src={src} alt="" className="size-full object-cover" />
      </div>
      <div className="absolute inset-0 bg-linear-to-t from-black via-black/55 to-black/25" />
      <div className="absolute inset-0 bg-linear-to-r from-black/80 via-black/30 to-transparent" />
    </div>
  );
}

/** What plays: the title, and under it an episode's number and name or a film's year. */
export interface Naming {
  title: string | null;
  detail: string | null;
}

function Name({ naming }: { naming: Naming }) {
  if (!naming.title) return null;
  return (
    <div className="flex flex-col gap-2">
      <h1 className="text-3xl font-semibold tracking-tight text-balance text-ink sm:text-4xl">{naming.title}</h1>
      {naming.detail && <p className="text-base text-soft">{naming.detail}</p>}
    </div>
  );
}

/** A release in a few words: its quality, size, and grabs. */
function about(release: Release): string {
  const { quality } = release;
  return [
    quality.resolution && RESOLUTIONS[quality.resolution],
    quality.codec && CODECS[quality.codec],
    quality.hdr && "HDR",
    release.size !== null && bytes(release.size),
    release.grabs !== null && count(release.grabs, "grab"),
    release.sources[0],
  ]
    .filter(Boolean)
    .join(" · ");
}

function PassedList({ title, passed }: { title: string; passed: Passed[] }) {
  if (passed.length === 0) return null;
  return (
    <section aria-label={title} className="flex flex-col gap-2">
      <h2 className="text-sm font-medium text-pencil">{title}</h2>
      <ul role="list" className="flex flex-col gap-2">
        {passed.map((source) => {
          const name = source.name ? readable(source.name) : null;
          return (
            <li key={source.release} className="flex min-w-0 flex-col text-sm">
              <p className="truncate text-soft" title={source.name ?? undefined}>
                {name ? (
                  <>
                    {name.title} <span className="text-pencil">{name.tags}</span>
                  </>
                ) : (
                  `Source ${source.release.slice(0, SHORT_ID)}`
                )}
              </p>
              <p className="text-pretty text-pencil">{skipText(source.reason)}</p>
            </li>
          );
        })}
      </ul>
    </section>
  );
}

/**
 * Which source of a play this is: "source 2 of the best 6". A search finds more sources than a play
 * tries, since it tries at most six, best first, and only those this screen plays.
 */
export function which({ rank, of }: { rank: number; of: number }): string {
  return of === 1 ? "the best source" : `source ${rank} of the best ${of}`;
}

/**
 * The steps of a title's play, at step `step`. A source the person picked from the title's list is
 * the only one tried, so its steps name it as theirs, and count no others.
 */
export function steps(attempt: Attempt, picked: boolean, step: number): Step[] {
  const { found, trying } = attempt;
  const source = picked ? "the source you picked" : trying && which(trying);
  return [
    picked
      ? { title: found === null ? "Finding the source you picked" : "Found the source you picked", detail: "Looking it up among this title's releases." }
      : {
          title: found === null ? "Finding sources" : found === 1 ? "Found 1 source" : `Found ${found} sources`,
          detail: "Searching the indexers for this title's releases.",
        },
    {
      title: !source ? "Trying sources" : step >= 2 ? `Playing ${source}` : `Trying ${source}`,
      detail: trying && (
        <>
          <span className="text-soft">{about(trying.release)}</span>
          {trying.check && <span className="block">{CHECKS[trying.check]}</span>}
        </>
      ),
    },
    ...READING,
  ];
}

interface StartingProps {
  attempt: Attempt;
  naming: Naming;
  backdrop: string | null;
  /** The step in progress: 0 finding sources, 1 trying them, 2 reading the video, 3 buffering. */
  step: 0 | 1 | 2 | 3;
  /** The person picked the source from the title's list. */
  picked: boolean;
  /** The download that plays, once there is one, for the repair the running step may wait on. */
  job: Job | null;
  /** How the download that plays is doing, under the steps. */
  readout?: ReactNode;
}

/** The screen before the first frame of a title: what plays, and what the engine is doing toward it. */
export function TitleStarting({ attempt, naming, backdrop, step, picked, job, readout }: StartingProps) {
  return (
    <div className="absolute inset-0">
      <Backdrop src={backdrop} />
      <div className="absolute inset-0 flex items-end overflow-y-auto pt-20 pr-[max(--spacing(6),7vw,env(safe-area-inset-right))] pb-[max(--spacing(8),11vh)] pl-[max(--spacing(6),7vw,env(safe-area-inset-left))]">
        <div className="flex w-full max-w-xl animate-appear flex-col gap-8 motion-reduce:animate-none">
          <Name naming={naming} />
          <Steps steps={waitingOn(steps(attempt, picked, step), step, job)} step={step} />
          {readout && <div className="min-h-5 text-sm text-soft">{readout}</div>}
          <PassedList title="Passed over" passed={attempt.passed} />
        </div>
      </div>
    </div>
  );
}

interface FailedProps {
  naming: Naming;
  backdrop: string | null;
  kind: "film" | "episode";
  error: ApiError;
  tried: Passed[];
  /** The page that lists this title's sources. */
  sources: string;
  /** Plays without the pinned source, when one was pinned. */
  unpinned: string | null;
  onRetry: () => void;
  onBack: () => void;
}

/** No source of a title played. Says why in the engine's words, and what each source did. */
export function TitleFailed({ naming, backdrop, kind, error, tried, sources, unpinned, onRetry, onBack }: FailedProps) {
  return (
    <div role="alert" className="absolute inset-0">
      <Backdrop src={backdrop} />
      <div className="absolute inset-0 flex items-end overflow-y-auto pt-20 pr-[max(--spacing(6),7vw,env(safe-area-inset-right))] pb-[max(--spacing(8),11vh)] pl-[max(--spacing(6),7vw,env(safe-area-inset-left))]">
        <div className="flex w-full max-w-xl animate-appear flex-col gap-8 motion-reduce:animate-none">
          <Name naming={naming} />
          <div className="flex flex-col gap-2">
            <h2 className="text-lg font-medium text-balance text-ink">This {kind} cannot play right now</h2>
            <p className="text-base text-pretty text-soft sm:text-sm">{sentence(error.message)}</p>
            {error.hint && <p className="text-base text-pretty text-pencil sm:text-sm">{sentence(error.hint)}</p>}
          </div>
          <PassedList title="Tried" passed={tried} />
          <div className="flex flex-wrap gap-2">
            <Button variant="primary" autoFocus={isTv()} onClick={onRetry}>
              Try again
            </Button>
            {unpinned ? (
              <Link href={unpinned} replace className={buttonClass({})}>
                Let newsreel choose
              </Link>
            ) : (
              <Link href={sources} className={buttonClass({})}>
                Choose a source
              </Link>
            )}
            <Button variant="quiet" onClick={onBack}>
              Back
            </Button>
          </div>
        </div>
      </div>
    </div>
  );
}
