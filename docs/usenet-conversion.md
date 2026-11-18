# Converting torrential to Usenet

newsreel is a fork of nexxeln/torrential that replaces BitTorrent with Usenet: search Newznab
indexers, then stream or download from NNTP servers. The goal is feature parity where it makes
sense.

## Approach

torrential already isolates the network behind two ports in its engine crate (`crates/daemon`), so most of the program stays:

- `daemon/engine_port.rs` (`EnginePort`): libtorrent upstream, `daemon/usenet.rs` here. It
  covers add, pause, resume, recheck, `want_range`, `written`, `stream`, and events.
- `daemon/search_port.rs` (`SearchPort`): torrent indexes upstream. Newznab is the same
  RSS API as Torznab, except the link is an `.nzb`.

The daemon stores each job's parsed `.torrent` bytes and hands them back to the engine
on every resume. The Usenet equivalent is a **manifest**: the NZB plus the resolved
layout of the playable files. The `NzbPort` (`daemon/nzb_port.rs`) resolves an NZB from a
search result, a file, or a URL over NNTP (first article of each file, RAR headers), so
`parse` stays synchronous and the owner, store, CLI, TUI, and web app need few changes.

NNTP, yEnc, NZB, RAR, and manifests live in the [`usenet`](https://crates.io/crates/usenet)
crate, which started here and is now published on its own from
[AugusDogus/usenet-rs](https://github.com/AugusDogus/usenet-rs).

A manifest maps every article to a byte range of an output file:

- A video posted directly: the output file is the posted file; each segment is a piece.
- A video stored (uncompressed) in RAR volumes: the output file is the inner video. Each
  segment covers part of one volume; the part inside the stored data maps to a range of
  the video. Nothing is extracted; the engine writes video bytes straight to disk.

Pieces are segments (about 700 KB), verified by their yEnc CRC32. `want_range` fetches
the segments under a byte range first, which is what streaming and seeking need.

## Feature map

| torrential | newsreel |
|---|---|
| Torrent indexes, lawful catalogs | Newznab indexers (NZBHydra, Prowlarr, direct) |
| Magnet, infohash, `.torrent` URL or file | NZB URL or file |
| Peers, seeds, trackers, DHT | NNTP servers by priority, connections |
| Seeders in ranking | Grabs |
| Piece hashes | yEnc CRC32 per article |
| Never seeds | Download-only by nature |
| Web seeds and archive.org mirrors | Dropped |
| Stream, seek, subtitles, ffmpeg remux, TUI, web, agent skill | Kept |
| Films and series by title (Cinemeta), play the first source that starts | Kept; sources are Newznab results |
| Swarm strength in a title's play order | Dropped; grabs break ties |
| Metadata probes of the next sources | NZBs of the next two sources fetched early |
| Hold other downloads back while a film streams | Per-download rate limit in the NNTP engine |
| Phones and TVs, Play on TV (DLNA), plays folder, Trash, ffmpeg fetch, login service | Kept |
| Windows: logon task, Recycle Bin, `install.ps1`, self-update | Kept |
| Ranking by the file a play opens (archive.org file lists) | Dropped; codecs come from the release name |
| Updates from GitHub releases, install script | Kept; from AugusDogus/newsreel |
| Compressed or encrypted RARs | Refused; a play moves to the next release |
| `indexers` list, add, and remove Torznab feeds, Prowlarr's and Jackett's indexers | Newznab APIs and Prowlarr's Usenet indexers; Jackett proxies only trackers |
| `server install`, Docker image, `connect`, `pull`, `public_address`, Tailscale pairing links | Kept; the image is `ghcr.io/augusdogus/newsreel` |
| Torrent traffic bound to one network interface, such as a VPN's | Dropped |

## Status

libtorrent, magnets, and the torrent search sources are gone; the binary links no C++ and
builds with Rust, a C compiler, and bun. `search`, `files`, `get`, `play`, `status`, `wait`,
`pause`, `resume`, `verify`, and `verify --repair` work against real servers, for search
results, local `.nzb` files, and NZB URLs. Direct posts, RAR4, and RAR5 stream with seeking.
Compressed, encrypted, expired, and incomplete releases are refused with a reason.

- **Servers.** Requests go to the best priority tier with a usable server; a busy tier is
  waited for, so backup servers only see articles the tiers above lack (or requests while
  those refuse connections).
- **Reuse.** Each landed piece's CRC32 is journaled under the state directory. A release added
  again after a crash or `rm` reads those pieces back from disk and fetches only what no
  longer matches.
- **IDs.** A search result is `u` and the SHA-1 of its NZB link (`ResultId`); a download is a
  SHA-1 of its NZB's segments (`DownloadId`), not of the whole document, because indexers vary
  the metadata, posters, dates, and groups on every fetch. The engine records which result
  became which download, so either works afterwards.
- **Ranking.** Newznab `grabs` stand where seeders stood: the table, `--sort grabs`, `--min-grabs`.
  Where upstream favoured official sources, TRaSH Guides' Radarr and Sonarr custom formats
  (release group and title conditions only, bundled by `scripts/trash/update.py`) rank above
  grabs: a release on a list to avoid gets the `avoid` flag and ranks below the rest, and a
  group in a tier ranks above unranked ones. Names with a season or episode use Sonarr's lists.
- **Titles.** A title's search asks every indexer each of its queries (name and year, then the
  name), and for a film, the releases tagged with its IMDb ID too, up to 300. A release tagged as
  the film fits under another name (a dub's, the original title, a misspelling) when its year
  fits, and one tagged as another title never fits. Episodes are tagged with TVDB IDs, not IMDb's,
  so a series is searched by words alone. A play waits for the quick indexers (2.5 s) or for all of them, tries the best source,
  and moves past one whose NZB does not arrive, holds no video, or gets no data. While one source
  is tried, the next two sources' NZBs are fetched and kept, so stalled indexers in a row cost
  one wait. A download a title play made is held as the search result it came from.
- **Holding back.** `EnginePort::limit` caps one download's bytes per second; the NNTP engine
  paces its article fetches against the cap and lets go at once when it is lifted.
- **Releases.** `install.sh`, `install.ps1`, the updater, and the release workflow use
  AugusDogus/newsreel. The builds need no C++, since libtorrent is gone, so Windows needs no vcpkg.
  Upstream's bridge that updates torrential 0.5 to 0.6 is left out: no newsreel ever spoke 0.5's protocol.
- **No compatibility.** Nothing reads BitTorrent input or old state: no magnet, infohash, or
  `.torrent` handling, no `torznab` config, no metadata phase, and one fresh database schema.
  Types, JSON fields, and error codes use Usenet terms (`connections`, `nzb_url`, `invalid_release`).
- **Tests.** `crates/newsreel/tests/usenet.rs` and `search.rs` run the binary against a
  loopback NNTP server (`usenet::fake`) and Newznab feeds; the fake engine behind the
  CLI tests serves fake releases the same way. In the engine's tests a release the fake network
  publishes is an NZB its indexer serves, and one it does not never arrives.
- **Indexers.** `newsreel indexers add` asks an address for Newznab `t=caps` and writes it to
  `sources.newznab`; Prowlarr's own address adds each of its enabled Usenet indexers. A key may sit
  in config.yaml as `api_key`, and a server's password as `password`, beside `api_key_env` and
  `password_env`, because the engine the login service or `server install` starts sees only
  `NEWSREEL_HOME`. A config.yaml newsreel writes a key or password into is readable by its owner
  alone.
- **Login service.** When launchctl, `systemctl --user`, or schtasks is missing or refuses, as under
  su, sudo, or ssh without a login session, in a container, or under WSL, `engine install` fails with
  `service_unavailable`, says why and what to do, and says whether the engine it stopped runs again.
- **Upstream.** Ported from nexxeln/torrential v0.7.0 by replaying the fork's commits onto it,
  its first conversion commits as one. Upstream's version number is not taken.
- **Name.** The app, its crates, paths, and environment variables are `newsreel`, not `torrential`,
  and its versions start again at 0.1.0.

Left out of upstream 0.7.0, for having no Usenet counterpart:

- `torrent_interface`, which binds libtorrent's peer, tracker, and DHT sockets to one interface so a
  swarm sees only a VPN's address, and the title play that fails at once while it is down. NNTP goes
  over TLS to the account's own provider, and no peer sees the address.
- libtorrent's listen port where TCP and UDP both bind, and its Windows UDP port test: newsreel
  listens for no peers.
- libtorrent writing through the system cache on Windows. The CI's timing of `engine stop` came along.
- A file list that comes while its folder hangs: newsreel has no metadata phase. A release's layout
  is known before its download starts, and every look in its folder already goes through `Reach`.
- Adding Jackett's indexers: Jackett proxies only torrent trackers.

Not done:

- Resolving a new NZB waits for a free connection behind running downloads, which on real servers is
  about one article long.

## Phases

1. **Tracer bullet.** A `usenet` crate (NNTP, yEnc, NZB, RAR, manifest). A Newznab source.
   Fetching an NZB resolves a manifest. A Usenet `EnginePort` writes segments to disk.
   `newsreel search` then `newsreel play` streams a stored-RAR release.
2. **Parity.** Wording and fields across CLI, TUI, and web (servers, not peers). Resume and
   recheck. Availability check before starting, fallback to the next result, retention
   and password signals in ranking, the remembered verdicts from the prototype.
3. **Removal.** Delete libtorrent, magnets, torrent sources, and the C++ toolchain from the
   Nix shell. Rename torrent types (`TorrentId`, `InfoHash`). Update README, skill, install.
