# Device test fixes

Fixes for the bugs the live device tests of 2026-11-18 found, one commit each. For each: the bug,
how it was found, the evidence, why the fix is right, and whether upstream (nexxeln/torrential
v0.7.0) has it too. Screenshots are in `~/.cache/nr-devfix/shots/` (before and after, `7-…-before` from the old build
and `7-…-after` from this branch) and `~/.cache/nr-devtest/shots/` (the original test run). Device
names: `7-devices-before.txt` ("Android", "Android 2") and `7-devices-after.txt` ("Android TV",
"Android phone"). `devices add` output: `7-devices-add-after-pipe.txt`.

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
- Found on Windows CI: once a stopped download's progress is saved, the owner takes it out of the
  engine, and `want_range` then knows nothing of it, so a read waited the 5 minute stall again.
  The owner's `StreamFile` answer now carries a stopped download's error, and a read that has no
  bytes to send fails with it (`503` before it starts, or the connection ends mid-answer). The
  test reads only after the download has stopped and left the engine, which hung on Linux too
  before this.

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
seconds as it played (log in `~/.cache/nr-devfix/shots/gmr-resume-after.log`).

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
`check the network connection, or retry with a longer --timeout`
(`~/.cache/nr-devtest/shots/tv-play-3.png`). `--timeout` is a command line flag; a browser has
nothing to set.

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
`~/.cache/nr-devfix/shots/tvbro-signals.log`): user agent `Mozilla/5.0 (Linux; Android 10; K) …
Chrome/157.0.0.0 Safari/537.36`, screen 960 by 540 at 2x, `maxTouchPoints` 5, `(pointer: coarse)`
and `(hover: none)` true, `(any-pointer: fine)` true, `(any-pointer: none)` false. `looksLikeTv`
needs no touch and either no pointer or a TV name in the user agent, and TV Bro gives none of
those.

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

