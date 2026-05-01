<div align="center">

<img src="octo/Assets/octo_logo.png" alt="Octo" width="160" />

# Octo

**Discovery + downloading layer for your Navidrome library.**

Type a song into your Subsonic client. See your owned tracks first, then everything Last.fm thinks you'd like next, streamable on demand from YouTube. Heart the ones you want — Octo grabs the FLAC from Soulseek, drops it in your library, Navidrome rescans, the song is yours.

[![License: GPL v3](https://img.shields.io/badge/License-GPL_v3-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)
[![.NET 9](https://img.shields.io/badge/.NET-9.0-512BD4)](https://dotnet.microsoft.com/)
[![Docker Compose](https://img.shields.io/badge/docker-compose-2496ED)](https://docs.docker.com/compose/)

</div>

---

## What is this

Octo is the missing half of self-hosted music. **Navidrome answers *"what do I have?"*. Octo answers *"what should I be listening to next, and how do I keep it?"***

It sits between your existing Subsonic clients and your existing Navidrome server. Search calls get enriched with Last.fm-driven recommendations. "Radio" on any song builds a similar-tracks queue from Last.fm and previews from YouTube. Star a song you don't own and Octo searches Soulseek peers, picks the best one, downloads the FLAC, triggers a Navidrome rescan — and within a minute the preview is replaced with a full-quality permanent copy in your library.

You don't change clients. You don't change your library. You just point your Subsonic apps at Octo instead of directly at Navidrome.

## Why Octo exists

In April 2026 Tidal hardened its API and broke every public-facing TIDAL proxy that Subsonic-side discovery tools relied on. The earlier project [**octo-radiostarr**](https://github.com/winters27/octo-radiostarr) (which I'd built around SquidWTF + Tidal) lost its primary streaming source overnight; Deezer and Qobuz only fill that gap if you're paying them.

Octo is the full refactor. It pivots to two sources that don't go away:

- **YouTube via yt-dlp** — for instant preview-quality streams.
- **Soulseek via slskd** — for permanent FLAC downloads when you keep something.

This isn't a patch on octo-radiostarr — it's a different shape, with a clean codebase, a real installer, an admin UI, multi-peer Soulseek retry, real Range support for iOS clients, and proper handling of the API quirks that off-the-shelf Subsonic clients rely on.

If you were running octo-radiostarr, this is what to switch to.

## What you get

### From your Subsonic client (Feishin, Arpeggi, Narjo, etc.)

- **Search includes discovery.** Type *"Tame Impala"* — your library hits come back at the top, plus 150 external Last.fm recommendations below. Tap any of them, hear the YouTube preview instantly.
- **Radio from any song.** Hit "Radio" on a track in your client. Octo fans out via Last.fm's `track.getsimilar`, finds 50 similar songs. Songs you already own play at full FLAC quality from your library; songs you don't preview from YouTube.
- **Star to keep.** Liking a YouTube-previewed song triggers a Soulseek search. Octo picks the best peer (queue depth, upload speed, file size), tries up to 5 in sequence if any reject, downloads the FLAC, names it per your folder convention (Flat or Organized), and triggers a Navidrome rescan. Within a minute the song shows up in your library forever.
- **Watermarked artwork.** External song covers come from a Deezer → iTunes → Last.fm fallback chain (international/indie catalogs that iTunes-US misses) and get a small Octo logo overlay so you can tell at a glance which entries are external vs already in your library.

### From the admin UI (`http://<your-host>:5274/admin`)

A real settings app. No `vim .env`, no `docker compose down/up` cycle for routine changes.

- **Status** — live health dots for every backing service (Octo, Navidrome, slskd, yt-dlp shim, Last.fm). Bad-state badge on the sidebar so you notice when something breaks.
- **Library** — download path, folder structure (`Flat` = `Artist - Title.flac`; `Organized` = `Artist/Title/file`), storage mode, download-on-star toggle, cache duration.
- **Subsonic / Last.fm / Soulseek / YouTube** — every connection setting.
- **Raw config** — edit `settings.json` directly with live JSON validation. For people who'd rather hand-write a section than click through tabs.
- **Config sources** — table of every effective config key with its current value (secrets masked). Resolves env vars vs settings file vs defaults so you can tell why a value is what it is.

Settings hot-reload on save. Settings flagged "restart required" trigger a Restart button in the sidebar (graceful exit, docker-compose brings it back in ~5s).

## Quick start

### Prerequisites

- A box with Docker installed. ([Linux install guide](https://docs.docker.com/engine/install/), [Mac install guide](https://docs.docker.com/desktop/setup/install/mac-install/))
- An existing [Navidrome](https://www.navidrome.org/) server. (Octo doesn't replace Navidrome — it sits in front of it.)
- A free [Last.fm API key](https://www.last.fm/api/account/create) for radio + discovery. Takes 30 seconds to make.
- A free [Soulseek account](https://www.slsknet.org/) for downloads on star. Also free, also 30 seconds.

### Install

```bash
git clone https://github.com/winters27/octo.git
cd octo
./install.sh
```

The installer:
1. Checks Docker is reachable.
2. Asks for the four things that need user input — Navidrome URL, music directory, Last.fm API key, Soulseek account credentials.
3. Generates a random admin password for the slskd web UI on first run.
4. Writes `.env` (chmod 600), brings the three-container stack up, waits for health.
5. Prints the admin URL.

When it's done, point your Subsonic clients at `http://<your-host>:5274` instead of your Navidrome server. Open `http://<your-host>:5274/admin` to manage settings.

### Updating

```bash
git pull
./install.sh   # idempotent — re-runs preserve all your existing values
```

## Compatible clients

Tested with off-the-shelf Subsonic / OpenSubsonic clients:

| Client | Platform | Status |
|---|---|---|
| [**Feishin**](https://github.com/jeffvli/feishin) | desktop (Mac / Win / Linux) | ✅ full feature support including OpenSubsonic transcode decisions |
| [**Arpeggi**](https://www.reddit.com/r/arpeggiApp/) | iOS | ✅ Range-supported audio streaming |
| [**Narjo**](https://www.reddit.com/r/NarjoApp/) | iOS | ✅ similar to Arpeggi |
| [Aonsoku](https://github.com/victoralvesf/aonsoku), [Subplayer](https://github.com/peguerosdc/subplayer), [Tempus](https://github.com/eddyizm/tempus), [Substreamer](https://substreamerapp.com/) | various | should work — anything implementing the Subsonic API spec |
| [Symfonium](https://symfonium.app/) | Android | ❌ won't work — offline-first architecture, never queries the server for searches |

Anything implementing standard Subsonic / OpenSubsonic should work.

## How it works

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
- **`yt-dlp-shim`** (internal-only) — wraps `yt-dlp` behind two HTTP endpoints so Octo never spawns it directly. Process isolation keeps yt-dlp's frequent extractor breakage from affecting the rest of the stack.
- **`slskd`** (port 5030) — Soulseek client with REST API. Octo authenticates and queues downloads when you star a song.

Navidrome is **not** part of this stack — it's whatever Navidrome you already have, on whatever host.

## Configuration

Octo reads configuration from three sources, highest priority first:

1. **`settings.json`** — what the admin UI writes to. Lives at `./octo-config/settings.json` on the host, mounted to `/app/config/` in the container. Hot-reloads within ~500ms when changed.
2. **Environment variables** — set in `.env` / `docker-compose.yml` at container startup.
3. **`appsettings.json`** — built-in defaults shipped with the image.

The admin UI's "Config sources" tab shows the effective merged value for every key.

### Storage modes

- **`Stream`** *(default)* — preview-only. Songs stream from YouTube and don't get saved. Star a song to download.
- **`Permanent`** — every song you play gets downloaded.
- **`Cache`** — temporary, files auto-cleanup after `CacheDurationHours`.

### Folder layouts

- **`Flat`** *(default)* — files land at `<download-path>/Artist - Title.flac`.
- **`Organized`** — `<download-path>/Artist/Title/file.flac`.

## FAQ

**Q: Do I need to give up my existing Navidrome?**
No. Octo proxies it. Your library, scrobbling, playlists, plugins, etc. all keep working — Octo just augments search and radio with discovery, and adds the star-to-download workflow.

**Q: Will my downloaded songs be tagged?**
Yes — slskd downloads are full FLACs from peer libraries that already have ID3 tags. Octo organizes them per your `FolderStructure` setting, then triggers a Navidrome rescan so they appear in your library.

**Q: What happens if Soulseek peers reject my download?**
Octo tries the next peer in queue/speed/size order, up to 5 attempts. ~30-50% of Soulseek peer requests get rejected ("Overwhelmed", queue full, banned) — single-peer-try downloads were too fragile.

**Q: Can I run this without Soulseek?**
Yes — set `Subsonic__DownloadOnStar=false` in your env. Star will fill the heart icon but won't trigger any download. You'll still get full search and radio enrichment via YouTube preview.

**Q: Can I run this without Last.fm?**
Yes, but search and radio fall back to local-only. The Last.fm key is free and takes 30 seconds; recommended.

**Q: Is this the same as octo-radiostarr?**
No. [octo-radiostarr](https://github.com/winters27/octo-radiostarr) was built around SquidWTF + Tidal and broke when Tidal hardened their API in April 2026. Octo is a clean refactor on YouTube + Soulseek with a real admin UI, multi-peer retry, Range support, OpenSubsonic compliance, and an installer.

## Development

```bash
dotnet restore
dotnet build
```

Project layout under `octo/`:

| Path | What's there |
|---|---|
| `Controllers/` | Subsonic API surface, admin API |
| `Services/Soulseek/` | slskd client, multi-peer download logic, registry/queue store |
| `Services/YouTube/` | shim HTTP client |
| `Services/CoverArt/` | Deezer / iTunes / Last.fm aggregator |
| `Services/Subsonic/` | request parsing, response building, model mapping |
| `Services/Admin/` | settings file writer (atomic, deep-merge) |
| `wwwroot/admin/` | the admin UI (vanilla JS, hand-rolled CSS, no build step) |

The yt-dlp shim is at `yt-dlp-shim/` (Python/Flask, ~200 lines).

## License

[GPL-3.0](LICENSE)

## Acknowledgments

- [**Navidrome**](https://www.navidrome.org/) — the self-hosted music server Octo proxies. Octo would be nothing without it.
- [**slskd**](https://github.com/slskd/slskd) — Soulseek daemon with REST API. Saves us from writing our own Soulseek client.
- [**yt-dlp**](https://github.com/yt-dlp/yt-dlp) — the only reason YouTube preview is feasible.
- [**Last.fm**](https://www.last.fm/api) — similar-tracks API powering radio + discovery.
- [**octo-fiestarr**](https://github.com/bransoned/octo-fiestarr) — original Subsonic-Deezer/Qobuz proxy whose codebase Octo's earliest commits descended from.
