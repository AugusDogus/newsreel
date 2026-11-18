# Device test fixes

Fixes for the bugs the live device tests of 2026-11-18 found, one commit each. For each: the bug,
how it was found, the evidence, why the fix is right, and whether upstream (nexxeln/torrential
v0.7.0) has it too. Screenshots are in `~/.cache/nr-devfix/shots/` (before and after) and
`~/.cache/nr-devtest/shots/` (the original test run).

## 1. A release that hangs at 99.9% forever

**Bug.** Playing Charade (1963), release `Charade.1963.BluRay.720p.H264-20-40`, stopped at 99.9%.
The download showed "downloading" with 0 connections for over 12 minutes, the engine logged
nothing, any HTTP range crossing byte 930,050,289 of the video hung until the 5 minute stall
timeout, and the player buffered forever.

**How it was found.** Playing the film on the Android TV emulator during the device tests.

**Evidence.** The NZB was fetched again and resolved with the `usenet` crate. The missing piece is
bytes 930,050,289..930,818,289 of the mp4, article
`<5214f544$0$16594$c3e8da3$e3f2c276@news.astraweb.com>` (part 124 of 137 of `part09.rar`, posted
2013-08-21). Asked on each of the four servers alone:

| Server | STAT | BODY |
|---|---|---|
| server A | 223, has it | decodes to 767,999 bytes, header says 768,000 |
| server B | 223, has it | same |
| server C | 223, has it | same |
| server D | 223, has it | same |

The four bodies are byte for byte identical (SHA-256 `a87a7aee…a9419`). Over a raw NNTP session
(no `usenet` crate involved) the body ends on a data line with no `=yend` line, and an independent
Python yEnc decoder also gets 767,999 bytes. So the article is not gone: it was posted (or
propagated) truncated, and every provider stores the same damaged copy. The NZB lists one
message-ID for it, so no other copy exists. The release carries PAR2 volumes
(`vol-01.par2` to `vol-010.par2`), which could rebuild it.

**Root cause in newsreel.** `usenet::article::fetch` reports a body that does not decode as
`ArticleError::Yenc`, which is not "missing". The engine's worker (`daemon/usenet.rs`) treated
every error other than missing as transient: it took the piece out of `in_flight`, stored the
text in `last_error` (which nothing ever read), slept 2 seconds and asked again, forever. Between
tries no request was in flight, hence 0 connections; the piece was never set aside, so the
download never failed; and nothing logged it. The HTTP range read ignored `want_range` errors
and waited for the 5 minute stall timeout.

**Fix.**
- A fetch now comes to one of three outcomes (`Fetched`): landed, unrecoverable (missing on every
  server, a copy that does not decode, or an article that holds other bytes than the NZB says),
  or retry (a server could not be reached or a connection broke). A server sends the same stored
  copy every time, so asking again for a damaged one cannot help.
- An unrecoverable piece is set aside with its reason and named in the engine log (the first
  three per download, so an expired release does not flood it).
- When a reader waits on such a piece, `want_range` fails with `no_source` naming the bytes and
  the article, and the engine stops the download with that error. The status shows it, a range
  read that reaches the gap ends there, and one that starts on it gets `503` with the reason.
  The web player already moves to the next source at the playhead when its download fails
  (`web/src/player/recovery.ts`), and a source under test that fails is skipped.
- A download nobody reads still fetches everything else and stops at the end, as before, now
  naming the first unrecoverable piece. That keeps the existing behaviour for `newsreel get`
  (tested by `a_download_whose_articles_expired_stops_with_no_source_and_keeps_what_it_verified`)
  and is what PAR2 repair needs: the rest of the data on disk.
- Transient failures are still retried (a server that is down must not fail a download, as
  `a_download_whose_only_server_goes_down_keeps_waiting` checks), but are logged at most once a
  minute per download.
- PAR2 seam: `unrecoverable()` in `daemon/usenet.rs` is the one place a piece is set aside; repair
  hooks in there.

**Live check.** A test engine on its own `NEWSREEL_HOME` played the same NZB against the real
servers. The log named the piece 10 s after the play started; `curl -r 930000000-930100000` got
the 50,289 bytes before the gap and then the connection ended; `curl -r 930050289-` got `503` with
the reason in 0.01 s; `newsreel status` showed `error: [no_source] no server can send bytes
930050289..930818289 of Charade.1963.BluRay.720p.H264.mp4: article <…> arrived damaged …`.