**Screens.** `7-tv-home-before.png` (no TV mode: the computer's `/` key hint, no focus ring) and
`7-tv-home-after.png` (TV mode with no `?tv=on`: the thick focus ring on the search field), on
engines at different addresses, so a kept `?tv=on` cannot carry over.

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

**On the emulator.** The arrow keys could not be shown there: TV Bro 3.0.6 on `august_tv` ignored
every remote key agent-device sent (a probe page logged no `keydown` at all, its cursor did not
move, and Menu did nothing), so the emulator says nothing either way about keys. What it does
show: after typing the sixth digit, focus is on Pair (`7-tv-pair-after-sixth-digit.png`; the
accessibility tree says `[button] "Pair" [focused]`), where the old page left it in the field
(`7-tv-pair-before-1.png`).

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

**On the emulator.** Not shown, for the reason in 6a; the player also did not start playback in
TV Bro on the emulator (`7-tv-playing-before*.png`).

**Upstream.** Same `REMOTE` map in v0.7.0. Upstream has it.

## 7. Wording and layout

### 7g. Paired devices named "Android" and "Android 2"

**Bug.** The phone (Chrome) and the TV (TV Bro) were both named from their user agents as
"Android", so the devices list showed "Android" and "Android 2".

**Evidence.** Both browsers send Chrome's reduced user agent, `Mozilla/5.0 (Linux; Android 10; K)
…`, where `K` stands for every model. The phone's adds `Mobile`; TV Bro's does not, and neither
names a TV (signals in fix 5).

**Fix.**
- A model in the user agent names the device (`Android 14; Pixel 8` gives "Pixel 8"; ` Build/…`
  is dropped).
- With no model, `Mobile` gives "Android phone", and its absence "Android tablet".
- The pair page sends `tv: true` when it sees a TV by the web app's own signals (or a kept
  `?tv=on`), which names a non-mobile Android browser "Android TV", and another non-handheld one
  "TV". An iPhone stays an iPhone.
- More TV names are read: SHIELD and Google TV.

**Found while checking it on the emulators.** The first build named the phone "k": the HTTP layer
lowercases every header value, the User-Agent too, so the reduced UA's `K` was not recognised and
models would read "pixel 8". The request now keeps the User-Agent as sent (it is only used for the
name). `a_request_keeps_its_user_agent_as_sent` (`daemon/http.rs`) failed before that change. A
first version of this test paired over the network with a second engine listening on every
address; on macOS CI it made `the_engine_keeps_trying_the_home_network_while_another_program_holds_its_port`
fail twice (main passed the same job), since macOS lets 127.0.0.1 and 0.0.0.0 sockets share a port.

**Tests.** `a_device_is_named_by_what_its_browser_runs_on` gained the Pixel 8, a Samsung tablet
model, both reduced user agents, SHIELD and Google TV (the Pixel 8 case expected "Android" before,
which is the naming this fix replaces). `a_device_whose_pair_page_saw_a_tv_is_named_for_one` is new.
`web/src/pair.test.ts` checks the page sends the field.

**Upstream.** Same name table in v0.7.0, with `("android", "Android")`. Upstream has it.

### 7a. Keyboard keys named on TVs and touchscreens

**Bug.** The player's buttons are labelled "Back (Esc)", "Play (Space)", "Mute (M)" and so on.
The label is also the tooltip and the accessible name, which a TV's or a phone's browser reads
out, where no keyboard exists to press them. Reported from the device tests; their screenshots do
not show a tooltip, so the evidence is the labels in `Chrome.tsx` and `Controls.tsx`.

**Fix.** `withKey(label, key)` (`web/src/lib/keyboard.ts`) adds the key only when the browser is
not a TV and has a pointer that hovers and is fine, as a computer's mouse or trackpad is. The back
button, play, mute, subtitles, fullscreen and next use it.

**Test.** `web/src/lib/keyboard.test.ts`.

**Upstream.** Same fixed labels in v0.7.0. Upstream has it.

### 7b. `newsreel devices add` says "scan the QR code" with no QR code printed

**Bug.** Piped or with `--plain`, `devices add` prints no QR code (only a person at a terminal gets
one) but still ends with `# scan the QR code with the phone's camera, …`
(`~/.cache/nr-devtest/logs/devices-add-pipe.txt`, `devices-add.txt`).

**Fix.** `offer_doc` takes whether a QR code was shown (`Qr::of(mode)`). Without one it says
`open the address on the phone or TV and enter the code`. `server install` uses the same text.

**Test.** `adding_shows_the_address_and_the_code_to_type` expected the QR wording in plain output,
which is the bug; it now expects the plain wording there and the QR wording when one is shown.

**Upstream.** Same text in v0.7.0. Upstream has it.

### 7c. "Found 10 sources", then "source 1 of 6"

**Bug.** The play screen said "Found 10 sources", then "Trying source 1 of 6"
(`7-tv-play-before-2.png`). The CLI and TUI say the same ("Found 6 sources",
"Trying 1 of 6").

**Which number is right.** Both. 10 is every release the search returned. 6 is how many this play
will try: `MOST_TRIED` is 6, and `next_up` leaves out releases whose names say a codec this screen
does not decode (`daemon/titles/attempt.rs`). The second number was only missing what it counts.

**Fix.** The trying line says "Trying 2 of the best 6", or "Trying the best source" when there is
one, in the web app (`which` in `TitleScreens.tsx`), the CLI and the TUI (`watching::trying` in
core). "Playing source 1 of 6" reads "Playing source 1 of the best 6".

**Screens.** `7-tv-play-before-2.png` ("Found 10 sources", "Trying source 1 of 6") and
`7-tv-play-after-3.png` ("Found 10 sources", "Trying source 1 of the best 6").

**Tests.** `TitleScreens.test.ts`, `a_play_names_the_source_it_tries_among_the_best_it_tries` in
core; the CLI, TUI and `tests/titles.rs` expectations of the old line now expect the new one.

**Upstream.** Same wording in v0.7.0. Upstream has it.

### 7d. TV: "Continue watching" runs off the screen, and the title page's actions sit on its edge

**Bug.** On the TV (960 by 540 CSS pixels in TV Bro), the one Continue watching card was cut off
at the bottom (`~/.cache/nr-devtest/shots/tv-25-home.png`), and the title page's Play, Play on TV
and Save buttons sat on the bottom edge (`tv-20.png`).

**Cause.** At 960 wide the card row is two columns (Tailwind's `sm`), so one card is 420 pixels
wide and 236 tall, under a header and a search field that a TV's larger type already makes tall.
The title page pushes its text down by `min(24vh, 13rem)` and shows the whole synopsis.

**Fix.** On a TV the card row has three columns, and the title page starts 4vh down, with tighter
gaps and the synopsis clamped to three lines.

**Screens.** Before: `7-tv-home-continue-before.png`, `7-tv-title-before.png`. After:
`7-tv-home-continue-after.png`, `7-tv-title-after.png` (in `~/.cache/nr-devfix/shots/`). The
watch position behind the card was saved through `POST /api/watched`, since the emulator's player
did not start the film.

**Test.** Layout only; checked on the TV emulator. The web app has no DOM test setup.

**Upstream.** Same classes in v0.7.0. Upstream has it.

### 7e. Phone: the Play on TV list covers the "Sources" heading

**Bug.** On a phone, Play on TV opens its list as a sheet fixed to the bottom of the screen, over
whatever is there, with nothing to set it apart: on the title page, over the "Sources" heading
(`~/.cache/nr-devtest/shots/ph-09-cast.png`, again in `7-phone-cast-sheet-before.png`).

**Fix.** The sheet sits over a dimmed page, so it reads as on top of the page, as a phone's sheets
do; a tap on the dimmed page closes it, as a tap outside did. Moving the list into the page instead
would push the buttons around under the finger.

**Screens.** Before: `7-phone-cast-sheet-before.png`. After: `7-phone-cast-sheet-after.png`.

**Upstream.** Same sheet in v0.7.0. Upstream has it.

### 7f. No title on screen until the logo arrives

**Bug.** A title page showed no name until its logo loaded, 4 s or more on the TV
(`~/.cache/nr-devtest/shots/tv-20.png`, and `7-tv-title-before.png` here 7 s in).

**Cause.** `Logo` keeps the picture out of the layout and invisible until it is measured, and shows
its text fallback only when the picture fails, so the heading is empty while it loads.

**Fix.** The text name shows until the logo is measured, then the logo takes its place (the same
picture element, so it still fades in). A logo that fails leaves the text, as before.

**Screens.** `7-tv-title-loading-after.mp4` and the frame `7-tv-title-loading-after.png`: Singin'
in the Rain's name in text for the 9 s its logo and backdrop took to come from the catalog, and
`7-tv-title-logo-loaded-after.png` with the logo in place on the next visit.

**Upstream.** Same `Logo` in v0.7.0. Upstream has it.

## 8. No build for Intel Macs

**Bug.** `install.sh` said "newsreel has no prebuilt binary for Darwin x86_64" on the Intel
MacBook (macOS 15.8), and the updater said "there is no prebuilt newsreel for macos x86_64".

**Fix.**
- The release workflow builds `x86_64-apple-darwin` on the same arm64 `macos-15` runner, as a
  cross build (Apple's toolchain links both architectures, and newsreel is pure Rust), and checks
  the binary runs there under Rosetta. The asset is `newsreel-x86_64-apple-darwin.tar.gz`.
- `scripts/release.sh` takes `RELEASE_TARGET`, builds the other Mac architecture with
  `--target`, and checks with `lipo` that the binary is the one it was asked for. It also builds
  natively on an Intel Mac.
- `install.sh` installs it on an Intel Mac. A shell under Rosetta on Apple silicon also says
  x86_64; `sysctl.proc_translated` tells them apart, and that Mac gets the native build.
- The updater's `target` names it for `macos x86_64`, so `update --check` and `install.sh` agree.
- The README says Intel Macs run it.

**Test.** `every_machine_the_release_builds_for_has_its_asset` failed for `macos x86_64` before.
The workflow was dispatched on this branch, where it publishes nothing (the draft release and the
image push run only for a tag); its result is in the report.

**Not done.** ffmpeg has no pinned build for Intel Macs (`ffmpeg/fetch.rs`), so a conversion there
needs an installed ffmpeg. `install.yml`, which installs a published release on each platform, has
no Intel runner yet.

**Upstream.** v0.7.0's `target` has only `("macos", "aarch64")`. Upstream has it.

## 9. `engine install` edge cases

### 9a. A missing launchctl, systemctl or schtasks is blamed on ssh

**Bug.** With `systemctl` (or `launchctl`, or `schtasks`) missing from the PATH, `engine install`
said `systemctl is not on this system` and then hinted that systemd's user manager does not answer
"as under su, sudo or ssh without a login session", which is about a different failure.

**Cause.** `service::output` sent a program it could not find and a program that refused to the
same `unavailable` hint.

**Fix.** A program that is not found gets its own hint (`service::missing`): it says the program is
not on the PATH, where it lives on that system (`/bin` on a Mac, System32 on Windows), and, for
systemd, that the system may have none and any newsreel command starts the engine meanwhile.

**Test.** `an_engine_install_the_login_service_refuses_says_so_and_what_to_do` now also checks the
missing-program hint names the PATH and not ssh; that part failed before.

**Upstream.** v0.7.0 has no `service_unavailable`; newsreel added it, so this is newsreel's own.

### 9b. `engine uninstall` says "uninstalled" and leaves an engine running

**Bug.** When `engine install` fails, it starts the engine it stopped again, outside the login
service. `engine uninstall` then removed the unit, said `uninstalled`, and left that engine running.

**Cause.** launchd and systemd stop the engine they started when the service is removed, and the
command stopped the engine itself only for Task Scheduler, which does not. An engine none of them
started was never stopped.

**Fix.** After it removes a service, `engine uninstall` stops any engine still running for this
home, whatever the manager, and says so: `stopped: the engine, pid N` (`"stopped": N` in JSON).
When there was no service it leaves a running engine alone, as before.

**Test.** `uninstall_after_a_failed_install_stops_the_engine_the_install_started_again`
(`crates/newsreel/tests/process.rs`) failed before (no `stopped`, engine alive) and passes now.

**Upstream.** v0.7.0 stops the engine itself only for Task Scheduler too. Upstream has it, though its
failed install path is newsreel's.
