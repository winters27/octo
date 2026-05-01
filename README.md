<div align="center">

<img src="octo/Assets/octo_logo.png" alt="Octo" width="280" />

# Octo

**The discovery layer your Navidrome library is missing.**

Search for music you don't own and hear it instantly. Heart what you like — Octo grabs the FLAC and adds it to your library forever.

</div>

---

## What is this

If you self-host **[Navidrome](https://www.navidrome.org/)**, you already know the catch: your library is only as interesting as the music you've already collected. Searching for something new just gets you "no results."

Octo fixes that. It plugs into your existing Navidrome and your existing Subsonic apps (Feishin, Arpeggi, Narjo, etc.) and adds three things:

- **Search finds new music.** Type "Tame Impala" — your owned tracks come back at the top, plus 150 Last.fm recommendations below. Tap any of them to hear it instantly via YouTube preview.
- **Radio works on every song.** Hit the radio/similar button on any track. Songs you already own play at full FLAC quality from your library; songs you don't preview from YouTube.
- **Heart to keep.** Like a song you don't own? Octo grabs the FLAC from Soulseek, adds it to your music folder, and triggers a Navidrome rescan. Within a minute, the song is permanently yours.

You don't switch apps. You don't change your library. You point your Subsonic apps at Octo instead of directly at Navidrome and everything else stays the same.

## Get started

You need:

- **A box with [Docker](https://docs.docker.com/engine/install/) installed.**
- **An existing Navidrome server** (Octo doesn't replace it, it sits in front of it).
- **A free [Last.fm API key](https://www.last.fm/api/account/create)** — 30 seconds to make one.
- **A free [Soulseek account](https://www.slsknet.org/news/node/1)** — also 30 seconds.

Then:

```bash
git clone https://github.com/winters27/octo.git
cd octo
./install.sh
```

The installer asks you four questions (Navidrome URL, music folder, Last.fm key, Soulseek login), brings everything up, and prints the address you point your apps at. That's it.

When you're done, point your Subsonic clients at `http://<your-host>:5274` instead of your Navidrome server.

## Compatible apps

| Works | App |
|---|---|
| ✅ | [Feishin](https://github.com/jeffvli/feishin) (desktop) |
| ✅ | [Arpeggi](https://www.reddit.com/r/arpeggiApp/) (iOS) |
| ✅ | [Narjo](https://www.reddit.com/r/NarjoApp/) (iOS) |
| ✅ | most other Subsonic apps |
| ❌ | Symfonium — works offline-first, never asks the server for searches |

## Updating

```bash
git pull && ./install.sh
```

The installer remembers your previous answers — re-running just refreshes the stack.

## Settings

Once it's running, open **`http://<your-host>:5274/admin`** in a browser. Every setting has a form. Status of every backing service shows on the dashboard. No need to edit config files by hand.

---

<details>
<summary><b>Advanced — architecture, technical details, FAQ</b></summary>

### Why Octo exists

This is a full refactor of an earlier project, **[octo-radiostarr](https://github.com/winters27/octo-radiostarr)**. That project was built around SquidWTF + Tidal and broke in April 2026 when Tidal hardened their API. Every public-facing TIDAL proxy went down with it.

Octo pivots to two sources that don't go away — **YouTube via yt-dlp** for instant previews, **Soulseek via slskd** for permanent FLAC downloads. It's a clean rebuild with a real installer, an admin UI, multi-peer retry, and the API quirks that off-the-shelf Subsonic clients need.

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

When you star a YouTube-previewed song, Octo:

1. Searches Soulseek for `<artist> <title>` (cleaned of `[brackets]` and redundant `Artist - ` prefixes).
2. Falls back to title-only search if the first query returns nothing usable.
3. Ranks candidates by queue depth, upload speed, file size.
4. Tries the top 5 peers in sequence with a 60s per-peer timeout.
5. Verifies the file landed on disk (slskd's polling endpoint sometimes drops successful transfers between polls).
6. Renames per `FolderStructure` setting and triggers a Navidrome rescan.

Around 30-50% of Soulseek peer requests get rejected naturally ("overwhelmed", queue full, banned). Single-peer-try downloads were too fragile — multi-peer is the difference between "downloads sometimes work" and "downloads reliably work."

### Cover art aggregator

Three sources tried in order; first hit wins:

1. **Deezer** — broad international catalog, picks 1000×1000 covers.
2. **iTunes** — limit=5, scored by artist match (avoids "Karaoke Version" hits).
3. **Last.fm** — track-level images, skips the deprecated artist-image placeholder.

Cached cross-source so a queue scroll doesn't trigger N external API calls per visible song.

### FAQ

**Do my downloaded songs get tagged?**
Yes — slskd downloads are full FLACs from peer libraries that already have ID3 tags. Octo organizes them per your `FolderStructure`, then triggers a Navidrome rescan.

**What if all 5 Soulseek peers reject?**
Octo throws an error and your star icon stays filled (UI feedback). Try again later or hand-grab the file. Real failures are rare in practice.

**Can I run this without Soulseek?**
Yes — set `Subsonic__DownloadOnStar=false`. Star fills the heart icon but won't trigger a download. Search and radio enrichment still work via YouTube.

**Can I run this without Last.fm?**
Yes, but search and radio fall back to local-only — no discovery layer. The free Last.fm key takes 30 seconds to make.

**Is this the same as octo-radiostarr?**
No — that project was Tidal-based and broke when Tidal hardened their API. Octo is a clean refactor on YouTube + Soulseek with a real admin UI, multi-peer retry, Range support, and an installer.

### Development

```bash
dotnet restore
dotnet build
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
- [**octo-fiestarr**](https://github.com/bransoned/octo-fiestarr) — original Subsonic-Deezer/Qobuz proxy whose codebase Octo's earliest commits descended from.
