<div align="center">

<img src="octo/Assets/octo_logo.png" alt="Octo — self-hosted music discovery for Navidrome" width="280" />

# Octo

**Self-hosted music discovery for Navidrome.**
Search and stream songs you don't own. Heart what you like — Octo grabs the FLAC and adds it to your library forever.

[![License: GPL v3](https://img.shields.io/badge/License-GPL_v3-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)
[![.NET 9](https://img.shields.io/badge/.NET-9.0-512BD4)](https://dotnet.microsoft.com/)
[![Docker Compose](https://img.shields.io/badge/docker-compose-2496ED)](https://docs.docker.com/compose/)
[![CI](https://github.com/winters27/octo/actions/workflows/ci.yml/badge.svg)](https://github.com/winters27/octo/actions/workflows/ci.yml)

</div>

---

## Who is this for

If you self-host your music with Navidrome (or any Subsonic-compatible server), you've already opted out of streaming-service lock-in. The downside: your library is only as interesting as the music you've already collected. Searching for something new just gets you "no results."

Octo is for people who want both — your music on your hardware, plus a working discovery engine that lets you **preview anything for free and keep only what you decide you want.**

Built for:

- **Self-hosters** running [Navidrome](https://www.navidrome.org/) who miss Spotify-style discovery.
- **Music nerds** who want full FLAC quality, not 320kbps streaming.
- **Subsonic app users** (Feishin, Arpeggi, Narjo) who want their existing apps to suddenly be smarter.
- **People canceling Spotify / Apple Music / Tidal** who need a real replacement, not "well, I'll just listen to less music."
- **Plexamp / Roon refugees** who like the discovery features but don't want the proprietary stack.

> **Note:** if you already pay for Qobuz, Deezer, or Yandex Music and want a Subsonic frontend that ingests your paid catalog into your library, [V1ck3s/octo-fiesta](https://github.com/V1ck3s/octo-fiesta) is closer to what you want — it downloads from those APIs directly. Octo is the *no-paid-streaming-required* path: previews come from YouTube, downloads come from Soulseek.

## What it does

- **Search finds music you don't own.** Tap a result to hear it instantly via YouTube preview.
- **Radio works on every song.** Owned tracks play at full FLAC; missing ones preview from YouTube.
- **Heart to keep.** Star a previewed song and Octo grabs the FLAC from Soulseek, adds it to your library, and tells Navidrome to rescan. Within a minute, the song is yours forever.
- **Search and heart whole albums.** Albums you don't own show up in search with real cover art and tracklists. Star one and Octo fetches every track. Downloads run one at a time, so a full album takes a while — album-heart sources can be disabled independently in the admin UI.

Plug Octo in front of your Navidrome. Point your Subsonic apps (Feishin, Arpeggi, Narjo, etc.) at Octo instead. Nothing else changes.

## Get started

Octo sits **in front of** your existing Navidrome. Your Subsonic app talks to Octo; Octo adds discovery and previews, then proxies everything else through to Navidrome:

```
   Subsonic app          Octo               Navidrome
  (Feishin, Arpeggi) ──▶  :5274  ──────────▶  (your library)
                           ├─▶ yt-dlp shim   (instant previews)
                           └─▶ slskd         (downloads on star)
```

So setup is two steps: **tell Octo where Navidrome is**, and **point your app at Octo**.

**Required**

- A box with [Docker](https://docs.docker.com/engine/install/) installed.
- An existing [Navidrome](https://www.navidrome.org/) server, reachable from the Octo host by LAN IP or service name (not `localhost`).

**Optional** — Octo runs fine without these:

- A free [Last.fm API key](https://www.last.fm/api/account/create) — enables radio / discovery.
- A free [Soulseek account](https://www.slsknet.org/news/node/1) — enables lossless FLAC downloads when you star a song.
- An existing Lidarr server — an alternative heart source when it already has indexers and a download client configured.

Then:

```bash
git clone https://github.com/winters27/octo.git
cd octo
./install.sh
```

The installer asks for your Navidrome URL (and, optionally, Last.fm and Soulseek), brings the stack up, and prints the address.

**When it's done:**

- Point your Subsonic apps at `http://<your-host>:5274` — **not** Navidrome's own address.
- Open the admin dashboard at **`http://<your-host>:5274/admin`** to manage every setting from the browser — no editing config files by hand.
- If a client reports the server is unreachable, that is Octo telling you setup is not finished: its ping response spells out exactly what to fix (usually the Navidrome URL).

## Compatible apps

| Works | App | Platform |
|---|---|---|
| ✅ | [Feishin](https://github.com/jeffvli/feishin) | desktop |
| ✅ | [Supersonic](https://github.com/dweymouth/supersonic) | desktop |
| ✅ | [Sublime Music](https://github.com/sublime-music/sublime-music) | Linux |
| ✅ | [Arpeggi](https://www.reddit.com/r/arpeggiApp/) | iOS |
| ✅ | [Narjo](https://www.reddit.com/r/NarjoApp/) | iOS |
| ✅ | [Amperfy](https://github.com/BLeeEZ/amperfy) | iOS |
| ✅ | [DSub](https://github.com/daneren2005/Subsonic) | Android |
| ✅ | [Ultrasonic](https://gitlab.com/ultrasonic/ultrasonic) | Android |
| ✅ | [Tempo](https://github.com/CappielloAntonio/tempo) | Android |
| ✅ | [Audinaut](https://github.com/nvllsvm/Audinaut) | Android |
| ✅ | [SubTracks](https://github.com/austinried/subtracks) | Android / iOS |
| ✅ | Tempus | Android |
| ✅ | most other Subsonic apps | |
| ❌ | Symfonium | searches its own offline copy, so it never asks the server |

Symfonium is the one that genuinely cannot work. It syncs your library to the device and
searches locally, so a search never reaches Octo and there is nothing to add results to.

**Both search generations are supported.** Subsonic has two search endpoints, `search2` and
`search3`, and Octo answers either. This matters more than it sounds: DSub and Ultrasonic
choose between them based on whether *you* browse by tags or by folders, not on the server
version, so a folder-browsing user talks `search2`. Both formats are supported too — some
clients speak JSON, some (DSub) only XML.

## Updating

```bash
git pull && ./install.sh
```

Re-running the installer keeps your existing answers.

Octo builds from source, so `git pull` is what actually updates it. `docker compose pull` only refreshes slskd.

### Which version am I on

Releases are dated, so `2026.07.29` is the release cut on that day. Your running version is shown under **About** in the admin dashboard, and it is the single most useful thing to include in a bug report.

To pin to a release instead of tracking `main`:

```bash
git checkout 2026.07.29 && ./install.sh
```

Prebuilt multi-arch images are also published to `ghcr.io/winters27/octo`, tagged `latest`, the release date, and the commit sha.

## Admin dashboard

`http://<your-host>:5274/admin`

Every setting has a form, every backing service has a live status indicator, and the **Raw Config** tab lets you edit the whole effective configuration as a JSON file if you'd rather work that way. Changes hot-reload — no rebuild, no restart for most settings.

## Notifications

Optional push notifications for the download lifecycle, because Subsonic has no way to
tell you a starred track landed — or quietly settled for a lossy copy.

- **Two transports, either works alone**: [ntfy](https://ntfy.sh/) (paste a topic URL,
  subscribe to the same topic in the ntfy app) and Discord webhooks (rich embed with
  album art). A transport is on when its URL is set.
- **Five events, each with its own toggle** in the dashboard's Notifications tab:
  download started (did it find lossless, or is it settling?), download completed,
  lossless fallback, download failed, and one summary per album instead of a ping per
  track.
- A **Send test** button verifies your URLs and tokens without waiting for a real
  download, and reports each transport's outcome separately.

---

## Frequently asked questions

### Is Octo a self-hosted Spotify alternative?

It's the discovery half. Octo doesn't replace your music *server* — that's still Navidrome — but it adds the search-and-listen-to-anything experience that streaming services do well. With Octo plugged in, your Subsonic app behaves more like Spotify or Apple Music: search returns recommendations, radio works on any song, and you can preview tracks you don't own. The difference is that "I want to keep this" downloads it as a real FLAC into your library, instead of renting it.

### Does this work with Plex / Plexamp?

No. Octo speaks the Subsonic API, not the Plex API. If you're a Plex user looking for self-hosted alternatives with discovery, the move is Navidrome + Octo + a Subsonic client like Feishin or Arpeggi.

### How is this different from Navidrome's built-in radio?

Navidrome's radio plays songs from your existing library. Octo's radio reaches *outside* your library — Last.fm finds similar tracks, YouTube provides the preview, and Soulseek provides the keep-it-forever path. Navidrome alone gives you a great library player; Octo turns that library into a launchpad for discovery.

### Is my data going anywhere?

No. Octo runs entirely on your hardware. It calls Last.fm (for similar-tracks data), YouTube via yt-dlp (for audio previews), and Soulseek peers (for downloads). Those are outbound queries — nothing about your library or listening history is shipped anywhere.

### Do downloaded songs get tagged correctly?

Yes. Soulseek peers share full FLAC files with their existing ID3 tags intact. Octo organizes them per your `FolderStructure` setting (`Flat` or `Organized`), then triggers a Navidrome rescan so they appear in your library exactly like everything else you own.

### What if I don't want to use Soulseek?

You can use YouTube or an existing Lidarr server, or disable automatic acquisition entirely.

Use **Downloads → Heart download priority** in the admin UI to order Soulseek, YouTube, and Lidarr and independently choose whether each handles song hearts, album hearts, or both. Octo tries eligible sources from top to bottom and stops at the first success. `DOWNLOAD_SOURCE`, `DOWNLOAD_ON_STAR`, and `DOWNLOAD_ALBUM_ON_STAR` remain migration defaults for existing and env-only installations.

Lidarr works at album level, so enabling it for song hearts still fetches the song's full album. It is last and disabled by default; configure its URL, API key, root folder, and profiles on the Lidarr page, then enable the heart types you want in the priority list.

To stop downloading altogether, turn off both heart types for every source. On an env-only installation, set `Subsonic__DownloadOnStar=false` and `Subsonic__DownloadAlbumOnStar=false`. Hearts still register as favorites without acquiring files.

### Can it run on a Raspberry Pi?

Yes — multi-arch images are published for amd64 and arm64. The yt-dlp sidecar does most of the CPU work; a Pi 4 or Pi 5 handles a single household's listening fine.

### Why is Octo a refactor of [octo-radiostarr](https://github.com/winters27/octo-radiostarr)?

The earlier project leaned on SquidWTF (a public TIDAL proxy) for streaming. In April 2026 Tidal hardened their API and broke every TIDAL proxy at once. Rather than patch around it, Octo was rebuilt on two sources that don't depend on a single fragile vendor API — YouTube via yt-dlp, and Soulseek via slskd. The old repo is archived; new development happens here.

### How is Octo different from [octo-fiesta](https://github.com/V1ck3s/octo-fiesta)?

Octo's earliest commits descended from [V1ck3s/octo-fiesta](https://github.com/V1ck3s/octo-fiesta) (via [bransoned/octo-fiestarr](https://github.com/bransoned/octo-fiestarr)), so the *concept* is the same: a Subsonic proxy that fills in songs you don't own. The implementation has diverged completely:

- **Octo-fiesta's model:** when you play an unowned song, it hits the Qobuz / Deezer / Yandex API with your paid streaming credentials, decrypts the audio, and writes the FLAC to disk permanently. Every play = a downloaded file. Excellent if you have a paid streaming sub and want a unified Subsonic UX over your subscription catalog.
- **Octo's model:** when you play an unowned song, you get a *YouTube preview* with zero disk impact. If you decide you want to keep it, you star it and Octo grabs the FLAC from Soulseek peers. Preview is free, ownership is opt-in.

Different audience. If you pay for streaming and want every play to enrich your library, octo-fiesta is the right tool. If you don't pay for streaming and want discovery + selective FLAC ownership, Octo is the right tool.

Other practical differences in Octo: a real admin UI, multi-peer Soulseek retry, HTTP Range support for iOS clients, Last.fm-driven discovery and radio, an interactive installer.

---

<details>
<summary><b>Advanced — architecture, technical details, more FAQ</b></summary>

### Background

Octo is a full refactor of [octo-radiostarr](https://github.com/winters27/octo-radiostarr). That earlier project ran on SquidWTF + Tidal and broke when Tidal hardened their API in April 2026. Octo pivots to **YouTube via yt-dlp** for previews and **Soulseek via slskd** for downloads — neither of which depends on a single fragile public API.

### Architecture

Three Docker containers in one `docker compose` stack:

```
┌──────────────────────────┐         ┌──────────────────┐
│  Subsonic clients        │────────▶│       octo       │──▶  Navidrome
│  (Feishin, Arpeggi, …)   │         │   (port 5274)    │     (your library)
└──────────────────────────┘         └──┬───────────┬───┘
                                        │           │
                              ┌─────────▼──┐    ┌───▼─────┐
                              │ yt-dlp shim│    │  slskd  │
                              │  sidecar   │    │ Soulseek│
                              └────────────┘    └─────────┘
```

- **`octo`** (port 5274) — the proxy + admin UI. Hijacks the Subsonic endpoints that need enrichment (`search3`, `getSimilarSongs2`, `stream`, `getCoverArt`, `star`, `scrobble`, `getTranscodeDecision`); passes everything else through to Navidrome unchanged.
- **`yt-dlp-shim`** (internal) — wraps `yt-dlp` behind two HTTP endpoints. Process-isolation keeps yt-dlp's frequent extractor breakage from affecting the rest of the stack.
- **`slskd`** (port 5030) — Soulseek client with REST API. Octo authenticates and queues downloads.

Navidrome is **not** part of the stack — Octo just talks to whatever Navidrome you already have.

### Configuration sources

Octo reads from three sources, highest priority first:

1. `settings.json` (admin UI writes here, hot-reloads in ~500ms).
2. Environment variables in `.env` / `docker-compose.yml`.
3. `appsettings.json` shipped with the image.

The admin UI's "Config sources" tab shows the merged effective value for every key.

### Download path on Windows and manual installs

`DOWNLOAD_PATH` in `.env` is a HOST path: it is bind-mounted as `/music` into the octo, yt-dlp-shim, and slskd containers, and it is the only path you change to move the library. Container-side settings (Octo's `Library__DownloadPath`, slskd's downloads dir) stay `/music`.

- **Windows (Docker Desktop)**: use forward slashes, e.g. `DOWNLOAD_PATH=E:/Media/Music`. Do not put a drive-letter path in the admin UI's download path field; that field is a path inside the container.
- **Manual installs** (not using the bundled compose file): slskd's `directories.downloads` must resolve to the same directory Octo's `Library:DownloadPath` points at, or Octo will never see finished downloads. Set it with the `SLSKD_DOWNLOADS_DIR` environment variable, and note that a value set in `slskd.yml` overrides that env var (slskd precedence: env vars < yaml).

### Existing Lidarr setup

Set `LIDARR_URL` and `LIDARR_API_KEY`, restart Octo, then open the **Lidarr** admin tab to test the connection and choose its root folder and profiles. Enable and position Lidarr under **Downloads → Heart download priority**. Octo does not install or configure Lidarr's indexers or download client.

The selected Lidarr root and Octo's effective Navidrome library root must expose the same underlying files. Their container paths may differ: Octo translates the imported path relative to the selected Lidarr root. For example, Lidarr `/data/music/Artist/Album/file.flac` can map to Octo `/music/Artist/Album/file.flac` when both mounts point at the same host directory.

`LIDARR_COMPLETION_MODE=Accepted` (default) returns control after Lidarr accepts the album search. `Imported` makes completion/failure notifications reflect the actual import, bounded by `LIDARR_IMPORT_TIMEOUT_SECONDS` (default 1800). Neither mode blocks playback or later hearts; imported files are reconciled into download history and trigger a Navidrome scan in the background.

### Playback and acquisition

Tracks already in your library play locally through Navidrome. Missing external results stream from YouTube. Playback does not acquire a permanent copy; heart the song or album to run the configured source priority.

Set `WAIT_FOR_LOSSLESS_ON_PLAY=true` if you would rather the first play wait for the lossless file. It is off by default because a Soulseek fetch routinely takes minutes and most clients time out long before that, which looks like the play failing. The setting also changes what searches advertise for external tracks, so it needs a restart, and clients that cached earlier results should re-search after you change it.

### Folder layouts

- `Flat` *(default)* — `Artist - Title.flac`.
- `Organized` — `Artist/Album/01 - Title.flac`. A track with no known album falls back to its own title as the folder. Existing files are never moved; this only affects new downloads.

### Subsonic API surface

Octo hijacks these endpoints; everything else proxies to Navidrome unchanged:

| Endpoint | Why |
|---|---|
| `search3` | merge local + Last.fm-driven external songs and Deezer-driven external albums |
| `getSimilarSongs2` | radio queue with local-first preference |
| `stream` | YouTube proxy with Range support, mp4/m4a passthrough |
| `getCoverArt` | Deezer → iTunes → Last.fm aggregator with Octo watermark |
| `getAlbum` | external album tracklists, and fills in tracks you're missing from an album you own |
| `star` | try enabled heart sources in priority order and stop after the first successful track/album acquisition |
| `scrobble` | sliding-window prewarm of next 8 in queue |
| `getTranscodeDecision` | OpenSubsonic — return direct-play for Octo IDs |

### Soulseek download details

When a song is starred, Octo:

1. Searches Soulseek for `<artist> <title>` (cleaned of `[brackets]` and redundant `Artist - ` prefixes).
2. Falls back to title-only search if the first query returns nothing usable.
3. Ranks candidates by queue depth, upload speed, file size.
4. Tries the top 5 peers in sequence with a 60s per-peer timeout.
5. Verifies the file landed on disk (slskd's polling endpoint sometimes drops successful transfers between polls).
6. Renames per `FolderStructure` setting and triggers a Navidrome rescan.

Around 30–50% of Soulseek peer requests get rejected ("overwhelmed", queue full, banned). Single-peer-try downloads were too fragile — multi-peer is the difference between "downloads sometimes work" and "downloads reliably work."

Starring an album runs the same process once per track, in sequence.

> **Hearting is "fetch", not "favorite".** Navidrome has never seen Octo's IDs for music you don't own yet, so there is nothing on its side to mark as starred. Once the files land and Navidrome rescans, they become ordinary library tracks — present, but not favorited. Star them again in your app if you want them flagged.

### Cover art aggregator

Three sources tried in order; first hit wins:

1. **Deezer** — broad international catalog, picks 1000×1000 covers.
2. **iTunes** — limit=5, scored by artist match (avoids "Karaoke Version" hits).
3. **Last.fm** — track-level images, skips the deprecated artist-image placeholder.

Cached cross-source so a queue scroll doesn't trigger N external API calls per visible song.

### FAQ

**Do downloaded songs get tagged?**
Yes — slskd downloads are full FLACs from peer libraries that already have ID3 tags. Octo organizes them per `FolderStructure`, then triggers a Navidrome rescan.

**What if all 5 Soulseek peers reject?**
Octo throws an error and the star icon stays filled. Try again later or grab the file by hand. Real failures are rare.

**Can it run without Soulseek?**
Yes. Enable YouTube for MP3 downloads, Lidarr for album-level heart acquisition, or disable every song-heart source to keep discovery without automatic acquisition.

**Can it run without Last.fm?**
Yes, but search and radio fall back to local-only — no discovery layer. The free Last.fm key takes 30 seconds.

### Development

```bash
dotnet restore
dotnet build
dotnet test
```

To build and preview the admin UI locally in an isolated Docker container:

```bash
./scripts/preview-admin.sh
```

The script opens `http://localhost:5277/admin/index.html` and uses temporary in-container settings and music directories. Run `./scripts/preview-admin.sh stop` when finished. Pass a different port as the first argument if needed.

Project layout:

| Path | What's there |
|---|---|
| `octo/Controllers/` | Subsonic API surface, admin API |
| `octo/Services/Soulseek/` | slskd client, multi-peer download logic |
| `octo/Services/Lidarr/` | Lidarr API, album submission, import reconciliation |
| `octo/Services/YouTube/` | shim HTTP client |
| `octo/Services/CoverArt/` | Deezer / iTunes / Last.fm aggregator |
| `octo/Services/Subsonic/` | request parsing, response building |
| `octo/Services/Admin/` | settings file writer (atomic, deep-merge) |
| `octo/wwwroot/admin/` | the admin UI (vanilla JS, hand-rolled CSS, no build step) |
| `yt-dlp-shim/` | Python/Flask sidecar (~200 lines) |

</details>

---

## License

[GPL-3.0](LICENSE)

## Acknowledgments

- [**Navidrome**](https://www.navidrome.org/) — the music server Octo proxies.
- [**slskd**](https://github.com/slskd/slskd) — Soulseek with a REST API.
- [**Lidarr**](https://github.com/Lidarr/Lidarr) — optional album acquisition and import manager.
- [**yt-dlp**](https://github.com/yt-dlp/yt-dlp) — makes YouTube preview feasible.
- [**Last.fm**](https://www.last.fm/api) — similar-tracks API.
- [**V1ck3s/octo-fiesta**](https://github.com/V1ck3s/octo-fiesta) — the upstream root of this lineage. The Qobuz/Deezer/Yandex Subsonic-proxy concept that Octo eventually rebuilt around YouTube + Soulseek started here.
- [**bransoned/octo-fiestarr**](https://github.com/bransoned/octo-fiestarr) — the intermediate fork of octo-fiesta whose codebase Octo's earliest commits descended from.