**Tests.** `a_read_across_an_article_no_server_has_fails_at_once_and_the_download_says_why`
(`crates/newsreel/tests/usenet.rs`) failed before (the read hung past the test's 60 s timeout)
and passes now. `a_damaged_or_missing_article_is_set_aside_and_an_unreachable_server_is_asked_again`
covers the classification.

**Upstream.** The Usenet engine is newsreel's own, so the retry loop is not upstream's bug. The
HTTP wait that ignores `want_range` errors is upstream's (`arrived` in `daemon/http.rs` at v0.7.0
is the same loop), but libtorrent never says a piece cannot come, so upstream cannot hit it.

**Left for the `usenet` crate.** `article::fetch` stops at the first copy that does not decode
and the pool returns the first server's body, so a copy damaged on one server but intact on
another is not looked for. Here all four servers had the same bytes, so it would not have helped.

## 2. Play on TV says it resumed, but the TV starts at 0:00

**Bug.** `newsreel tv play` printed `at: 51:37` and the cast bar showed the resume point, but the
renderer played from 0:00. After 30 seconds the bar jumped to the TV's real place without a word.

**How it was found.** Casting to gmrender-resurrect (nixpkgs, gstreamer output to fakesink)
during the device tests. Its log (`~/.cache/nr-devtest/logs/gmr2.log`) shows the order:
`TransportState: PLAYING` at 15:15:10.35, `CurrentTrackDuration: 1:35:52` at 15:15:10.85, then
`RelativeTimePosition` counting up from 0:00:01. No position jump follows, so the resume seek did
nothing.

**Root cause.** The cast seeks on the first poll that says PLAYING. gmrender says PLAYING as soon
as it hands the URI to gstreamer, before the pipeline has prerolled; a seek then is answered with
success and lost. The cast took the answer as done (`Seek::Taken`), showed the intended time for
`RESUME_WITHIN` (30 s), and then gave up silently. A seek the TV refused was retried on the next
three polls (3 s), which a slow TV can also miss.

**Fix** (`daemon/cast/mod.rs`, `Resume::check`).
- Seek only once the TV can take it: it says the track's length, or its position has moved.
- After a seek the TV took, check where it is. Within 10 s before the target (a seek lands on the
  keyframe before it) counts as there. Not there 5 s later sends the seek again, up to 3 seeks.
- While it resumes, the cast still shows where it resumes (upstream's
  `a_cast_that_resumes_shows_where_it_resumes_while_the_tv_seeks_there`, for an LG TV that
  takes about 3 s, still passes). Once the seeks are spent, it shows the TV's real place and
  logs `<tv> did not go to <time> after 3 seeks, so it plays from <time>`.
- The unit test of the old rule (`…or_has_had_half_a_minute_to`) asserted the 30 s give-up, which
  is the behaviour removed here, so it was rewritten for the new rule.

**Tests.** With a fake TV that plays before it knows the length and loses one more seek after,
`a_cast_resumes_once_the_tv_knows_the_length_and_seeks_again_when_a_seek_is_lost` failed before
(one seek, sent too early) and passes now (none before the length, two after).
`a_cast_whose_tv_never_takes_the_resume_shows_where_the_tv_really_is` failed before (one seek,
the intended time shown for 30 s) and passes now (three seeks, then the real place).

**Live check.** gmrender-resurrect 0.3.1 from nixpkgs, `newsreel tv play <download> --at 10:00`
on a 20 minute test video served from a loopback NNTP server: PLAYING at 17:23:26.07, length
known at 26.72, `RelativeTimePosition: 0:10:00` at 27.07, and `newsreel tv` showed 602, 606, 610
seconds as it played (log in `~/.cache/nr-devfix/gmr-resume-after.log`).

**Upstream.** Same code in v0.7.0 (`Resume::over`, `RESUME_WITHIN` of 30 s). Upstream has it.

## 2b. A renderer that runs libupnp drops the Play after SetAVTransportURI

**Bug.** Found while checking fix 2 against gmrender: every cast failed with `nr-fix-renderer
cannot be reached (error sending request for url (…/upnp/control/rendertransport1))` right after
the renderer logged the new URI.

**Evidence.** Two SOAP requests on one kept-alive connection from Python: the first gets `200`
with no `Connection: close`, the second gets `BrokenPipeError`. libupnp answers one request per
connection without saying so. The cast's reqwest client keeps connections for reuse, so the Play
sent right after SetAVTransportURI goes down the dead one. The earlier device test did not hit
it, so it depends on timing.

