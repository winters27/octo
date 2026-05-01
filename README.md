<div align="center">

<img src="octo/Assets/octo_logo.png" alt="Octo" width="280" />

# Octo

**Add discovery and downloads to your Navidrome library.**

</div>

---

## What it does

- **Search finds music you don't own.** Tap a result to hear it instantly via YouTube preview.
- **Radio works on every song.** Owned tracks play at full FLAC; missing ones preview from YouTube.
- **Heart to keep.** Star a previewed song and Octo grabs the FLAC from Soulseek, adds it to your library, and tells Navidrome to rescan. Within a minute, the song is yours forever.

Plug Octo in front of your Navidrome. Point your Subsonic apps (Feishin, Arpeggi, Narjo, etc.) at Octo instead. Nothing else changes.

## Get started

You need:

- A box with [Docker](https://docs.docker.com/engine/install/) installed.
- An existing [Navidrome](https://www.navidrome.org/) server.
- A free [Last.fm API key](https://www.last.fm/api/account/create).
- A free [Soulseek account](https://www.slsknet.org/news/node/1).

Then:

```bash
git clone https://github.com/winters27/octo.git
cd octo
./install.sh
```

The installer asks for those four things, brings the stack up, and prints the address. Point your Subsonic apps at `http://<your-host>:5274` and you're going.

## Compatible apps

| Works | App |
|---|---|
| ✅ | [Feishin](https://github.com/jeffvli/feishin) (desktop) |
| ✅ | [Arpeggi](https://www.reddit.com/r/arpeggiApp/) (iOS) |
| ✅ | [Narjo](https://www.reddit.com/r/NarjoApp/) (iOS) |
| ✅ | most other Subsonic apps |
| ❌ | Symfonium — offline-first, doesn't query the server for searches |

## Updating

```bash
git pull && ./install.sh
```

Re-running the installer keeps your existing answers.

## Settings

Open `http://<your-host>:5274/admin` for the admin UI. Every setting has a form, every backing service has a status indicator. No need to edit config files.

---

<details>
<summary><b>Advanced — architecture, technical details, FAQ</b></summary>

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

### Storage modes

- `Stream` *(default)* — preview-only. Heart a song to download.
- `Permanent` — every song you play gets downloaded.
- `Cache` — downloads expire after `CacheDurationHours`.

### Folder layouts

- `Flat` *(default)* — `Artist - Title.flac`.
- `Organized` — `Artist/Title/file.flac`.

### Subsonic API surface

Octo hijacks these endpoints; everything else proxies to Navidrome unchanged:

| Endpoint | Why |
|---|---|
| `search3` | merge local + Last.fm-driven external results |
| `getSimilarSongs2` | radio queue with local-first preference |
| `stream` | YouTube proxy with Range support, mp4/m4a passthrough |
| `getCoverArt` | Deezer → iTunes → Last.fm aggregator with Octo watermark |
| `star` | trigger Soulseek download (multi-peer retry, FLAC) |
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
Yes — set `Subsonic__DownloadOnStar=false`. Star fills the heart but won't trigger a download. Search and radio enrichment still work.

**Can it run without Last.fm?**
Yes, but search and radio fall back to local-only — no discovery layer. The free Last.fm key takes 30 seconds.

### Development

```bash
dotnet restore
dotnet build
dotnet test
```

Project layout:

| Path | What's there |
|---|---|
| `octo/Controllers/` | Subsonic API surface, admin API |
| `octo/Services/Soulseek/` | slskd client, multi-peer download logic |
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
- [**yt-dlp**](https://github.com/yt-dlp/yt-dlp) — makes YouTube preview feasible.
- [**Last.fm**](https://www.last.fm/api) — similar-tracks API.
- [**octo-fiestarr**](https://github.com/bransoned/octo-fiestarr) — original codebase Octo's earliest commits descended from.
