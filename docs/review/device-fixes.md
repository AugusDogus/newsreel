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
  hooks in there (section 10 does).
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

**Since done** (section 10): ffmpeg has a pinned build for Intel Macs, and `install.yml` installs
the published release on an Intel runner too.

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

## 10. PAR2 repair and request priority

Wires in the `usenet` crate's 0.3 work (branch `par2-priority`, PR AugusDogus/usenet-rs#1): PAR2
repair and request priority. The crate is a git dependency pinned to a rev until it is published.

### 10a. Charade plays through its damaged article

**Before.** Fix 1 made a read across Charade's damaged article fail at once with the reason, so a
title play moves on to its next source and `newsreel get` stops at the end. The release carries PAR2
volumes that rebuild the article, as SABnzbd does, and the user chose that: repair, and fall back to
another source only when repair cannot work.

**Fix** (`daemon/usenet/repair.rs`).
- The resolver keeps the NZB beside the manifest when the NZB posts PAR2 files (the stored release is
  now `NRR1`, the manifest's length, the manifest, then the NZB), since the manifest leaves PAR2 files out.
- When a piece is set aside and the NZB has PAR2 volumes, a repair task starts. It fetches the set's
  index, maps the damage to slices, and fetches only as many recovery blocks as those slices need
  (`Par2Files::choose`, one more when the blocks happen to be dependent). The download keeps going.
- Mapping: the set protects the posted files, here the RAR volumes, not the mp4. Each piece of the mp4
  is a byte range of one volume (its article's `begin + skip`), so the damaged piece falls in 2 of the
  set's 384,000-byte slices. The volumes' other bytes, RAR headers and any stored file the play does
  not keep (such as a sample), are fetched from their articles and held in memory, up to 256 MiB.
  Posted files are matched to the set by name, else by size and first 16 KiB, since posters rename:
  Charade's NZB says `part010.rar` where its PAR2 set says `part10.rar`.
- Every other slice is fed to the crate's `Repair` as its pieces land. Once none is waiting, it
  rebuilds, writes the mp4's share of the rebuilt slices, checks the pieces' CRCs on disk, marks
  them done, and the waiting read goes on.
- Status says so in plain words. `newsreel status` gains a `repair:` line (`repairing a damaged part,
  fetching its recovery files`, then `repairing a damaged part, about 1m left`, then `almost done`),
  the JSON gains `progress.repair` (`articles`, `stage`, `blocking`, `eta`), the TUI says `repairing`,
  and the web player says `Repairing a damaged part of this video · about 1m 23s left` instead of
  "Waiting for articles". `blocking` is true while a reader waits on the damaged bytes. A repair
  does not count as stalled: not to the owner's no-source timer, not to the HTTP read's 5 minute
  stall timeout, and not to the title player's switch to the next source.
- Restart: the journal records a piece set aside (`journal::Recorded::Lost`), so an engine that
  restarts mid-repair goes on repairing without asking the servers for the article again.
- Fallback: with no PAR2 files, too few blocks, or blocks that cannot solve the slices, the piece
  stays set aside with the reason (`the release's PAR2 files cannot rebuild it: the damage spans 2
  slices of the PAR2 set, and the release's PAR2 files hold 1 recovery block`). A reader waiting on
  it fails at once, as before, so a title play moves to its next source; a plain download fetches
  the rest and stops with the reason.

**Tests** (`crates/newsreel/tests/usenet.rs`, against the fake server, with `fake::par2` posts and
the new `FakeServer::damage`, which serves an article cut before its `=yend` line as Charade's was):
`a_damaged_article_is_rebuilt_from_par2_and_a_read_across_it_gets_the_posted_bytes`,
`a_damaged_article_inside_rar_volumes_is_rebuilt_from_the_volumes_par2_set` (RAR5 volumes with a
sample the play does not keep), `too_few_recovery_blocks_fail_a_waiting_read_at_once_and_stop_a_download_with_the_reason`,
`a_repair_cut_short_by_an_engine_restart_goes_on_and_finishes`, and the no-PAR2 case in
`a_read_across_an_article_no_server_has_fails_at_once_and_the_download_says_why`. Unit tests cover
the journal's lost records, the status wording, and the repair's ETA.

**Live check.** A test engine on its own `NEWSREEL_HOME` and port 47990 played the real NZB, with the
four real servers. The repair log:

```
c17883 cannot fetch bytes 930050289..930818289 of Charade.1963.BluRay.720p.H264.mp4: article <5214f544$0$16594$c3e8da3$e3f2c276@news.astraweb.com> arrived damaged: yEnc data for "…part09.rar" decoded to 767999 bytes, but its header declares 768000; …
c17883 repairs 1 damaged article from its PAR2 files: 2 slices of 384000 bytes, 2 recovery blocks fetched in 5.3s
c17883 repaired 1 damaged article in 0.5s once the rest of the release was in, 89.4s after it was found
```

| | 12 MiB/s cap | no cap (about 100 MB/s) |
|---|---|---|
| Release still to download when the read reached the gap | 70% | 23% |
| Read across the gap (`930000289..930868289`) to its last byte | 90.2 s | 5.8 s |
| Of which: PAR2 index, volume headers and 2 recovery blocks fetched, beside the download | 5.3 s | 7.2 s |
| Of which: rebuild once the rest was in | 0.5 s | 0.1 s |

Nearly all of the wait is the rest of the release arriving, which repair reads. The read returned all
868,000 bytes, and they equal the finished file's. A first build fetched the volumes' header articles
one volume at a time, and fetching took 24.9 s; they now go in one batch.

**par2cmdline.** `~/.cache/nr-par2/scripts/volumes.py` rebuilds the 15 RAR volumes from the finished
mp4 alone, plus each volume's RAR headers from its own articles (31 articles, its own NNTP and yEnc
code, nothing from newsreel). par2cmdline 1.4.0 (nixpkgs) against the release's own
`vol-01.par2`: `All files are correct, repair is not required.` As a control, zeroing the damaged
article's 768,000 bytes in the rebuilt `part09.rar` gives `damaged. Found 272 of 274 data blocks` and
`You need 2 more recovery blocks`; restoring them gives `All files are correct` again.

**Upstream.** libtorrent has no PAR2; this is newsreel's own.

### 10b. A seek during a download goes first

**Bug.** On the Mac's 12 MiB/s link, a seek during a running download waited about 27 s: the engine
ran as many requests as the pool had room for (105 connections, 4 deep), and a seek's article
shared the link with all of them.

**Fix.** The crate's 0.3 pool holds background requests to a self-tuned limit, the fewest that keep
the link full, and lets urgent ones skip it.
- The pieces an HTTP range read needs now (up to 16) are fetched at once at `Priority::Urgent`,
  beside the workers, even when a worker already waits on them in the pool (`rush` in
  `daemon/usenet.rs`); whichever lands first is kept.
- A release's first reads are urgent too: the resolver's NZB, first articles and RAR headers, the
  start of the video and its end (the play's first range reads), and a repair's PAR2 reads.
- Workers stay at the pool's capacity, which lets the pool see when the limit holds them back.
- `connections` in status counts requests that can be on a connection (background ones up to the
  pool's limit, plus urgent ones), at most the connections open. It was in-flight requests at most
  the connections open, which read 105 while most of them waited.

**Live check.** The same test engine, 1 MiB range reads at 45%, 70%, 80% and 90% of the mp4 while the
download was between 25% and 80% done, against main (`7358535`) built the same way. The cap is a
relay inside a network namespace of the test engine's own (pasta, with `/etc/hosts` and
`/etc/nsswitch.conf` bind-mounted there) that holds what the servers send to 12 MiB/s together; TLS
goes end to end. tc could not do it without root: the namespace has no ingress or tbf qdisc, and
newsreel's own per-download limit paces fetches rather than filling a link.

| Seek, 1 MiB | main | this branch |
|---|---|---|
| 12 MiB/s cap | 25.5, 26.6, 13.6, 12.9 s (105 connections) | 1.2, 1.0, 1.0, 0.75 s (11 to 13 connections); another run 1.6, 3.3, 2.0, 2.2 s (24 to 34) |
| no cap | 5.5, 4.8, 1.0, 1.7 s | 4.1, 0.78, 0.49, 0.58 s; another run 3.1, 2.0, 1.4, 2.1 s |
| Resolving another NZB meanwhile (`newsreel files`), capped | 9.2 s | 0.89 s, 1.2 s |
| Same, no cap | 0.79 s | 0.45 s, 0.32 s |

With no cap the servers' own per-connection speed is what limits, so the pool keeps all 105
connections busy and a seek queues behind one of their articles; the gain is smaller there.

### 10c. Intel Mac gaps

- ffmpeg: jellyfin-ffmpeg 8.1.3-2's `portable_mac64` build is pinned for `macos x86_64`, beside the
  arm64 one, with the size and SHA-256 its release publishes (checked against a download: both
  programs are x86_64 Mach-O).
- `install.yml` installs the published release on `macos-15-intel` as well.

**Not done.** The crate's `article::fetch` still stops at the first copy that does not decode (fix 1).
A release whose PAR2 files are posted under obfuscated names is not repaired, since `Par2Files::find`
goes by name.


# Second retest

Fixes for what the second round of device tests found (Android phone and TV emulators and the Linux
engine on 2026-11-19, and the Intel Mac, whose evidence stays on the Mac). One commit each. The
retest's screenshots and logs are in `~/.cache/nr-retest/shots/`; the before and after screenshots
and logs for these fixes are in `~/.cache/nr-retest2/shots/`, named by section (`7-tv-start-before.png`,
`7-tv-start-prompt-after.png`). The `usenet` crate's part is on its `par2-priority` branch
(`dbdd261`, `ceb8860`, `7f67175`), and newsreel pins `7f67175`.

## 11. A repair's recovery files, and its ETA

**Bug.** On the Mac (10 to 50 MiB/s, 105 connections) the player said "fetching its recovery files"
for the whole wait, status stayed `{"stage":"preparing","blocking":true}` with no ETA, and the log said
the PAR2 index, headers and recovery blocks took 35.8 s and 36.1 s (5.3 to 7.2 s on Linux).

**Cause.** Three things.
- The repair's fetches were already urgent, but an urgent request goes to the connection with the
  fewest requests in flight, and when every connection carries background requests it queues
  behind one of them. Replies come in the order commands went out, so it waits for that article.
  A Mac link where each connection is slow, and the pool's background limit had grown past the
  number of connections, makes that seconds per round, and the repair fetches in rounds (the
  index, the volumes' first articles, their headers, the recovery blocks).
- The repair fetched each volume's first article twice: once to match it to the PAR2 set by size
  and first bytes, then again for its RAR headers.
- Status gave a repair an ETA only in the `waiting` stage, and the download's own ETA disappeared
  whenever one second landed nothing (section 12).

**Fix.**
- Crate (`dbdd261`): an urgent request never queues behind a background one. With no connection
  free of background requests it waits for the first to free up, which is never later than
  queueing behind the least loaded one, since background requests wait while an urgent one does.
  Test: `an_urgent_request_waits_for_a_connection_without_background_requests_rather_than_queue_behind_one`
  (3.04 s before, behind the slow article; about 0.3 s now).
- The repair's fetches go at urgent priority while a reader waits on the damaged bytes
  (`blocking`), and beside the download otherwise, so a repair nobody waits on does not slow a seek.
  The first articles fetched to match volumes are kept for their headers.
- A repair has the download's ETA while it fetches its recovery files too: rebuilding reads the
  whole release, so it is done when the rest of the release is in, and the recovery files are a
  few dozen articles beside it. With the rest already in, nothing says how long they take, so it
  shows none. The player says "Repairing a damaged part of this video · about 1m 55s left" in both
  stages.

**Tests.** `a_repair_has_the_downloads_eta_while_it_fetches_its_recovery_files_and_waits_for_the_rest`
(`daemon/book.rs`), `a_repair_says_what_it_waits_for_in_plain_words` (core), and `format.test.ts`;
the repair integration tests still pass.

**Live check** (a fresh test home playing the real NZB; a reader asks for bytes
930,000,289..930,868,289 at once, and status is read every second;
`1-2-repair-timeline-after-*.log`):

| | 12 MiB/s cap | no cap |
|---|---|---|
| Recovery files fetched (log) | 20.1 s | 2.2 s (the retest: 5.3 to 7.2 s) |
| First ETA | 18 s after the play started, still `preparing`: 136 s | 17 s: 16 s |
| Read across the gap returned | 127.2 s, all 868,001 bytes | 32.9 s |

Under the cap the recovery fetch took longer than the retest's 12.2 s, but that run found the
damage with 70% of the release still to come and no reader waiting yet; here the reader waits from
the start and the release is 115 s from done, so the fetch is never what the reader waits on. The
Mac was not tested again: its evidence and engine stay on the Mac.

**Upstream.** libtorrent has no PAR2 and the pool is newsreel's: newsreel's own.

## 12. The ETA jumps around

**Bug.** The repair ETA read 125, 492, 726, 102 and 465 s within 12 s (`repair-api-timeline.log`).

**Cause.** The engine measures the rate as the bytes that landed in the last second, and articles
land whole, about 768 KB each, so one second's rate swung between 0.7 and 13.7 MB/s while the mean
was about 5. The ETA was the bytes left over that one second's rate, and a second with nothing
landed reset the 10 s wait before any ETA shows.

**Fix** (`daemon/eta.rs`). The rate is the mean over the last 30 s. The ETA shows once bytes have
arrived for 10 s, and a second with nothing is part of the mean, not the end of the estimate. The
time shown counts down by itself and moves toward a new estimate by at most a fortieth a second (at
least 2 s), so the minutes change gradually.

**Tests.** `the_time_left_moves_gradually_through_a_rate_that_swings_and_follows_the_mean` feeds
the retest's own 3 s rates: over the 12 s in which the old ETA said 125 to 726 s the new one stays
within a fifth, every step is the countdown plus at most a fortieth a second, and it starts within
a fifth of the mean. `an_eta_waits_until_the_rate_has_held_for_ten_seconds` asserted the reset on a
second with nothing, which is the behaviour replaced; it is now
`an_eta_waits_until_bytes_have_arrived_for_ten_seconds_and_outlasts_a_second_without_any`.

**Live check.** Under the cap the ETA went 136, 128, 125, 123, 114, 109 ... 4, 1 s, one reading a
second, never up by more than the step, and the repair finished 115 s after the first reading.

**Upstream.** v0.7.0 has the same rule and formula (`ETA_AFTER`, bytes left over the rate), but
libtorrent's rate is already averaged, so the jumps are newsreel's.

## 13. PAR2 files posted under obfuscated names

**Bug.** A release whose PAR2 files have hidden names was not repaired: `Par2Files::find` goes by
`.par2` names.

**Fix.** The resolver already reads every posted file's first article (it skips only files named
`.par2`), and dropped those starting with `PAR2\0PKT`. Crate (`ceb8860`): it now lists them in
`Manifest::par2` with the recovery set ID from their first packet's header, and
`Par2Files::find_resolved` finds those sets beside the named ones, grouped by set, so a sample's set
stays apart. Each file of such a set is a volume whose blocks are estimated from its size, and the
smallest is read for the description. A manifest with none writes no `par2` field. newsreel keeps
the NZB when either finds a set, and its repair skips the set's files when it matches posted files
to the set by size and first bytes.

**Tests.** Crate: `par2_files_posted_under_obfuscated_names_are_found_by_their_first_article`.
newsreel: `a_damaged_article_is_rebuilt_from_par2_files_posted_under_obfuscated_names` (RAR volumes
and PAR2 files all under hex names) failed before (the read got `503`) and passes now.

**Upstream.** No PAR2 upstream: newsreel's own.

## 14. A log that says two things

**Bug.** The engine log said "the article is damaged, so fetch another copy; the copy on the server is
bad, so another posting of the release may be needed. It is repaired from the release's PAR2 files".

**Cause.** Three layers each added advice: the crate's yEnc error ("fetch another copy"), its
article error ("another posting may be needed"), and newsreel ("It is repaired").

**Fix.** Crate (`7f67175`): a yEnc error says what is wrong and no longer what to do, since only the
caller knows whether another copy exists. newsreel builds the line from the error's parts: what
arrived, then one conclusion, "so it is rebuilt from the release's PAR2 files", or, without PAR2
files, that another posting may be needed. Live (`1-4-engine-log-after.txt`):

```
c17883 cannot fetch bytes 930050289..930818289 of Charade.1963.BluRay.720p.H264.mp4: article <…> arrived damaged: yEnc data for "…part09.rar" decoded to 767999 bytes, but its header declares 768000, so it is rebuilt from the release's PAR2 files
c17883 repairs 1 damaged article from its PAR2 files: 2 slices of 384000 bytes, 2 recovery blocks fetched in 2.2s
c17883 repaired 1 damaged article in 0.8s once the rest of the release was in, 32.6s after it was found
```

**Tests.** The crate's `a_damaged_article_is_sent_cut_short_every_time_and_does_not_decode` and
newsreel's `a_damaged_or_missing_article_is_set_aside…` failed before; the PAR2 integration test
now checks the log line ends with the rebuild and says nothing of another copy or posting.

**Upstream.** newsreel's own.

## 15. Charade plays sound and no picture

**Bug.** Charade's top-ranked release (`Charade.1963.BluRay.720p.H264-20-40`) played sound with a
black picture in Safari (iOS 18.1 Simulator and macOS) and never decoded a frame in Chrome on the
Android emulator; nothing told the user.

**Root cause.** The file's `avcC` record has `lengthSizeMinusOne` 2, so every H.264 NAL unit is
prefixed by a 3-byte length. ISO/IEC 14496-15 allows only 1, 2 or 4 bytes (`lengthSizeMinusOne`
0, 1 or 3). ffmpeg reads it anyway (`ffprobe`: `nal_length_size=3`); the browsers that failed hand
the stream to platform decoders (MediaCodec on Android, VideoToolbox in Safari), which got no frame
out of it. Repackaged as fragmented MP4 with 4-byte lengths, the same stream decodes on the same
emulator (below). The length size is the one way the file departs from the format. Nothing else is odd: Main profile, level 4.1,
1216x720, no edit list, AAC LC; the `M4A ` major brand is cosmetic. Found with ffprobe and a box dump
(`~/.cache/nr-retest2/scripts/avcc.py`): `avcC 014d4029fee1…`, the `fe` carrying the 2.

**Fix.**
- `Media` reads `nal_length_size`; a file with another size than 1, 2 or 4 has no `direct` type, so
  the player asks the engine to repackage it. Repackaging copies the stream through
  `h264_mp4toannexb` (or `hevc_mp4toannexb`), and the MP4 muxer writes 4-byte lengths for Annex B
  input. A plain `-c:v copy` keeps the 3-byte lengths (checked with ffprobe on both outputs).
- Watchdog (`web/src/player/frozen.ts`): when the video is not paused, has a second buffered, its
  page is visible, and neither its clock nor its decoded frames move for 6 s, the player says so. A
  file the browser read directly is repackaged first (as a decode error already was); one that
  freezes repackaged moves a title to its next source, saying "This browser shows no picture of
  this source. Trying another one."; a file of a release says the picture stopped.
- Ranking: not changed. The length size is in the `moov`, which only the download reads, not the
  release's name, so it cannot be ranked by cheaply; the engine routes such files to repackaging as
  soon as it reads them.

**Live check.**
- Android emulator (`spotify-play-api29`, Chrome 101), the file read directly
  (`5-android-chrome-direct-before*.png`): not paused, readyState 4, 1216x720, clock at 0 and 0
  frames decoded after 25 s, sound playing (the speaker in the status bar). The newsreel player on
  this branch (`5-android-chrome-player-after.png`): repackaged through a Media Source, 1575 to 1648
  frames in 3 s, and a frame read back from the video (`5-…-after-decoded-frame.jpg`) is the title
  sequence. This emulator's screenshots do not capture the hardware video layer, so its screen is
  black in both; the frame counts and the read-back frame are the evidence.
- Desktop Chromium on this host through the preview tools: repackaged, 271 to 368 frames in 4 s
  (`5-desktop-chromium-player-after.png`).
- TV Bro on `august_tv` showed Charade's picture through the same path (`7-tv-start-prompt-after.png`).
- Safari was not tested again (the Mac's simulators); it takes the same repackaged stream as Chrome.
- The watchdog's live trigger was not reproduced, since the engine now repackages Charade; the
  frozen pattern it watches for is the one measured above.

**Tests.** `a_container_browsers_read_names_the_type_to_try` (a 3-byte length has no direct type),
`a_stream_copies_what_browsers_decode_and_converts_the_rest` (the bitstream filter), `frozen.test.ts`
and `recovery.test.ts`.

**Upstream.** v0.7.0 reads no NAL length size and has no watchdog. Upstream has it, for any
torrent posted the same way.

## 16. Stop on a remote loses the place

**Bug.** On Android, Chromium sends a remote's Stop to the Media Session, which had no `stop`
handler, so Chromium's own stop rewound to 0:00 and Play started over.

**Fix.** A `stop` handler leaves the player as Back does, which saves the place first. An action
a browser does not know no longer throws out of setting up the others. The page's own key
map is still right for TVs that send keydown: LG webOS and Samsung Tizen send 415 Play, 19 Pause,
413 Stop, 412 Rewind, 417 Fast Forward and 10252 Play/Pause (Tizen), which map as before, and
`MediaStop` leaves the player as this handler does.

**Test.** `system.test.ts`: the session's `stop` leaves the player and does nothing else.

**Upstream.** v0.7.0's `useMediaSession` has no `stop`. Upstream has it.

## 17. On a TV the first Play does not start the film

**Bug.** After Play on the title page, TV Bro sat paused at 0:00 behind a grey placeholder until the
centre Play was pressed (`7-tv-start-before.png`).

**Cause.** TV Bro is Android's WebView, which wants a press of its own for every video: the press on
the title page was seconds and a page earlier. The player already focused its centre Play on a TV,
but the only cue was a small symbol, under WebView's own grey play placeholder for a video without a
poster.

**Fix.** The centre Play says "Press OK to start the video" under it on a TV ("Press play to start
the video" elsewhere), and the video gets a transparent poster, so WebView draws no placeholder and
the picture behind is the film's (`7-tv-start-prompt-after.png`). Making the first press start it
would need the same video element from the title page on, which the player does not keep.

**Upstream.** Same player in v0.7.0. Upstream has it.

## 18. A play whose download was removed spins forever

**Bug.** `newsreel rm` of a play's download, then an engine restart, left the phone on "Buffering
the first seconds · 0 B/s · 0%" (`8-phone-removed-before.png`).

**Cause.** The player follows its download in the jobs feed, and falls back to the job it started
with when the feed no longer has it, so it waited on a download that was gone.

**Fix.** When the engine answers and has no download with the play's ID (`vanished` in
`lib/jobs.ts`), the player pauses and says "This download was removed: newsreel no longer has the
download this video plays from … Try again to download it anew", with Try again and Back. While a
title replaces its source, the old one going is expected and says nothing. Found on the emulator: the
broken stream's own "The video stopped" drew over it, so that one now gives way
(`8-phone-removed-after.png`).

**Test.** `lib/jobs.test.ts`; on the emulator as above.

**Upstream.** v0.7.0 falls back the same way. Upstream has it.

## 19. Chapter names that are a timestamp

**Bug.** His Girl Friday's chapters are named `00:00:00.000` and so on, so the bar read
"0:34 / 1:32:08  00:00:00.000" (`9-phone-chapter-before.png`).

**Fix.** The engine leaves out a chapter name that is only a time (digits, colons and dots, with a
colon) or `Chapter N`; the chapters themselves stay on the seek bar.

**Test.** `a_chapter_named_only_by_its_time_or_number_has_no_name` (and that "Chapter 1: The Ride"
and "1963" stay). Not shown on a device: the test home has no film with such chapters.

**Upstream.** Same in v0.7.0. Upstream has it.

## 20. The title page's sources still say `--timeout`

**Bug.** The title page's Sources list and `/api/title/sources` said "retry with a longer --timeout"
(`10-12-phone-hint-and-count-before.png`), though section 4a fixed the play screen.

**Cause.** Both `/api/search` and `/api/title/sources` stream their results and end with the error as
their last line, which `results` sent as the search gave it. Only the routes' other failures went
through `for_browser`.

**Fix.** The last line goes through `for_browser` too. The fake search port now fails a search whose
every source failed, as the real one does, so the web tests reach this path.

**Test.** `a_search_and_a_titles_sources_that_every_indexer_failed_hint_at_what_a_browser_can_do`
failed before ("check the network connection, or retry with a longer --timeout") and passes now.
On the phone, with the indexer pointed at a closed port: "All sources failed (hydra). Check that the
computer running newsreel is online, then search again." (`10-phone-sources-hint-after.png`).

**Upstream.** v0.7.0 streams the error the same way, with the `--timeout` hint. Upstream has it.

## 21. Pairing cookies collide across engines on one host

**Bug.** A browser that never paired with this engine was told it "was removed", from another
engine's cookie on the same host (`11-tv-pair-removed-before.png`); cookies ignore ports, so pairing
with one engine also replaced another's cookie.

**Fix.**
- Each engine has an ID in its database (new `engine` table), and its cookie is
  `newsreel_device_<id>`. The engine reads only its own from the `Cookie` header. The remote CLI keeps
  the name with the token, since it pulls files with that cookie.
- The engine keeps the token hashes of the devices it removed (`removed_devices`). Only one of those
  reads as "removed"; any other token is "not paired". Guessed tokens still count toward the
  per-address limit, which used to count only "removed" ones.

**Tests.** `each_engine_names_its_own_cookie_and_only_a_token_it_removed_reads_as_removed` and, over
HTTP with two engines, `a_browser_paired_with_another_engine_on_the_host_is_not_paired_here_and_can_pair_with_both`.
The existing device tests use the engine's cookie name. Not tried on a device: the emulators reach
the engine over loopback, which needs no pairing.

**Upstream.** v0.7.0 has one `torrential_device` cookie and the same "removed" for any unknown token.
Upstream has it.

## 22. "9 sources" and "Found 10 sources"

**Cause.** Two counts. The title page lists the play's line: a download here first, and a release
posted twice under the same name once (Charade's FraMeSToR remux is listed twice, `ubacff2` and
`u51c7bb`). The play counted every release the search returned.

**Fix.** The play counts the same line the page lists. Live, the page said "7 sources, in the order
Play tries them" and the play "Found 7 sources" (`12-phone-*-after.png`). An indexer that answers
differently between the page's search and the play's can still make them differ.

**Tests.** `a_play_counts_its_sources_as_the_title_page_lists_them_with_a_release_posted_twice_once`
failed before (3 against the page's 2). `replace_skips_a_release_whose_nzb_turns_out_to_be_the_stalled_download`
expected the search's 2, and now expects the page's 3: the download here, listed by its own name,
and the search's two.

**Upstream.** v0.7.0 counts the search too, but lists releases once by ID, not by name, so its
counts differ only with a download here or more than 12 results.

## 23. `update --check` before the first release

**Bug.** "returned 404 Not Found without a release version … run newsreel update again later".

**Fix.** GitHub answers `/releases/latest` with 404 until a repository has a release. That is now
"no release": `update --check` prints `latest: none published` and "no release of newsreel is
published yet, so there is nothing to update" (JSON `"latest": null`), `update` prints "No release
of newsreel is published yet, so there is nothing to install; 0.1.0 stays." and exits 0, and the
engine's daily check no longer logs a failure. Once a release exists `/latest` redirects, as
before.

**Test.** `with_no_release_published_update_says_so_and_that_there_is_nothing_to_do` failed before
(exit 5 with the old message) and passes now.

**Upstream.** Same check in v0.7.0. Upstream has it.

## 24. Minor

- **Pair page error.** The "six digits" error stayed after the sixth digit. It clears once the code
  is whole. `pair.test.ts`. Upstream has it (v0.7.0 shows the same error and clears it only on the
  next submit).
- **`engine status` and `engine run`.** Status named `state/engine.log`, which an engine run in the
  foreground never writes. The engine notes at start whether its stderr is that file (by file
  identity: device and inode on Unix, volume and file index on Windows; a first version asked
  Windows only whether stderr was a console, which Windows CI showed misses a file sent elsewhere), and status then says "this engine logs
  where newsreel engine run was started, not to …/engine.log" and gives no `log`.
  `engine_status_of_an_engine_run_in_the_foreground_says_where_it_logs_not_engine_log` failed
  before. Upstream has it.
- **`verify` then `wait`.** A wait that saw its download checked now ends with "c17883 was checked:
  every piece of its selected files is intact"; damage ends it with the verification error, as
  before. Only a check of data already there counts, and downloading after it undoes it: a new
  download is checked too, before it has anything (macOS CI caught that). `a_wait_that_saw_its_download_checked_says_the_check_found_it_intact`. A wait started
  after the check ended cannot know there was one. Upstream has it.
- **`indexers add` of an indexer already there.** It matched entries by exact URL, so the base
  address the retest had in config.yaml and the `/api` address `add` found made a second entry with
  the key in plain text; and a match replaced a key variable and renamed silently. Now an entry
  written without `/api` matches too and takes the `/api` address, a key its variable already holds
  keeps the variable, and the reply says what changed ("updated hydra: renamed from hydrabase, and
  its key is now written in config.yaml, in place of $HYDRA_KEY", or "unchanged"). `added` lists
  new entries only. `adding_an_indexer_already_there_keeps_its_name_and_key_variable_and_says_what_changed`
  failed before; the existing test's "adding it again changes nothing" now checks `updated`.
  Upstream replaces the key and renames the same way, without the `/api` match, which is newsreel's.
- **Title heading blank between the name and the logo** (`14e-tv-title-blank-before.png`). Section
  7f's name went the moment the logo was measured, while the logo took 300 ms to fade in. The name
  now fades out over the logo on the same 300 ms. Layout only, no test. Upstream showed no name at
  all until the logo (section 7f), so this gap is newsreel's.


# Third retest

Fixes for what a Safari and iPhone check of the second retest found. One commit each. The scripts,
traces and run logs are in `~/.cache/nr-retest3/` (`scripts/`, `runs/<run>/engine.out`). The
`usenet` crate's part is `ac04b12` on `par2-priority`, which newsreel now pins.

Findings 2 and 3 were measured on this host through a relay shaped like the Mac's link
(`scripts/shaper.py`): 25 MiB/s in all, 0.5 MiB/s on each connection, 40 ms each way, with the four
real servers (105 connections in the first tier) and TLS end to end, in a network namespace of the
test engine's own. Each run is a fresh home playing the real NZB. The engine logged a line for every
NNTP request (its priority, how long it waited for a connection, how long the reply took, and how
many requests were ahead of it on that connection) and every HTTP read; that tracing was not
committed.

## 25. The start screen hides a repair

**Bug.** A resume just before Charade's damaged article said "Getting ready to play · Buffering the
first seconds." for about 80 s while status said `repair: blocking, eta 19 → 14 → 8 → 1`.

**Cause.** Only the player's buffering overlay read `progress.repair` (section 11). The start
screens' steps (`READING` in `Chrome.tsx`, the release player's `Starting` and the title player's
`TitleStarting`) always said their own words.

**Fix.** `waitingOn` puts the repair's words in place of the running step's detail while a read
waits on a repair (`blockingRepair`: the download is running and `repair.blocking`): "Repairing a
damaged part of this video · about 19s left". Both start screens follow the download that plays,
also once a source plays and buffers its first seconds (`BufferingStart`, `TitleBuffering`), and
the overlay uses the same test.

**Test.** `Chrome.test.ts`: the running step says the repair and its time left; a repair no read
waits on, a paused download, or no download leave the steps as they are. Not shown on a device: the
phone emulator's serial was held by another agent's session, which was left alone.

**Upstream.** No PAR2 upstream: newsreel's own.

## 26. Fetching the recovery blocks is still slow on a Mac-like link

**Bug.** The Mac logged the recovery blocks fetched in 21.6 to 69.4 s, 33.3 and 36.0 s of them while
a reader waited; 2.2 s on this host uncapped.

**Reproduced.** Through the shaped relay, a reader asking for the damaged bytes at once: 33.6 s, then
12.0 s on a second run (`runs/before-repair-1`, `before-repair-0b`).

**Cause.** The traces rule out TLS (connections were open long before; opening one took about 1 s,
at the start only) and the tiers (every repair request went to the first tier). Two things:
- **The first fetch went at background priority.** A repair starts the moment its piece is set
  aside, and its first fetch, the PAR2 index, picks its priority then. The reader's own urgent
  fetch had found the damage, but the reader marks itself as waiting on the repair only on its next
  poll, so the index joined the download's queue: 13.1 s and 5.3 s for one article, while the
  download's own requests waited 7 to 13 s for the background limit (128 in flight).
- **Urgent requests stacked on a few connections.** An urgent request never queues behind a
  background one (section 11), so with the background limit (128) past the connections (105),
  nearly every connection carried background requests and the repair's rounds of 7 and 25 urgent
  articles went up to 4 deep on the few that did not. A connection sends its replies in turn, at
  0.5 MiB/s here, so those rounds took 18.5 s; single urgent articles took 1.7 s, the 4th in line
  up to 8.9 s.
- The rest is the repair's rounds, each waiting on the one before: the index (1 article), the
  volumes' first articles (7), their headers (25), the recovery volume (2), all about 768 KB. On
  this link no article comes in under about 1.7 s.

**Fix.**
- Crate (`ac04b12`): an urgent request does not queue behind another urgent one while a connection
  carrying background requests drains. Background requests wait while an urgent one does, so those
  connections free up in a fraction of an article each, and a burst spreads over them. Test:
  `urgent_requests_spread_over_connections_that_free_up_rather_than_stack_on_one` (6 urgent
  requests, one free connection out of 6, 0.25 s an article): 1.20 s before, 0.58 s now.
- newsreel: a piece set aside by a reader's own fetch, or while one is under way, marks the reader as
  waiting then (`settle`), so the repair's first fetch is urgent. Test:
  `a_piece_set_aside_that_a_reader_rushed_is_one_the_reader_waits_on`, which failed before.
- A repair fetches 32 articles at once, not 16, so a film's 25 header articles go in one round.

**Live check** (seconds to "recovery blocks fetched", same relay and NZB):

| | before | after |
|---|---|---|
| Reader asks for the damaged bytes at once | 33.6, 12.0 | 5.3, 5.5 |
| Of which: the PAR2 index | 13.1, 5.3 | 0.3, 0.3 |
| Reader asks 30 s into the download (limit 72 of 105, idle connections left) | 10.2 | 8.2 |
| No shaping | 3.8 | 0.8 |

With the pool fix and the urgent start alone, 16 at once, the first case took 7.0 s. Every read
returned all 868,001 bytes. The Mac was not tested again.

**Upstream.** libtorrent has no PAR2 and the pool is newsreel's: newsreel's own.

## 27. Resuming from a saved place starts slowly

**Bug.** From 0:00 the player started in 15 to 20 s, from 1:19 and about 1:13 in 52 to 59 s.

**What a resume reads.** The same as from 0:00 up to the stream: the play, then `/api/media`, whose
ffprobe reads the `moov`, which Charade keeps at the front (2.7 MB, 4 articles). Then ffmpeg seeks
(`-ss`), and the pieces it reads there are fetched urgently, one article each round. Neither of
those differs much. The traces show what does: from 0:00 ffmpeg made 6 HTTP reads before its third
fragment; from 1:19:00, 549, one every 20 ms.

**Cause.** Two things together.
- Charade's audio is stored ahead of its video, further the later the film: 16 MB apart at 20 min,
  53 MB at 1:19 (ffprobe of the packets' positions). ffmpeg reads in time order, so after a seek it
  hops between the two places, a few hundred bytes of audio, then a frame of video, and each hop is
  a new HTTP read. From 0:00 the two are close and one read serves both.
- Each read of a download still running asks the engine thread twice (`want_range`), and the thread
  slept up to 10 ms waiting for the Usenet engine's events between requests: the Usenet engine
  could not wake it, so it was polled. On a finished download the reads skip the engine: the same
  stream from 1:19:00 made its hops and had its third fragment in 0.45 s.

**Fix.** The Usenet engine implements `notify`: a download's tasks wake the engine thread when they
send an event, so the thread sleeps until a request or an event comes and a request waits for
nothing. Test: `the_engine_thread_answers_requests_at_once_and_not_after_waiting_for_events`, 50
asks in turn: 9.9 ms an ask before, under 3 ms now.

**Live check** (seconds from the play's start, same relay and NZB, two runs each):

| | from 0:00 | from 1:19:00 |
|---|---|---|
| Play answered (the NZB resolved) | 20.5 to 20.8 | 20.5 to 20.8 |
| Media read | 24.0 to 24.3 | 24.0 to 24.3 |
| First fragment, before | 24.3, 24.3 | 33.4, 33.6 |
| First fragment, after | 24.2, 24.1 | 28.3, 28.6 |
| Third fragment, before | 24.4, 24.3 | 38.9, 39.1 |
| Third fragment, after | 25.8, 24.1 | 30.1, 32.0 |

The hops now run at about 300 a second. What is left from a saved place is two article rounds in
turn: ffmpeg finds where the video is only once the audio's bytes have come (1.7 and 1.9 s on this
link). Fetching both at once would need the engine to read the file's sample tables. The Mac's
longer gap (about 37 s against about 10 s here) was not measured again.

**Upstream.** v0.7.0's libtorrent engine implements `notify`, so the wait is newsreel's own.

## 28. A picked source said "Found 9 sources" and "Playing the best source"

**Fix.** A play of a source picked from the title's list says "Finding the source you picked",
"Found the source you picked", then "Trying" and "Playing the source you picked", and counts no
others. The engine still looks the release up among the title's search results, which the step's
detail says ("Looking it up among this title's releases.").

**Test.** `TitleScreens.test.ts`: a picked source's steps, and a play the engine chooses for as
before. The CLI's `newsreel play <title> --source` still printed "Found 9 sources" and "Trying the
best source"; section 31 fixes it.

**Upstream.** v0.7.0 says "Found 9 sources" and "Playing source 1 of 1" for a picked source.
Upstream has it.

## 29. Stop on a TV Bro remote still loses the place

**Bug.** Section 16's Media Session `stop` handler never runs in TV Bro: its WebView has no
`navigator.mediaSession`. On a remote's Stop the film rewound to 0:00 and the player stayed open, so
the next Play started the film over and saved 0:00 as the place.

**What reaches the page.** A probe page on `august_tv` in TV Bro (WebView, Chrome 157 UA,
`mediaSession=false`), with the Stop key (`android-keycode:86`) sent by agent-device
(`~/.cache/nr-retest4/probe/probe.html`):
- Playing at 8.6 s: `pause` with the time already 0 and `seeking` true, then `seeking`, `timeupdate`
  and `seeked`, all at 0.
- Paused at 8.2 s: only `seeking`, `timeupdate` and `seeked` at 0.
- No `keydown`, `keyup` or `keypress` (no `MediaStop`, 178 or 86), no `visibilitychange`, no
  `pagehide`, `emptied` or `abort`. The next Play key played from 0:00.

So the one sign is a seek to 0:00 that the page did not make, with the video paused.

**Fix.** `useBrowserStop` (`system.ts`) watches `seeking`: in a browser without a Media Session, a
seek to exactly 0 while paused, once the video started, that the player did not ask for, leaves the
player as Back does. `PlayerActions` marks its own seeks until they land (`seeked`), so Home, Replay,
the seek bar's start and a chapter at 0:00 are not taken for Stop.

**Why it is right.**
- The place is kept by what already happens: the `pause` handler saves the time of the last
  `timeupdate` (8.6 s here, not 0), and the rewind is no play, so `Unsaved` saves nothing more
  (section 2's rule: only played time on a page in view, or a start, is saved).
- It cannot fire in Chrome or Safari: both have a Media Session, whose `stop` handler (section 16)
  gets the key and suppresses the browser's own rewind. LG and Samsung TVs send keydown 413, which
  the key map leaves on. The other seeks of the page's own (`source.ts`) never land on exactly 0:
  they start a file at a time over 0, or step 0.01 s onto data.
- WebKit's "time zero" while a Media Source buffer empties or before metadata (section 15's
  `RemuxSource` comments) happens only under a seek the player asked for, or before the video
  started, and only in Safari, which has a Media Session.

**Tests.** `system.test.ts`: WebView's stop leaves the player; a seek to 0:00 the player asked
for, one before the start, one in a browser with a Media Session, one to 0.01 s, and one while
playing do not. Both failed to load before (no `stoppedByBrowser`).

**On the emulator.** `august_tv`, TV Bro, this branch's engine with Charade 20-40 from Usenet
(screenshots in `~/.cache/nr-retest4/shots/`):
- Playing at about 0:50 (`29-playing-before-stop.png`), Stop left the player for the title page,
  whose button read "Resume, 1h 53m left" (`29-after-stop.png`).
- Resume played on from there: paused about 27 s later, the player read 1:19 (`29-resumed-time.png`).
- Stop while paused at 1:19 left the player too, and the title page read "Resume, 1h 52m left"
  (`29-after-paused-stop.png`).

The engine was reached through `adb reverse`, since it refuses the emulator's `10.0.2.2` host name.

**Upstream.** v0.7.0 has no Media Session `stop` and the same `pause` save. In TV Bro, Stop rewinds
the same way and Play starts over. Upstream has it.

## 30. The frozen-picture watchdog switched a source while the video waited for data

**Bug.** Once, with a second tab playing the same film, the watchdog of section 15 moved Charade to
its next source ("This browser shows no picture of this source"). Two clean retakes did not.

**Cases checked.** In headless Chromium 159 over CDP, a Media Source page fed from a throttled
server runs the watchdog exactly as `frozen.ts` and `Playback.tsx` had it at ccf8e5a, and logs what
it sees each second (`~/.cache/nr-retest4/probe/mse.html`, `scripts/lab.ts`, runs in `runs/`):

| Case | What the watchdog saw | Fired |
|---|---|---|
| A second tab opened over the first, 25 s, then back | The hidden tab paused (a muted video) and is skipped while hidden anyway; back in view its clock moved at once | no |
| Main thread CPU throttled 40x for 25 s | Clock and frames moved every second; the media pipeline has its own threads | no |
| Feed at 0.6x, 0.5x and 0.4x the bitrate | Waits of 3 to 4 s, readyState 2, 2.1 s buffered | no |
| Feed at 0.25x the bitrate (two plays sharing the link) | Waits of 8 s, readyState 2, 2.1 s buffered, clock and frames still | yes, 3 times in 75 s |

**Cause.** The watchdog took 1 s buffered ahead of the playhead for "has data to play". Chrome
does not resume on 1 s: fed slower than it plays, it holds the buffered fragment (2 s here) and
waits, readyState 2 (`HAVE_CURRENT_DATA`), until the next arrives. While it waits, the clock and the
decoded frames stand still, which is the pattern the watchdog looks for. Two plays of one film share
the download and the link, so each stream can fall well below its bitrate. A repair wait looks the
same from the element: no data comes until it ends. Background tabs and throttling did not trip it,
since a hidden page is skipped and the pipeline's clock is not on the throttled thread.

**Fix.** A look counts only when the element does not wait for data (readyState at least
`HAVE_FUTURE_DATA`) and no read of the download waits on a repair (`blockingRepair`, section 25), on
top of what it already needed: playing, not seeking, in a page in view, 1 s buffered, and neither
the clock nor the decoded frames moving for 6 s. The frozen case of section 15 still counts: Chrome
on Android read Charade directly with readyState 4.

**Tests.** `frozen.test.ts`: a video that waits for data with 2.12 s buffered for 8 s (the lab's
numbers), and one whose download waits on a repair, are not frozen. Both failed before. In the lab
the same 0.25x run fired the old rule 3 times and the new one never
(`runs/watchdog-quarter-feed-after.txt`).

**Not reproduced.** The original incident's own numbers were not logged, so this is the case the
lab found that fits it (a second play of the same film), not a replay of it.

**Upstream.** v0.7.0 has no watchdog: newsreel's own.

## 31. The CLI said "Found 9 sources" for a picked source

**Bug.** `newsreel play <title> --source <id>` printed "Found 9 sources" and "Trying the best
source", as section 28's web player did before.

**Fix.** The CLI's lines follow the web's (2dd1e62): "Finding the source you picked among the
releases of Charade (1963)", "Found the source you picked", "Trying the source you picked: …", and
no count. The play line names the release as before. The same goes for `newsreel get <title>
--source` and `newsreel tv play <title> --source`, which print the same steps.

**Test.** `a_play_of_a_picked_source_names_it_as_the_one_picked_and_counts_no_others` in
`client/tests.rs` failed on the old lines and passes now.

**Upstream.** v0.7.0 prints "Found 9 sources" and "Trying 1 of 1" for a picked source. Upstream has
it.
