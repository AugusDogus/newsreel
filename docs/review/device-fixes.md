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