**Fix.** The cast client keeps no idle connections (`pool_max_idle_per_host(0)`). A TV's control
requests are a few a second, so a new connection each costs nothing that matters.

**Test.** `a_tv_that_hangs_up_after_each_answer_without_saying_so_still_plays_the_cast`: the fake
TV keeps the connection open after its answer and drops the next request on it. It failed before
with the same error as gmrender, and passes now.

**Upstream.** Same client in v0.7.0. Upstream has it.

## 3. Hand edits to config.yaml are ignored until the engine restarts

**Bug.** An indexer added to config.yaml by hand showed up in `newsreel indexers`, which reads
the file, but searches never asked it, and nothing said why.

**Cause.** `serve` in `daemon/mod.rs` loads config.yaml once and builds everything from it.
Only the engine's own writes (`indexers add` and `remove`) call `SearchPort::reconfigure`.

**What upstream's design supports.** Upstream already re-reads some settings each time it uses
them (preferences, the update policy, the player) and can swap a running search's indexer list
(`Search::reconfigure`, used by `indexers add`). Everything else is built once at start: the NNTP
pool and its connections (the `usenet` crate's `Pool` has no way to change its servers), the
downloads folder the owner and disk detector watch, the network listener and its exposure, and
the subtitle, catalog and ffmpeg clients. Rebuilding those in place would mean restarting most of
the engine from inside, which is what a restart already does safely (it saves state first).

**Fix** (`daemon/config_watch.rs`). The engine looks at config.yaml every 2 seconds. When it
changes:
- it loads it; one that does not load is logged and the running settings are kept;
- a changed indexer list or `sources.disabled` goes to `reconfigure`, so searches use it at once;
- a change to a setting read only at start (`downloads.dir`, `usenet.servers`, `subtitles`,
  `catalog`, `ffmpeg`, `plays.max`, `server`, `public_address`) is logged, and written to
  `state/restart-needed`, which `newsreel engine status` shows as its notice, with how to restart
  (`newsreel engine stop`; the next command starts it again). Undoing the edit clears it.

**Test.** `the_engine_searches_indexers_added_by_hand_and_says_when_a_change_needs_a_restart`
(`crates/newsreel/tests/search.rs`) failed before (the new indexer was never asked) and passes
now. `only_what_the_engine_reads_once_needs_a_restart` checks which settings count.

**Upstream.** Same single load in v0.7.0 (`Config::load` in `serve`). Upstream has it.

## 4. Hints that do not fit their reader

### 4a. The web app says "retry with a longer --timeout"

**Bug.** When every indexer failed, the web app's search and title pages showed the hint
`check the network connection, or retry with a longer --timeout`. `--timeout` is a command line
flag; a browser has nothing to set.

**Cause.** The hint is fixed in `all_failed` (`crates/search/src/assemble.rs`), and the engine
hands the same error to the CLI and to the web API.

**Fix.** `failed_hint(sources, reader)` picks the hint from how the sources failed and who reads
it. The CLI keeps its wording. The web API (`web::fail`, and the last line of a title play)
replaces the hint of a `providers_unavailable` error with the browser's: `check that the computer
running newsreel is online, then search again`. It works from the error's structured
`details.sources`, not by editing text.

**Upstream.** v0.7.0 has the same fixed hint in `all_failed`. Upstream has it.

### 4b. A Newznab address without `/api`

**Bug.** With NZBHydra's address written without `/api`, the search failed with
`expected an RSS feed, got <html>` (right) and the hint `check the network connection` (wrong:
the network was fine). `newsreel indexers add` refused the same address.

**Fix.**
- A search whose address answers a web page, and does not end in `/api`, asks `<address>/api`
  once. When that answers the feed, the source keeps using it (`Newznab::api`). When it does not,
  the failure stays the web page one.
- `indexers add` and the check `newsreel indexers` runs do the same, and `add` writes the `/api`
  address it found to config.yaml.
- When an indexer still answers a web page, the hint names `/api`: `an indexer answered a web page,
  not its feed: its url in config.yaml must be its Newznab API, which usually ends in /api, such as
  http://127.0.0.1:5076/api for NZBHydra`.

**Tests.** `an_indexer_given_by_its_base_address_is_searched_at_its_api`,
`an_indexer_that_answers_a_web_page_gets_a_hint_about_its_api_path_not_the_network`
(`crates/newsreel/tests/search.rs`) and `an_indexers_base_address_is_added_as_its_api`
(`crates/newsreel/tests/indexers.rs`) failed before and pass now.
`a_failed_search_hints_at_what_its_reader_can_do_and_at_api_for_a_web_page` checks both readers.

**Live check.** The test engine with the real NZBHydra written as `http://<host>:5076` (no `/api`):
`newsreel search "charade 1963"` listed its releases, and `newsreel indexers` showed it answering
with its categories.

**Upstream.** Upstream searched Torznab feeds, which have the same `/api` convention, through the
same fixed hint. The fallback to `/api` is new here.

## 5. TV Bro on Android TV is not taken for a TV

**Bug.** On the Android TV emulator, TV Bro got the phone and computer layout: no remote focus
rings, small type. Only `?tv=on` turned TV mode on.

**Evidence.** A probe page opened in TV Bro 3.0.6 on `august_tv` reported (in
`~/.cache/nr-devfix/signals/server.log`): user agent `Mozilla/5.0 (Linux; Android 10; K) …
Chrome/157.0.0.0 Safari/537.36`, screen 960 by 540 at 2x, `maxTouchPoints` 5, `(pointer: coarse)`
and `(hover: none)` true, `(any-pointer: fine)` true, `(any-pointer: none)` false. `looksLikeTv`
needs no touch and either no pointer or a TV name in the user agent, so all three tests fail.

**Fix** (`web/src/lib/tv.ts`).
- Android-based TV names (Android TV, Google TV, Fire TV's `AFT…` models, BRAVIA, SHIELD,
  Chromecast, Mi TV) count whatever touch the browser claims, since those browsers claim touch.
- The other TV platforms (webOS, Tizen, SMART-TV, NetCast, HbbTV, VIDAA) and a missing pointer
  count as before, without touch.
- New: a coarse main pointer that cannot hover, on a 16:9 landscape screen at least 960 wide, with
  no phone or iPad in the user agent. That is what TV Bro reports. Tablets report the same pointer,
  but their screens are 16:10 or 4:3, so they stay out.
- `?tv=on`, `?tv=off` and `?tv=auto` are unchanged.

**Tests.** `web/src/lib/tv.test.ts`: TV Bro's signals and four Android TV-family user agents
failed before and pass now; tablets in 16:10 and 4:3 stay out; the old cases still pass.

**Upstream.** Same `looksLikeTv` in v0.7.0. Upstream has it.

## 6. Remote navigation

### 6a. The Pair button cannot be reached with a remote

**Bug.** On the TV, the pair page opens with focus in the code field. Down, OK and Enter stay in
the field, and nothing else moves focus, so a remote never reaches Pair
(`~/.cache/nr-devtest/shots/tv-04-pairfocus.png`).

**Fix** (`crates/daemon/src/daemon/web/pair.html`). Down moves from the code to Pair and Up back;
OK or Enter in the code pairs (and says the code needs six digits when it is short); the sixth
digit moves focus to Pair. Keys are read by name, or by key code for TV browsers that send none.

**Test.** `web/src/pair.test.ts` runs the page's own script against a small stand-in DOM. Both
tests fail on the old page (`PAIR_PAGE=<old copy> bun test src/pair.test.ts`) and pass now.

**Upstream.** Same page in v0.7.0, with no key handling. Upstream has it.

### 6b. The player ignores media keys

**Bug.** On a TV the player answered only Enter, Up and Down from a remote
(`REMOTE` in `web/src/player/keys.ts`); Play/Pause, Fast Forward, Rewind and Stop did nothing.

**Fix.** `MediaPlayPause` plays or pauses, `MediaPlay` and `MediaPause` play and pause (so a
browser that also passes the key to the media session does not undo it), `MediaFastForward` and
`MediaRewind` go 10 s, and `MediaStop` leaves the player as Back does. LG and Samsung often send
only key codes (415, 19, 10252, 417, 412, 413), which map the same way. Media keys work wherever
focus is, since no control uses them.

**Test.** `a remote's media keys play, pause, wind, and stop, by name or by a TV's key code,
wherever focus is` in `web/src/player/keys.test.ts` failed before and passes now.

**Upstream.** Same `REMOTE` map in v0.7.0. Upstream has it.
