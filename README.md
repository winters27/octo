<div align="center">

<img src="octo/Assets/octo_logo.png" alt="Octo, self-hosted music discovery for Navidrome" width="280" />

# Octo

**Self-hosted music discovery for Navidrome.**
Play songs you don't own yet, and keep the ones you like as FLAC.

[![License: GPL v3](https://img.shields.io/badge/License-GPL_v3-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)
[![.NET 9](https://img.shields.io/badge/.NET-9.0-512BD4)](https://dotnet.microsoft.com/)
[![Docker Compose](https://img.shields.io/badge/docker-compose-2496ED)](https://docs.docker.com/compose/)
[![CI](https://github.com/winters27/octo/actions/workflows/ci.yml/badge.svg)](https://github.com/winters27/octo/actions/workflows/ci.yml)

</div>

---

## Octo's own apps

Octo is a proxy, so it works with your Navidrome server and the Subsonic apps you already use. If you'd like players made to go with it, there are Octo apps for desktop and Android. In them, the music Octo finds sits beside your library, and keeping a song is just **Add to library**.

![Home in the Octo desktop app](docs/images/players/desktop-home.webp)

- **Add songs to your library.** Press **+** on a song, an album or a search result and it joins your library.
- **One search for everything.** Your music comes first, then what Octo found, and all of it plays straight away.
- **Whole albums.** Every track shows, the ones you have are marked, and one press adds the rest.
- **Follow every download.** The downloads drawer lists what Octo is fetching for you and what it fetched lately. Each one opens to its log: what was searched, the copies found with their format, size and peer, the one chosen and why, every check, the tags, the cover and the lyrics.
- **Find songs.** Run a song's search again on your download sources (Soulseek, and Lidarr's releases), see every copy with its details, and pick the one you want. For a song you have, only a lossless copy can take its place.
- **Your stations on Home**, each with its own painted cover.

![An album in the Octo desktop app](docs/images/players/desktop-album.webp)

![A station in the Octo desktop app](docs/images/players/desktop-playlists.webp)

![The player in the Octo desktop app](docs/images/players/desktop-player.webp)

<table>
<tr><td width="33%"><img src="docs/images/players/phone-search.webp" alt="Search in the Octo Android app"></td><td width="33%"><img src="docs/images/players/phone-album.webp" alt="An album in the Octo Android app"></td><td width="33%"><img src="docs/images/players/phone-player.webp" alt="The player in the Octo Android app"></td></tr>
</table>

The desktop app runs on Windows and Linux, and the Android app on Android 10 and newer. They need Octo 2026.09.29 or newer, and they work as regular players with Navidrome too. Download them from [Octo for Windows and Linux](https://github.com/winters27/octo/releases/tag/desktop-v1.3.2) and [Octo for Android](https://github.com/winters27/octo/releases/tag/android-v1.2.4), or add [Octo's F-Droid repository](https://winters27.github.io/octo/fdroid/) so the Android app updates through F-Droid, Droid-ify or Neo Store. The source is at [winters27/octo-player](https://github.com/winters27/octo-player).

## What Octo does

Octo sits in front of Navidrome and adds what a streaming service gives you: search past your own library, radio, and stations that learn from what you play. Previews stream from YouTube, and the songs you keep arrive from Soulseek, or your own Lidarr, as tagged files in your library.

- **Search finds music you don't own**, and any of it plays right away as a preview.
- **Radio and stations grow from your listening:** Your Mix, discovery, artist and genre stations, plus optional genre and decade mixes from your own library.
- **Keep what you like.** Octo downloads it, tags it, files it under the right album and tells Navidrome to rescan. Whole albums work too.
- **Downloads are checked.** A "lossless" file made from an MP3 is caught, and optional Review and Duplicates playlists show what Octo couldn't confirm and what you have twice.
- **Lyrics** land beside downloads, and come in live for songs that have none.

Any Subsonic app works: point it at Octo instead of Navidrome and nothing else changes.

> If you pay for Qobuz, Deezer or Yandex Music and want that catalog in your library, [V1ck3s/octo-fiesta](https://github.com/V1ck3s/octo-fiesta) fits better, since it downloads from those services directly. Octo needs no paid streaming account.

## Get started

Octo sits **in front of** your existing Navidrome. Your Subsonic app talks to Octo; Octo adds discovery and previews, then proxies everything else through to Navidrome:

```
   Subsonic app          Octo               Navidrome
  (Feishin, Arpeggi) ──▶  :5274  ──────────▶  (your library)
                           ├─▶ yt-dlp shim   (instant previews)
                           ├─▶ slskd         (downloads on star)
                           └─▶ your Lidarr   (optional heart source)
```

So setup is two steps: **tell Octo where Navidrome is**, and **point your app at Octo**.

**Required**

- A box with [Docker](https://docs.docker.com/engine/install/) installed.
- An existing [Navidrome](https://www.navidrome.org/) server, reachable from the Octo host by LAN IP or service name (not `localhost`).

**Optional** (Octo runs fine without these):

- A free [Last.fm API key](https://www.last.fm/api/account/create) enables radio and discovery.
- A free [Soulseek account](https://www.slsknet.org/news/node/1) enables lossless FLAC downloads when you star a song.
- An existing [Lidarr](https://github.com/Lidarr/Lidarr) server: an alternative heart source once it has working indexers and a download client.

Then:

```bash
git clone https://github.com/winters27/octo.git
cd octo
./install.sh
```

The installer asks for your Navidrome URL (and, optionally, Last.fm and Soulseek), brings the stack up, and prints the address.

**When it's done:**

- Point your Subsonic apps at `http://<your-host>:5274`, **not** Navidrome's own address.
- Octo checks your Navidrome sign-in before it plays or fetches a song from outside your library; while it cannot reach Navidrome, those songs are refused.
- Open the admin dashboard at **`http://<your-host>:5274/admin`** to manage every setting from the browser, with no config files to edit by hand.
  It is unauthenticated, so keep Octo on a trusted network. See [Admin dashboard](#admin-dashboard).
- If a client reports the server is unreachable, that is Octo telling you setup is not finished: its ping response spells out exactly what to fix (usually the Navidrome URL).

## Compatible apps

| Works | App | Platform |
|---|---|---|
| ✅ | [Octo's own apps](#octos-own-apps) | Windows, Linux, Android |
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
| 🟡 | [Symfonium](https://symfonium.app/) | your stations' tracks, not free-text search (see below) |

<details>
<summary><b>Symfonium, and apps that search in older ways</b></summary>

**Symfonium** copies your library to the phone and searches only that copy, so a typed search
never reaches Octo. What does reach Octo is the copy itself: Symfonium pages through the whole
library with an empty `search3` query. Octo continues that walk past your last song with
your radio stations' tracks that you don't own yet, so they land on the phone as ordinary
songs. There they search, browse and play as previews like anything else, the station
playlists find their tracks, and hearting one downloads it exactly as it does from any
other app. What you can't do is search for any song at all: search on the phone only finds
what the stations have suggested.

- Run a full sync in Symfonium to pick up new suggestions. Stations refresh as you listen,
  and Octo rebuilds the list when they change, or an hour after its last build.
- A song you heart shows up twice for a while: the preview, and the downloaded copy once
  Navidrome has scanned it. The next sync after the list is rebuilt drops the preview.
- Controlled under **Behavior → Discovery for offline-search apps** in the admin dashboard
  (`ENABLE_SYNC_CATALOG`, `SYNC_CATALOG_CLIENTS`, `SYNC_CATALOG_MAX_SONGS`). Only apps listed
  there get the extra tracks, so apps that search the server keep a clean library view.

**Both search generations are supported.** Subsonic has two search endpoints, `search2` and
`search3`, and Octo answers either. This matters more than it sounds: DSub and Ultrasonic
choose between them based on whether *you* browse by tags or by folders, not on the server
version, so a folder-browsing user talks `search2`. Both formats are supported too: some
clients speak JSON, some (DSub) only XML.

</details>

## Updating

Octo checks GitHub every 6 hours for a newer release. When one is out, the dashboard says so at the top of every page, and **About** shows what's new.

### From the dashboard

On Linux with systemd, the installer offers a small update helper. With it, **About → Update now** installs the new release: the helper fetches it, builds it, and restarts Octo, and the dashboard shows each step.

- Octo itself never gets access to Docker. It can only ask, by writing a file in its config folder, and only for the newest published release.
- The helper builds the new release before it stops anything, so a failed build leaves Octo running as it was. If the new release will not stay up, the helper goes back to the old one.
- It leaves the folder alone when Octo's own files have local changes. Your `.env`, `docker-compose.override.yml` and config are not Octo's files, so they never block it.

To add the helper to an existing install, or take it off again:

```bash
scripts/updater/install-updater.sh
```

```bash
scripts/updater/install-updater.sh --remove
```

### By hand

Without the helper, **About** shows the command for the new release. From the Octo folder:

```bash
git fetch --tags && git checkout --detach 2026.10.05 && docker compose build && docker compose up -d
```

Octo builds from source, so this is what actually updates it. `docker compose pull` only refreshes slskd. Re-running `./install.sh` also works and keeps your existing answers.

If you track `main` instead of releases, `git checkout main && git pull && ./install.sh` still works.

To turn the release check off, set `UPDATES_CHECK=false`, or switch off **Look for new releases** under **About**.

### Which version am I on

Releases are dated, so `2026.07.29` is the release cut on that day. Your running version is shown under **About** in the admin dashboard, and it is the single most useful thing to include in a bug report.

To pin to a release instead of tracking `main`:

```bash
git checkout 2026.07.29 && ./install.sh
```

Prebuilt multi-arch images are also published to `ghcr.io/winters27/octo`, tagged `latest`, the release date, and the commit sha.

## Admin dashboard

`http://<your-host>:5274/admin`

Every setting has a form, every backing service has a live status indicator, and the **Raw Config** tab lets you edit the whole effective configuration as a JSON file if you'd rather work that way. Changes hot-reload: no rebuild, and no restart for most settings. The few that only apply after a restart are marked **Restart** where you edit them, and anything saved but still waiting on a restart is listed at the top of every page until you restart Octo.

> [!WARNING]
> **The admin dashboard has no authentication, so run Octo on a trusted network only.**
>
> Anyone who can reach port 5274 can change every setting, including your Last.fm API key
> and shared secret, your Navidrome admin login, and the slskd sign-in Octo uses, and
> connect or disconnect each listener's Last.fm. The passwords and the shared secret show
> only as a placeholder, but most other settings, API keys included, can be read.
> Nothing on that page asks who you are.
>
> Do not port-forward 5274 or put it on a public hostname. If you need Octo from
> outside your network, reach it over a VPN such as [Tailscale](https://tailscale.com/)
> or [WireGuard](https://www.wireguard.com/), or put it behind a reverse proxy that
> requires authentication and blocks `/admin` outright. A proxy that only fronts the
> Subsonic API and refuses `/admin` and `/api` is enough for music clients, since those
> only need `/rest`.
>
> Octo does refuse admin changes from other websites and stops them reading the admin API:
> a write has to carry an `X-Octo-Admin` header, which a page on another origin cannot add.
> A script that changes settings must send that header too. **This is not a login.** Anyone
> who can reach port 5274 directly, or a DNS-rebinding page, can still use the dashboard, so
> the advice above stands.

## Notifications

Optional push notifications for the download lifecycle, because Subsonic has no way to
tell you a starred track landed, or quietly settled for a lossy copy.

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

It's the discovery half. Octo doesn't replace your music *server* (that's still Navidrome), but it adds the search-and-listen-to-anything experience that streaming services do well. With Octo plugged in, your Subsonic app behaves more like Spotify or Apple Music: search returns recommendations, radio works on any song, and you can preview tracks you don't own. The difference is that "I want to keep this" downloads it as a real FLAC into your library, instead of renting it.

### Does this work with Plex / Plexamp?

No. Octo speaks the Subsonic API, not the Plex API. If you're a Plex user looking for self-hosted alternatives with discovery, the move is Navidrome + Octo + a Subsonic client like Feishin or Arpeggi.

### How is this different from Navidrome's built-in radio?

Navidrome's radio plays songs from your existing library. Octo's radio reaches *outside* your library: Last.fm finds similar tracks, YouTube provides the preview, and Soulseek provides the keep-it-forever path. Navidrome alone gives you a great library player; Octo turns that library into a launchpad for discovery.

### Is my data going anywhere?

Octo's per-user play ledger and station snapshots stay in `/app/config/lastfm-radio-state.json`. It sends Last.fm only the artist, title, and tag lookups needed to build recommendations; it does not send the ledger, Navidrome credentials, usernames, or stream URLs. Continuous Radio URLs contain opaque, expiring in-memory session tokens rather than Navidrome credentials. Every 6 hours Octo asks GitHub for its own release list, sending nothing but its version in the request's user agent; `UPDATES_CHECK=false` stops it. YouTube and Soulseek receive the ordinary outbound lookups needed for preview/acquisition. Once a listener connects Last.fm on the dashboard, Octo also sends that listener's plays (outside songs, and library songs unless left to Navidrome; artist, title, album and time) to their own Last.fm account, and with a ListenBrainz token set it sends the same plays to ListenBrainz.

### Do downloaded songs get tagged correctly?

Yes. Every download is identified before it is filed: the fingerprint service's answer, the music database and Deezer each offer the releases the song could be from, and every one is weighed against what was asked for and what the file is. A sure match tags the file with the full release set (album, original date, label, catalogue number, barcode, release type and status, the recording and release ids), measures its loudness for ReplayGain, and gives it the largest cover the cover chain can find. A doubtful match keeps the name that was asked for, fills only what is missing, and goes to the Review playlist as before. The Fetched songs page shows, under each row, which release won, by how much, and where each field came from. Octo then organizes the file per your `FolderStructure` setting (`Flat`, `ByArtist` or `Organized`) and triggers a Navidrome rescan, so it appears in your library exactly like everything else you own. The details are under [Tags, covers and lyrics](#tags-covers-and-lyrics).

### Can it run on a Raspberry Pi?

Yes. Multi-arch images are published for amd64 and arm64. The yt-dlp sidecar does most of the CPU work; a Pi 4 or Pi 5 handles a single household's listening fine.

---

<details>
<summary><b>Advanced: architecture, technical details, more FAQ</b></summary>

### What if I don't want to use Soulseek?

You can use YouTube or an existing Lidarr server, or disable automatic acquisition entirely.

Use **Streams & hearts → Heart download priority** in the admin UI to order Soulseek, YouTube, and Lidarr and independently choose whether each handles song hearts, album hearts, or both. Octo tries eligible sources from top to bottom and stops at the first success. `DOWNLOAD_SOURCE`, `DOWNLOAD_ON_STAR`, and `DOWNLOAD_ALBUM_ON_STAR` remain migration defaults for existing and env-only installations.

Lidarr works at album level, so enabling it for song hearts still fetches the song's full album. It is last and disabled by default; configure its URL, API key, root folder, and profiles on the Lidarr page, then enable the heart types you want in the priority list.

To stop downloading altogether, turn off both heart types for every source. On an env-only installation, set `Subsonic__DownloadOnStar=false` and `Subsonic__DownloadAlbumOnStar=false`. A heart on a song Octo found for you then downloads nothing and is not kept, because Navidrome has no such song to favorite.

`RECORD_REQUESTED_BY` (on by default) names the Subsonic user who asked for each download on
its entry in **Fetched songs** and on the download notification, so on a shared library you can
tell one person's acquisitions from another's. A track that two people star while it is still
downloading lists both, because the second star joins the transfer already running rather than
starting a second one. Acquisitions Octo starts itself are unattributed, as are all entries
written before this existed. Turning it off stops the username being captured at all rather
than hiding it afterwards, so nothing downstream holds it; names already written stay.

A heart on a song you already have is a favorite in Navidrome, straight away, and downloads
nothing. A heart on a song you do not have only downloads it; heart it again once it is in your
library to make it a favorite. `STAR_DOWNLOADS_FOR_REQUESTER` (off by default) makes Octo also
favorite a download when it lands, for the person who hearted it; an album heart then
favorites the album. Octo's own apps are left out,
because their star button means Add. The person's sign-in is held in memory
with the download until the song arrives (a password is first turned into a token, so the
password itself is never held), and a restart drops it.

### Why is Octo a refactor of [octo-radiostarr](https://github.com/winters27/octo-radiostarr)?

The earlier project leaned on SquidWTF (a public TIDAL proxy) for streaming. In April 2026 Tidal hardened their API and broke every TIDAL proxy at once. Rather than patch around it, Octo was rebuilt on two sources that don't depend on a single fragile vendor API: YouTube via yt-dlp, and Soulseek via slskd. The old repo is archived; new development happens here.

### How is Octo different from [octo-fiesta](https://github.com/V1ck3s/octo-fiesta)?

Octo's earliest commits descended from [V1ck3s/octo-fiesta](https://github.com/V1ck3s/octo-fiesta) (via [bransoned/octo-fiestarr](https://github.com/bransoned/octo-fiestarr)), so the *concept* is the same: a Subsonic proxy that fills in songs you don't own. The implementation has diverged completely:

- **Octo-fiesta's model:** when you play an unowned song, it hits the Qobuz / Deezer / Yandex API with your paid streaming credentials, decrypts the audio, and writes the FLAC to disk permanently. Every play = a downloaded file. Excellent if you have a paid streaming sub and want a unified Subsonic UX over your subscription catalog.
- **Octo's model:** when you play an unowned song, you get a *YouTube preview* with zero disk impact. If you decide you want to keep it, you star it and Octo grabs the FLAC from Soulseek peers. Preview is free, ownership is opt-in.

Different audience. If you pay for streaming and want every play to enrich your library, octo-fiesta is the right tool. If you don't pay for streaming and want discovery + selective FLAC ownership, Octo is the right tool.

Other practical differences in Octo: a real admin UI, multi-peer Soulseek retry, HTTP Range support for iOS clients, Last.fm-driven discovery and radio, an interactive installer.


### Background

Octo is a full refactor of [octo-radiostarr](https://github.com/winters27/octo-radiostarr). That earlier project ran on SquidWTF + Tidal and broke when Tidal hardened their API in April 2026. Octo pivots to **YouTube via yt-dlp** for previews and **Soulseek via slskd** for downloads, neither of which depends on a single fragile public API.

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

- **`octo`** (port 5274): the proxy + admin UI. Personalized Radio, its state store, recommendation queue, and refresh worker all run in this process. Octo hijacks the Subsonic endpoints that need enrichment and passes everything else through to Navidrome.
- **`yt-dlp-shim`** (internal): wraps `yt-dlp` behind two HTTP endpoints. Process-isolation keeps yt-dlp's frequent extractor breakage from affecting the rest of the stack.
- **`slskd`** (port 5030 for its web page, 50300 for other Soulseek users): Soulseek client with REST API. Octo authenticates and queues downloads, and slskd shares your library back, read-only (see [Sharing back on Soulseek](#sharing-back-on-soulseek)).

Navidrome is **not** part of the stack. Octo just talks to whatever Navidrome you already have.

### Configuration sources

Octo reads from three sources, highest priority first:

1. `settings.json` (admin UI writes here, hot-reloads in ~500ms).
2. Environment variables in `.env` / `docker-compose.yml`.
3. `appsettings.json` shipped with the image.

The admin UI's "Config sources" tab shows the merged effective value for every key.

Last.fm Radio is enabled by default. `LASTFM_EXPOSE_AS_PLAYLISTS` and
`LASTFM_EXPOSE_AS_STREAMS` independently publish Octo stations in those two client
surfaces; both default to true. Playlist tracks retain normal playback quality.
Continuous streams are normalized to `LASTFM_RADIO_STREAM_BITRATE_KBPS` (96, 128, 192,
256, or 320; default 192). Octo warms persisted stations at startup, publishes only
stations with a complete starter track, and maintains a three-track runway in its
bounded 24-hour/512 MiB temporary cache. Unplayable tracks are rejected for 24 hours
and replaced through the normal refresh path; no prepared track is added to the music
library. **Start radio from this song** remains a one-time `getSimilarSongs[2]` queue.

`GENRE_NORMALIZE` collapses the genres downloads arrive with into a list you can browse.
Rules are a pattern-to-genre table applied **in order, first match wins**, edited in the
dashboard (or as `Genre.Mappings` in settings JSON) because encoding structured rows in
`.env` is brittle; a broad-genre preset is one click away. A built-in blocklist drops
YouTube categories, format tags and years, and `GENRE_BLOCKLIST` adds to it. `GENRE_MAX`
caps how many genres a track keeps, and defaults to keeping what is already there so
switching normalisation on is not itself destructive; set it to 1 for one broad genre per
track. `GENRE_ON_EMPTY` decides what happens when nothing
survives: `Clear` removes the genre and is the default, because genre was previously only
ever written when non-empty and never cleared, so junk like "People & Blogs" survived
forever; `Leave` keeps it and `Unknown` writes `GENRE_UNKNOWN_LABEL`. A file that had no
genre and resolved to none is left untouched either way. `GENRE_FALLBACK=LastFm` fills a
blank genre from Last.fm's top tags, which needs `LASTFM_API_KEY` but not radio. This
applies to new downloads. To apply it to files already in the library, the **Tags & genres** page
has a re-tag tool: pick a scope, **Preview changes** walks every file and writes nothing, and only
then can you apply. Apply is withheld if you change the scope or the rules after previewing,
because it would write something other than what the preview showed. An undo that is cancelled
or cannot reach a file keeps that file's entry, so running Undo again finishes the job. Every one of its endpoints requires signing in with a Navidrome admin
account, because `/api/admin` has no authentication of its own and this rewrites tags.

Applying records each changed genre frame in `/app/config/genre-backfill-journal.jsonl`, which
backs a one-click undo. **Undo restores the genre and nothing else**: writing a tag rewrites the
whole tag block, so an unusual field the tag library does not model is lost on the first save;
entries are matched by file path, so a file moved afterwards stays changed; and if that journal
is gone there is no undo at all. Keep your own backup of the music folder. A run that is
interrupted by a restart is never resumed automatically, since restarting may be how you
stopped it.

Library actions let a user fix a wrong download from the player they are already using, by
adding the track to an action playlist Octo keeps (`LIBRARY_ACTIONS_PLAYLISTS`) or, if they
turn it on, by rating it (`LIBRARY_ACTIONS_RATINGS`). `LIBRARY_ACTIONS_ENABLED` is off by
default, and the feature stays inert even when on until at least one username is added to the
allowlist in the dashboard: **an empty allowlist means nobody, never everybody.** The action
names, which actions exist, and which star count maps to which action are all editable, and
five stars means Keep, which removes nothing, so the top of the scale is never destructive.
A replacement (Better quality, Wrong version, Wrong song) takes the original's place in
Navidrome: it keeps the original's file name, title, album and album artist tags, so its plays,
favorites and playlist places stay with it. The action history says whether Navidrome kept it.

`LIBRARY_ACTIONS_DRY_RUN` is on by default, so the first run of a newly enabled install is a
rehearsal you can read before anything is real. Nothing is ever deleted outright: removed files
move to `LIBRARY_ACTIONS_TRASH_DIR` under the music folder, with a sidecar manifest so a restore
works even if the action journal is lost, and only the retention sweep
(`LIBRARY_ACTIONS_TRASH_DAYS`, 0 to keep forever) really deletes. `LIBRARY_ACTIONS_POLL_SECONDS`
and `LIBRARY_ACTIONS_MAX_PER_CYCLE` bound how fast actions are noticed and applied.

`LIBRARY_ACTIONS_UPGRADE_PER_WEEK` (0, the default, is off) has Octo upgrade that many lossy
songs a week to lossless by itself, through Better quality, spread evenly across the week and
one at a time. It needs library actions on, Better quality switched on, someone on the
allowlist (it acts as the first person there) and Octo's Navidrome admin credential to read the
library. It waits while anything else is downloading, takes songs it has never tried first, and
leaves a song alone for four weeks after trying it. With dry run on it only rehearses. Better
quality and this weekly upgrade search Soulseek for up to `SLSKD_UPGRADE_SEARCH_WAIT_SECONDS`
(default 90, 30 to 300) instead of the usual `SLSKD_SEARCH_WAIT_SECONDS`, because they look for
songs the quick search did not find. A change to it applies after a restart.

Two things to know before turning ratings on. Clearing a rating afterwards needs the rating
owner's own credentials, because Subsonic ratings are per user, so **Octo caches a replayable
Subsonic auth triplet per user in memory** for as long as it runs. And because no client asks
for confirmation before setting a star, a mis-tap is a request. The playlists carry no such
risk, which is why they are the default.

An app can also remove a song directly, without a playlist or a rating, through the
`octoLibraryActions` extension, which is only listed while library actions are on.
`getLibraryActions` tells the caller whether actions are on, whether they are on the allowlist,
whether it is a dry run, and how many days a removed file is kept. `libraryAction` with an `id`
and `action=remove` does exactly what the Delete playlist does, with the same allowlist, dry run
and quarantine, as the user whose credentials it carries, and answers with what happened. The
Delete action has to be on for it to do anything, and the caller has to be a Navidrome admin.
Octo asks Navidrome for a scan right after, so the song leaves the library in seconds rather
than at the next scheduled scan. With `copy=true` (Library health removing a second copy of a song the library
keeps) the song is not refused when it is asked for again, since only that copy was unwanted. Both always answer in JSON.

Version 3 of the extension is what the apps' Library health fixes with. Every one of these needs
library actions on, the caller on the allowlist and a Navidrome admin, and only rehearses while
dry run is on. `libraryAction` takes `action=` one of:

| Action | What it does |
| --- | --- |
| `restore` | puts a removed song back where it was, from the trash; `getLibraryTrash` lists the songs there and when each goes for good |
| `retag` | writes the tags sent (`title`, `artist`, `album`, `albumArtist`, `year`, `genre`, `track`, `disc`, `isrc`; an empty one clears it, except title, artist and album) into the file, in place, so Navidrome keeps the song's id, plays and playlist places |
| `joinAlbum` | with `like=` another song's id, gives the song that song's album tags (title, album artists, release date, MusicBrainz album id, compilation, year), which mends an album Navidrome shows as two |
| `cover` | finds the album's cover the way the Cover art page does and puts it inside a file that has no picture; `preview=true` only says what it found |
| `lookup` | the tags a download of the song would get, beside what the file says now; writes nothing |
| `undo` | puts back the last `retag`, `joinAlbum` or `cover` of the song, unless the file has changed since |

Each edit is kept in `tag-edits.json` beside the settings, with the tags before and after, and is
followed by one Navidrome scan a few seconds later (a normal scan, never a full one).

`LIBRARY_ACTIONS_REVIEW` gives each allowed user a Review playlist, where Octo asks about the
downloads a person can settle by listening: AcoustID had never heard the recording, was not
sure of it, or heard a different one in a YouTube download. A download is asked about in the
playlist of the person who requested it when they are on the allowlist, and of every allowed
user otherwise. Answer by adding the track to an action playlist, by Keep (a fifth action that
removes nothing, on five stars by default), or by removing it from Review, which means "fine,
stop asking". Nothing in Review is ever acted on by itself, and a settled track is not asked
about again. `LIBRARY_ACTIONS_NOTICE_PREFIX` (`▸ ` by default) sets Octo's playlists apart
from the action playlists, since in one Octo asks you something and in the other you tell Octo
something, and `LIBRARY_ACTIONS_NOTICE_MAX` (default 100) is how many questions one playlist
holds at once; the rest wait their turn. `LIBRARY_ACTIONS_RATINGS_SCOPE` says where a star
counts as a command: `NoticeOnly` only on a track in Review or Duplicates, where the only
reason to rate it is to answer, and `Global` on any track. `Auto`, the default, is
`NoticeOnly` while either playlist is on and `Global` otherwise, which is how ratings behaved
before they existed.

`LIBRARY_ACTIONS_REVIEW_SWEEP_PER_HOUR` (0, the default, is off) has Review check music that
was already in the library too, that many songs an hour, and only while nothing is
downloading. It needs download verification on (`SLSKD_VERIFY_DOWNLOADS`) and an AcoustID key
(`ACOUSTID_API_KEY`). It asks one person, the library keeper: the Navidrome admin when they are
on the allowlist, otherwise the first allowed user. Besides the usual questions it asks when AcoustID
is sure a song is something else, or when a song runs much longer or shorter than the
recording it matched; those are questions too, never acted on by themselves. It stops while 50
of its questions wait for an answer, and a Keep on one sends nothing to AcoustID, since its
tags were never confirmed. Octo's own downloads are skipped, since each was checked when it
arrived if verification was on at the time; `LIBRARY_ACTIONS_REVIEW_SWEEP_OCTO_DOWNLOADS` checks
them too.

`LIBRARY_ACTIONS_DUPLICATES` adds a Duplicates playlist per allowed user: recordings the
library holds more than once, side by side, the copy worth keeping first (lossless before
lossy, then the higher bitrate). Two files are copies only when they carry the same
MusicBrainz recording id and are the same version, so a live take, a remix, a radio edit or a
second part never is, and a file without a recording id is never grouped; Octo writes that id
on every download it confirms, and Picard does too. Octo only points copies out. Remove the
one you do not want with an action playlist; Keep, or taking a copy out of the playlist, says
the copies are on purpose and stops Octo asking about them. The library is walked every
`LIBRARY_ACTIONS_DUPLICATES_SCAN_HOURS` (default 24) with Octo's Navidrome admin credential,
and the dashboard can start a walk at once.

`SLSKD_VERIFY_DOWNLOADS` fingerprints each finished Soulseek download with Chromaprint and
identifies it through AcoustID before it joins the library, using the free key in
`ACOUSTID_API_KEY`. A file identified as a different recording is deleted and its peer and
filename are remembered in `/app/config/rejected-peers.json`, so that exact file is never
downloaded again; entries lapse after 30 days and the Soulseek admin page can forget them all
at once. `SLSKD_MIN_MATCH_SCORE` (50-99, default 85) is how sure AcoustID must be before its
answer may reject anything, so raising it makes Octo *more* permissive, because weaker matches
are ignored rather than acted on. A track with no AcoustID entry at all is always accepted.
`SLSKD_TAG_FROM_MUSICBRAINZ` writes the matched recording's MusicBrainz title, artist, album
and year over the peer's own tags. `NAME_FROM_MATCH` goes one step further and names the file
from the match as well (artist folder, title, album and track number), so the path and the
tags come from one decision; it is off by default because a canonical name is not always the
one you file under, and it only ever names files Octo downloads and confirms. YouTube
downloads are identified too, but never rejected: YouTube has no second candidate, so a
disagreement is kept and, with the Review playlist on, asked about. Verification needs
`fpcalc` in the runtime image (`libchromaprint-tools`); without it the feature logs once and
accepts everything. A song asked for with an ISRC (an album track Deezer listed) is also held
to that code: a file whose own tags carry it is confirmed even with no AcoustID key or entry,
and a fingerprint that names a recording spelled differently (a title in its own script, or
translated) is confirmed when MusicBrainz lists the ISRC on it. A different ISRC never rejects
a file, since re-releases are often given new codes.

"Catch fake lossless files" on the Soulseek admin page (on by default) checks every download
that claims to be lossless for a lossy file converted to it. Octo decodes a few seconds from
several points in the track with ffmpeg and looks for the cutoff a lossy encoder leaves: about
17 kHz for a 128 kbps MP3, 19 kHz for 192, 20 kHz for 256 and 320. It needs a steep drop that
stays at the floor, so a recording that is simply quiet up high is never called fake. A likely
transcode is held back while the next lossless copy is tried, and kept when no peer has a
genuine one: it is still the right song, and the download record notes what it was likely
made from. The better-quality library action refuses a transcode as a replacement, and the
Duplicates playlist never suggests one over a genuine lossless copy.

`ACOUSTID_SUBMIT` sends answers back. When someone Keeps a track from Review that AcoustID had
never heard or was not sure of, Octo submits its fingerprint with the MusicBrainz recording it
belongs to, so the next lookup of that recording is a confident one. It needs your own
AcoustID user key in `ACOUSTID_USER_KEY` (from acoustid.org/api-key once signed in) as well as
the application key, and sends the fingerprint, its length, the recording id and the file
format, never a file name, a path or a username. Nothing is sent in rehearsal mode, for a
track AcoustID named as something else, for a fingerprint other than the standard 120 seconds
(`SLSKD_FINGERPRINT_SECONDS`), or when no single MusicBrainz recording fits the track's title,
artist and length.

Each kind of dynamic station is configured on its own, so a listener can keep Your Mix
without collecting an artist radio per favorite band. `LASTFM_ENABLE_YOUR_MIX` and
`LASTFM_ENABLE_DISCOVERY_MIX` (both default true) switch those two stations,
`LASTFM_ARTIST_STATION_COUNT` (default 2) and `LASTFM_GENRE_STATION_COUNT` (default 3)
say how many of each to build, and 0 builds none. The defaults are what Octo has always
produced. All four take effect without a restart, and switching one off removes those
stations from clients on the next request.

Other Radio defaults use
`LASTFM_ENABLE_PERSONALIZED_STATIONS`, `LASTFM_ENABLE_DISCOVERY_STATIONS`,
`LASTFM_HISTORY_RETENTION_DAYS`, `LASTFM_DISCOVERY_PERCENT`,
`LASTFM_RADIO_TRACK_COUNT`, `LASTFM_REFRESH_INTERVAL_HOURS`, and
`LASTFM_MINIMUM_PLAYS`. Pinned categories are managed as
`LastFm.DiscoveryStations` in the existing Last.fm admin tab or settings JSON because
encoding structured rows in `.env` is brittle.

The in-process worker refreshes stale snapshots in the background while retaining the
last good version. State is bounded and versioned in
`/app/config/lastfm-radio-state.json`; do not share that file across Octo instances
because cross-process locking is not supported.

### Mixes

`MIXES_ENABLED` adds genre and decade mixes drawn from each listener's own library, listed
beside the radio stations and served by Octo in the same way: per listener, read-only, and
never written to Navidrome, so a rescan cannot empty one and nobody edits one by accident.
`MIXES_GENRES` and `MIXES_DECADES` choose the kinds. A genre or decade gets a mix once it has
`MIX_CREATE_AT` tracks (default 20) and loses it only below `MIX_REMOVE_BELOW` (default 10),
so one at the edge does not come and go, and `MIX_MAX_PLAYLISTS` (default 20) shows the
largest first. Years and anything on the genre blocklist never get a mix of their own.

Each mix holds `MIX_TRACK_COUNT` tracks (default 100) with at most `MIX_MAX_PER_ARTIST` (3)
by one artist; the cap is never relaxed, so a mix that cannot be filled without breaking it
is shorter. A mix is a seeded draw that holds still for `MIX_REFRESH_HOURS` (24), so every
client shows the same tracks, and is then drawn again. `MIX_NEW_SHARE` keeps that percentage
of each mix, and of the Discovery Mix station, for tracks new to the listener: never played,
or added in the last `MIX_NEW_DAYS`. It is 0 by default, which changes nothing.
`MIX_NAME_FORMAT` names them, `{0}` being the genre or decade ("{0} Mix" when empty).

Mix and station covers are drawn by Octo in the same design as the Octo apps' playlist
covers: the list's name in white over one of 48 painted backgrounds, picked to match the
colours of the covers of its first songs (a station's seed artist first), or its genre's or
decade's colour when those give none. The same list keeps the same background while its
music does. Under the words the background is darkened only as far as white needs, keeping
its colour. Names are set in [Inter](https://github.com/rsms/inter) 4.1 (SIL Open Font
License, shipped as `licenses/Inter-OFL.txt`); names in scripts Inter lacks use Noto CJK,
DejaVu or Symbola from the image. No cover carries an Octo mark. A picture in
`/app/config/covers` named after a mix or station (`Rock Mix.jpg`), or after its genre or
decade (`Rock.png`), replaces its cover, and replacing the picture shows without a restart.

### Download path on Windows and manual installs

`DOWNLOAD_PATH` in `.env` is a HOST path: it is bind-mounted as `/music` into the octo, yt-dlp-shim, and slskd containers, and it is the only path you change to move the library. Container-side settings (Octo's `Library__DownloadPath`, slskd's downloads dir) stay `/music`.

- **Windows (Docker Desktop)**: use forward slashes, e.g. `DOWNLOAD_PATH=E:/Media/Music`. Do not put a drive-letter path in the admin UI's download path field; that field is a path inside the container.
- **Manual installs** (not using the bundled compose file): slskd's `directories.downloads` must resolve to the same directory Octo's `Library:DownloadPath` points at, or Octo will never see finished downloads. Set it with the `SLSKD_DOWNLOADS_DIR` environment variable, and note that a value set in `slskd.yml` overrides that env var (slskd precedence: env vars < yaml).

### Existing Lidarr setup

Set `LIDARR_URL` and `LIDARR_API_KEY`, restart Octo, then open the **Lidarr** admin tab to test the connection and choose its root folder and profiles. Enable and position Lidarr under **Downloads → Heart download priority**. Octo does not install or configure Lidarr's indexers or download client.

The selected Lidarr root and Octo's effective Navidrome library root must expose the same underlying files. Their container paths may differ: Octo translates the imported path relative to the selected Lidarr root. For example, Lidarr `/data/music/Artist/Album/file.flac` can map to Octo `/music/Artist/Album/file.flac` when both mounts point at the same host directory.

A heart through Lidarr keeps the same promises as one through Soulseek. Lidarr brings the whole album, so a song already in your library is deleted from what it brought (an MP3 you own is queued for Better quality instead), a song removed with a library action stays removed, and an album Octo had Lidarr monitor goes back to unmonitored once it lands, so Lidarr does not fetch the songs Octo moved into its own layout again. Every song Lidarr imports then goes through the same pipeline as a Soulseek download: AcoustID (a wrong recording, or a live take you did not ask for, is deleted), the spectrum check, release matching, tags, ReplayGain, lyrics and your folder layout.

A heart waits for Lidarr's import, up to `LIDARR_IMPORT_TIMEOUT_SECONDS` (default 1800), and any song that did not land (Lidarr found nothing in time, or the file failed a check) goes to the next source in **Heart download priority**, which skips the songs that did. The wait never blocks playback or later hearts. `LIDARR_COMPLETION_MODE=Accepted` (default) sends a notice when Lidarr accepts the search and one for the album when an album heart lands; `Imported` also sends one when a track heart's album lands, and one when an import fails or times out.

### Playback and acquisition

Tracks already in your library play locally through Navidrome. Missing external results stream from YouTube. Playback does not acquire a permanent copy unless one of the two settings below is on; heart the song or album to run the configured source priority.

Set `WAIT_FOR_LOSSLESS_ON_PLAY=true` if you would rather the first play wait for the lossless file. It is off by default because a Soulseek fetch routinely takes minutes and most clients time out long before that, which looks like the play failing. The setting also changes what searches advertise for external tracks, so it needs a restart, and clients that cached earlier results should re-search after you change it.

`WAIT_FOR_SEARCH_DURATIONS` (on by default) has a search wait for the YouTube lengths of the top rows from outside your library before it answers, so the length shown is the length of the video that plays. Turned off, a new search answers a few seconds sooner and those rows show Deezer's length. Octo still finds the YouTube length afterwards, and apps that look the song up again when it starts playing show that one. The change takes effect without a restart.

Set `DOWNLOAD_ON_PLAY=true` to keep a copy of every song played from outside your library, radio included. The copy comes from the first song source in the heart download priority that Octo fetches itself (Soulseek or YouTube, never Lidarr), and playback still starts from YouTube at once. Only the start of a play counts, not a seek. Hearts always go ahead of these downloads, and at most one played song waits its turn: a song played while another is waiting is skipped, and tried again the next time it is played. Each one shows in the app's download list like a heart, and hearting a song that is already downloading this way makes it a heart. `LIDARR_ALBUM_ON_PLAY=true` hands the album of every played song to Lidarr, once per song until Octo restarts; one Lidarr turns down is tried again on the next play. Every hand-off makes Lidarr search all its indexers, so on radio it pulls in an album per song. Both are off by default and have switches under **Streams & hearts, Playing songs you don't own**.

Octo sends plays to Last.fm itself: songs from outside your library, which Navidrome has never heard of, and your library songs too (the dashboard's "Also send library plays", on by default). Octo also sends outside plays to ListenBrainz when a token is set. For Last.fm, paste your API key and its shared secret on the dashboard's Last.fm page (or set `LASTFM_API_KEY` and `LASTFM_API_SECRET`); Save checks both with Last.fm. Then press **Connect** next to a listener and allow access on last.fm while signed in as that person. The dashboard notices by itself when that is done, and for someone else you can copy the link and send it to them. Each listener has their own connection. If Navidrome is also linked to the same Last.fm, or an app scrobbles to Last.fm itself, turn that off (or turn off "Also send library plays") or plays count twice.

### Folder layouts

- `Flat` *(default)*: `Artist - Title.flac`.
- `ByArtist`: `Artist/Title.flac`.
- `Organized`: `Artist/Album/01 - Title.flac`. A track with no known album falls back to its own title as the folder. Existing files are never moved; this only affects new downloads.

A download is filed once it has been tagged, so the album Deezer finds for a track that arrived without one names its folder. When no album turns up anywhere (the source, Deezer or the file's own tags), the track is filed as a single under its title (`ALBUM_FROM_TITLE`, on by default) rather than joining the one `[Unknown Album]` Navidrome gives every album-less track; a compilation is left alone, since a hundred one-track albums would be worse. A collaboration's folder is named after its first artist, and only when MusicBrainz or Deezer says who that is: `Bizarrap, Rauw Alejandro` is filed under `Bizarrap/`, while `Earth, Wind & Fire` and `Tyler, The Creator` stay whole because every source names them whole. The full credit stays in the artist tag, and each artist also gets a value of their own in the `ARTISTS` tag, so Navidrome lists the track under every one of them. File names keep annotations that name a different recording, such as `(Live)`, `[Remix]` and `(feat. X)`, and drop only upload noise like `(Official Video)`. A file already at the chosen path is replaced only when it is provably the same song; anything else keeps both.

### Tags, covers and lyrics

Every download is matched before it is tagged. The fingerprint service (with verification on), the music database and Deezer each offer the releases the song could be from, and every candidate is scored against what was asked for and what the file is: the title and artist, the length, the album when the request named one, the file's own album and year when a peer tagged it, the ISRC and barcode, the fingerprint, the kind of release (a studio album over a single, a single over a compilation), how long after the recording's first release the pressing came out, and where the candidate came from. The result is a distance from 0 (the same) to 1 (nothing agrees), and three levels of confidence:

- **Strong** sets the album-level tags from the release that won, whatever the file or the catalog said.
- **Medium** does the same only when the fingerprint service or the music database backs the release; a Deezer or file-tag candidate only fills blanks, the way Deezer always did.
- **Low** fills blanks only and keeps the name that was asked for. Two pressings of different albums too close to call leave the recording certain and the album as it was.

A song whose request named an album (an album fetched whole, a Deezer album listing) keeps that album, its track number and its disc; the release only confirms it and lends its facts. A song that arrived without one goes under the first release of its recording (`PREFER_ORIGINAL_ALBUM`, on by default), even when the file was ripped from a compilation; off keeps the file's own album when it names one. The year is the recording's first release (`YEAR_FROM_ORIGINAL_RELEASE`, on), so a 2011 remaster of a 1991 album reads 1991, and `PREFERRED_COUNTRIES` (empty by default, for example `US, XW, GB`) breaks ties between pressings. `RELEASE_DETAILS_LOOKUP` (on) asks the music database once per download for the release's label, catalogue number, barcode, status and track ids, and searches it by name when the fingerprint named nothing; it needs no key.

The tag set a sure match writes is the one Picard writes and Navidrome reads, named per container: `ALBUM`, `DATE` (the year), `ORIGINALDATE` and `ORIGINALYEAR`, `LABEL`, `CATALOGNUMBER`, `BARCODE`, `ISRC`, `RELEASETYPE`, `RELEASESTATUS`, `RELEASECOUNTRY`, `MUSICBRAINZ_TRACKID` (the recording), `MUSICBRAINZ_RELEASETRACKID`, `MUSICBRAINZ_RELEASEGROUPID`, `MUSICBRAINZ_ARTISTID` for every credited artist, `MUSICBRAINZ_ALBUMARTISTID`, and `ACOUSTID_ID`. The ISRC has its own field now rather than a comment, and a comment the file arrived with is left alone. The album id is deliberately not written, because Navidrome groups albums by it before the album name and a track carrying it beside one without it splits an album; nor is `RELEASEDATE`, which would split an album whose tracks matched different pressings. A new ID3 tag is version 2.4; a tag a file arrived with keeps its version. Every track of an album fetched whole shares the release the first track settled on, so the album shows one label, one catalogue number and one year.

A peer tags a file for the release it ripped, so Octo cleans what the file arrived with before writing its own. The album id, `RELEASEDATE` (and a Vorbis `YEAR`, which Navidrome also reads as one) and `ALBUMVERSION` never stay on a download, for the same reason Octo never writes them; the one exception is a song joining an album folder that already carries them (the `Organized` layout), which gets that album's exact values so it lands in the same album. When the song is filed under another album than the one the file named (a compilation rip filed under the studio album, say), everything else the peer wrote about that release goes too: its barcode, label, catalog number, release ids, original date, album gain and its track and disc numbers. A field Octo writes is the only value under any of its names, so a peer's `ORGANIZATION` or `UPC` cannot stand beside Octo's `LABEL` or `BARCODE` as a second label or barcode. `DISCTOTAL` is written when the release says how many discs it has. Whether the words are explicit goes into `ITUNESADVISORY` (the `rtng` atom on MP4: 1 explicit, 2 the clean edit, 0 neither), which Navidrome shows as each song's explicit status; it comes from the Deezer hit whose ISRC is the downloaded file's (the one asked for stands in only when neither the file nor the fingerprint names one), and a file name that says clean always wins.

`REPLAYGAIN` (on) measures each download's loudness once, beside the lookups, and writes `REPLAYGAIN_TRACK_GAIN` and `REPLAYGAIN_TRACK_PEAK` by the ReplayGain 2.0 convention (-18 LUFS, true peak), which the Octo apps and most players use to even out volume; an album fetched whole gets `REPLAYGAIN_ALBUM_GAIN` and `REPLAYGAIN_ALBUM_PEAK` once the walk ends. `REPLAYGAIN_TIMEOUT_SECONDS` (45) caps the measurement; past it the download goes on without ReplayGain. Every lookup has its own cap too, and a timeout costs fields, never the download.

Under each row of **Fetched songs**, "How it was tagged" shows the release that won and how sure Octo is, every candidate it weighed with its biggest penalties, what each field was set to and from, the notes (a catalog that did not answer, a release lookup that timed out), the seconds each stage took and the measured loudness. **Try it on a song** (Tags & genres) runs the same identification on a file in the music folder (fingerprint and loudness included) or on an artist and title alone, and shows the same report without writing anything, including whether the song is explicit and what a download would take off the file. `TAG_REHEARSAL` (off) runs the matching on every real download and shows the report, but writes only what Octo wrote before, plus ReplayGain, the ISRC and the fingerprint id, which do not depend on the match: a way to watch it on real downloads before trusting it.

Cover art comes from a chain, and the largest cover found wins: Apple's full-size master of the same album (often 3000 px, taken only when the artist and album name match), the Cover Art Archive when a fingerprint named the release (`COVER_ART_ARCHIVE`), then the catalog's own cover, then Deezer, iTunes and Last.fm by name, and last the file's own art. A cover that is not square is a video thumbnail and counts as missing (`REPLACE_VIDEO_COVERS`); when nothing better turns up its centre square is used, which for a YouTube "Topic" upload is the real cover inside the letterbox. Each song carries the cover at 1500 px, or at the full size it was found with `FULL_SIZE_COVERS=true`. `COVER_FILE` also writes a full-size `cover.jpg` beside the file, only in the `Organized` layout and only in a folder the download created, because Navidrome ranks `cover.*` above embedded art and a new file in an existing album folder would change that album's cover. A `cover.jpg` Octo wrote itself gives way to a larger one later; one you put there never changes.

**Soft covers** (dashboard, Cover art) finds soft covers and replaces the ones you pick. A scan reads your songs only (album by album, using Navidrome's own song list), Octo's downloads or the whole library, and shows every album whose cover is smaller than the size you choose (or missing) as a wall of covers. Pick all of them or just some and find better covers: albums are matched at Apple in bulk by barcode (from the song's own tag, or from Deezer), 20 to 40 per request, and each tile then shows the larger cover found, with its true size, without downloading it yet. A cover that does not look like the album's current one (another edition, or another album with the same name) is marked Different art and left unpicked. Replace downloads Apple's master at up to 5000 px and puts it in every song and, if you ask, in a JPEG `cover.jpg` or `folder.jpg` beside it. A cover is replaced only when the new one is clearly larger, and every cover replaced is kept so Undo puts them all back.

`LYRICS_FETCH` (off by default) writes lyrics beside each download, looked up in the background so a slow service never holds up the next download, and answers `getLyricsBySongId` live for any song as it plays, external songs included. Synced lyrics go in a `.lrc` and plain ones in a `.txt` with the audio file's name, both of which Navidrome reads at request time without a rescan; an instrumental gets nothing, and a file that already has lyrics is never touched. `LYRICS_SOURCES` sets the order: `song` (the lyrics a song already has, in its tags or a file beside it, which cannot be switched off and comes first when it is not listed), `kugou` (timed word by word for most songs, but an unofficial API that can change without notice), `lrclib` (open, timed line by line), `lyricsovh` (plain text), and `netease`, which goes much deeper on non-Western and older music but is also an unofficial API, so it only runs when you list it. Leave a source out and it is never contacted. A song's own lyrics rank like any source, so with `kugou` above `song`, a song whose tags hold line-timed lyrics plays KuGou's word-timed ones; with `LYRICS_PREFER_WORD_TIMED` on, word timing wins wherever it ranks. `LYRICS_SAVE_TO` says where found lyrics are saved: `beside` (the default), `inside` the song's tags (which rewrites the audio file, and shows after a Navidrome scan that Octo asks for), or `both`. What Octo writes is marked `[re:Octo]`, and lyrics Octo did not write are never replaced. The order was chosen by measurement: see [docs/lyrics-source-eval.md](docs/lyrics-source-eval.md). Every source is held to the same rule before its lyrics are used: the same title (a remix or a live take never stands in for the original), the same artist, and a length within three seconds. Credits at the top of a lyric are stripped.

Word timing reaches every client that can use it. A `.lrc` with word timing is enhanced LRC: each line keeps its standard `[mm:ss.xx]` tag, so any player shows it line by line, and `<mm:ss.xx>` tags time the words, which Navidrome turns into OpenSubsonic word cues. `getLyricsBySongId` answers with the same cues for the songs Octo answers itself, but only when the client asks for them with `enhanced=true`; a client that does not ask gets exactly the lines it always got. `LYRICS_PREFER_WORD_TIMED` (on by default) lets a later source with word timing win over an earlier one with only line timing. The legacy `getLyrics` call (artist and title) gets the same lookup as plain text.

The dashboard's **Lyrics** page works like the soft covers wall. **Scan** reads your songs (Octo's downloads or the whole library) and lists the ones with no lyrics, plain lyrics, or lyrics timed only by line; it looks nothing up and changes nothing. **Find better lyrics** looks the picked songs up in the source order, a second and a half apart, and shows what it found with its first lines; a match the sources were not sure of is shown but not picked. **Save** writes them where `LYRICS_SAVE_TO` says. Octo replaces only lyrics it wrote: better lyrics for a song with someone else's go beside it as a `.lrc`, which Navidrome serves ahead of the tags, and the originals stay. Everything Save writes over is kept, so **Undo** puts it back. A lyrics pin follows its song by artist and title, so it survives a file being replaced by a better copy. **Fix a song's lyrics** chooses other lyrics for one song, hides them, or goes back to automatic, for every app at once. The Octo app does the same through the `octoLyrics` extension (`getLyricsCandidates`, `setLyricsChoice`).

### Subsonic API surface

Octo hijacks these endpoints; everything else proxies to Navidrome unchanged:

| Endpoint | Why |
|---|---|
| `search3` | merge local + Last.fm-driven external songs and Deezer-driven external albums; later pages carry on through the outside songs page one started |
| `getSimilarSongs2` | radio queue with local-first preference |
| `getPlaylists`, `getPlaylist` | append authenticated per-user read-only Radio snapshots and materialize tracks local-first |
| `createPlaylist`, `updatePlaylist`, `deletePlaylist` | protect reserved Radio IDs while relaying ordinary mutations |
| `getInternetRadioStations` | append startup-warmed authenticated Octo stations immediately, with a one-starter same-request fallback, while preserving ordinary internet radio |
| `createInternetRadioStation`, `updateInternetRadioStation`, `deleteInternetRadioStation` | protect Octo stations while relaying ordinary internet-radio mutations |
| `/radio/stream/{token}` | consume the ready MP3 pool, optionally frame its existing artist/title as client-requested ICY metadata, and replenish it until disconnect |
| `stream` | YouTube proxy with Range support, mp4/m4a passthrough |
| `getCoverArt` | Deezer → iTunes → Last.fm aggregator with Octo watermark |
| `getArtist` | an artist's albums, EPs and singles from Deezer beside the ones you own, each with its OpenSubsonic `releaseTypes` |
| `getAlbum` | external album tracklists, and fills in tracks you're missing from an album you own |
| `star` | try enabled heart sources in priority order and stop after the first successful track/album acquisition |
| `scrobble` | relay library plays to Navidrome, send plays to Last.fm (library ones too unless left to Navidrome) and outside plays to ListenBrainz, prewarm the next 8, and learn deduplicated completed plays for the authenticated user |
| `getTranscodeDecision` | OpenSubsonic: return direct-play for Octo IDs |
| `getLyricsBySongId`, `getLyrics` | lyrics for outside songs and for library songs Navidrome has none for; chosen or hidden lyrics for every client; word cues with `enhanced=true` |
| `getLyricsCandidates`, `setLyricsChoice` | the `octoLyrics` extension: every lyrics entry for a song, and pinning one, hiding lyrics, or going back to automatic |
| `getLibraryActions`, `libraryAction`, `getLibraryTrash` | the `octoLibraryActions` extension: what the caller may do to library files, removing one song the way the Delete playlist does, putting it back, and the tag, album and cover fixes of Library health |
| `getAcquisitions`, `getAcquisition`, `clearAcquisitions` | the `octoAcquisitions` extension: the caller's downloads; version 2 adds each one's log and clearing finished ones |
| `findSongs`, `getFoundSongs`, `pickFoundSong` | `octoAcquisitions` 2: a song's search run again on the caller's download sources, every copy found, and fetching the one picked |
| `/api/artist/{id}`, `/api/album?artist_id=` | Navidrome's own API, for clients that use it (Feishin): an outside artist's page and its albums |
| `getOpenSubsonicExtensions` | Navidrome's list plus `octoAcquisitions` 1 and 2, `octoLyrics` (while lyrics lookups are on), `octoLibraryActions` (while library actions are on) and `songLyrics` 1 and 2 |

### Soulseek download details

When a song is starred, Octo:

1. Searches Soulseek for `<artist> <title>` (cleaned of `[brackets]` and redundant `Artist - ` prefixes), reading up to 2,000 files, as Better quality does. A popular song can fill that with its fastest peers' MP3s in a second; when nothing usable came back and the search stopped at its limit, the same search runs once more, four times as wide.
2. Falls back to title-only search if the first query returns nothing usable, then to `<artist> <album>` for peers who name files by number and title only.
   When every query finds the song only lossy (an MP3 where FLAC is preferred), Octo asks the best three of those peers for the folder that MP3 sits in, and takes a FLAC of the same song from beside it: albums are often shared in both formats, and only one answered the search. Those files go through every check below, like any search hit.
3. Keeps only the version you asked for and ranks the rest by queue depth, upload speed, file size. A file name needs the song's title, not its guest credit or version tag, so "Take Care (feat. Rihanna)" finds a plain `04 - Take Care.flac`. The song itself never takes a radio edit, clean, sped up, extended, remix or live copy, whether the file name or its album folder says so ("Too Close (Radio Edit) - Single"). A request for a version gets that version: a copy named for it goes first, and a radio edit may also be a plainly named file of its length.
4. Tries the top 5 peers in sequence. A peer that keeps sending is waited for however slow it is (up to an hour); one that sends nothing for 180 seconds by default (`SLSKD_DOWNLOAD_TIMEOUT_SECONDS`) is cancelled in slskd, so its file can never land later as a second copy, and the next peer is tried.
5. Verifies the file landed on disk (slskd's polling endpoint sometimes drops successful transfers between polls).
6. Renames per `FolderStructure` setting and triggers a Navidrome rescan.

Around 30 to 50% of Soulseek peer requests get rejected ("overwhelmed", queue full, banned). Single-peer-try downloads were too fragile; multi-peer is the difference between "downloads sometimes work" and "downloads reliably work."

Before any of that, Octo checks whether the song is already in your library (Navidrome's own search, the same artist and title in the same version, a length within 8 seconds or the same album). A lossless copy is kept and nothing is downloaded; a lossy one is kept and queued for a higher quality copy when Soulseek or Lidarr can look for one and Better quality is on for you (`SKIP_OWNED_SONGS`, on by default). A heart through Lidarr follows the same rule: Lidarr brings the whole album, and the songs you already have are deleted from what it brought.

Each Soulseek download lands in a hidden folder of its own (`.octo-incoming/slskd/<id>` in slskd's downloads folder), so Octo always finds exactly its own file. Once slskd has shown it honours that folder, up to `SLSKD_PARALLEL_DOWNLOADS` (default 3, at most 6) downloads transfer at once; moving files into your library stays one at a time. An slskd without batch downloads, or one whose download subfolder setting is `{}`, keeps Octo at one at a time.

Starring an album searches the album once and takes one person's folder of it in a single batch, matched to the tracklist by title, length and track number. Songs that folder lacks, or whose file fails a check, are searched one by one, side by side (`SLSKD_ALBUM_FOLDERS`, on by default).

When slskd is up but not logged in to the Soulseek network (Soulseek's server has maintenance now and then), the dashboard shows it as a warning, and hearts wait up to `SLSKD_OUTAGE_HOLD_HOURS` (default 6) for it before using the next source, so a maintenance window does not turn everything you heart into YouTube MP3s. Waiting hearts survive a restart.

The dashboard's **Better quality** page lists every song in your library that is not lossless, including the ones Octo got from YouTube, and finds a higher quality copy of the ones you pick, several at a time. Octo's apps offer the same as **Find higher quality** on a song or an album. Both go through the Better quality library action, so they need library actions on, the Better quality action on, you on the allowed list, and rehearsal mode off.

Copies come from Soulseek, from Lidarr, or from both: **Library actions → Where to look for a higher quality copy** (`LIBRARY_ACTIONS_UPGRADE_SOURCE`). Automatic, the default, asks Soulseek first and Lidarr for what Soulseek cannot find, using whichever is set up, and asks Lidarr alone while Soulseek is offline. Lidarr only fetches whole albums, so Octo borrows the album: it copies out the one song, which then goes through the same checks and the same in-place swap as a Soulseek copy, deletes the other files that search brought in, and puts the album's monitoring back as it was. A song whose file Lidarr itself manages is left to Lidarr, which upgrades it in place when its quality profile asks for lossless.

> **A heart is "fetch" for a song you do not have, and "favorite" for one you do.** Octo checks your library first. A song you already have, even the copy Octo found outside your library, becomes your favorite in Navidrome straight away and downloads nothing (an MP3 is queued for Better quality). A song you do not have is downloaded and nothing more: once it is in your library, heart it there to make it a favorite. `STAR_DOWNLOADS_FOR_REQUESTER` makes Octo favorite downloads when they land instead. Octo's own apps are left out of both, since their heart means Add.

### Sharing back on Soulseek

Soulseek only works because people share. Many users will not send files to someone who shares nothing. So the bundled compose file has slskd share your music library back:

- **Read-only.** `DOWNLOAD_PATH` is mounted into slskd a second time, at `/share`, read-only, and that is the folder slskd shares. An upload only ever reads a file. Other people see it as a folder named `Music`, never your own path.
- **Nothing private.** Octo's working folders (`.octo-incoming`, `.octo-trash`) are hidden folders, which slskd skips, and a share filter keeps out Octo's temporary and partial files. slskd's own unfinished downloads live in `slskd-state/incomplete`, which is not shared. To keep a folder of your own out, add it to `SLSKD_SHARED_DIR` with a `!` in front, for example `[Music]/share;!/share/Voice Memos`.
- **Polite limits.** 4 uploads at a time and 2048 KiB/s (about 16 Mbit/s) in all, so sharing never crowds out streaming from your server (`SLSKD_UPLOAD_SLOTS`, `SLSKD_UPLOAD_SPEED_LIMIT`). Raise them if you have upload to spare.
- **New songs are shared too.** slskd looks through the shared folders once a day (`SLSKD_SHARE_RESCAN_MINUTES`, 60 or more). Every look reads each file's header, so a library on a cloud drive is better at 10080, once a week. The dashboard's **Rescan shared folders** button looks straight away.

**Forward TCP port 50300** on your router to the machine running Octo. That is the port other people connect to, to download from you; without it only people whose own port is open can reach you, and some of your own downloads fail too. Never forward 5030: that is slskd's own web page. If you change slskd's listening port, forward that port instead, and publish the same number in the compose file.

The dashboard's **Soulseek** page has a **Sharing** card: the folders and files shared, uploads now and over the last 7 days, the upload limits, and whether the port is open. It warns when you share nothing, when a shared folder turns out empty, when the port is closed, and when slskd still has its default sign-in. The port test is Soulseek's own (`tools.slsknet.org`): Octo sends it the port number and nothing else, at most every 6 hours unless you press **Test the port**, and `SLSKD_CHECK_PORT=false` stops it. The test checks the address Octo's request comes from, so it is only right when slskd reaches the internet the same way, not through a VPN of its own.

**Open slskd** at the top of that page opens slskd's own web page in a new tab, at this server's name on port 5030, or at `SLSKD_WEB_URL` when you reach slskd some other way. slskd asks for its own sign-in (`SLSKD_USERNAME` and `SLSKD_PASSWORD` in `.env`). Octo never hands its slskd login to the browser: the dashboard shows the slskd password as a placeholder, like the Navidrome admin password.

A share list in `slskd-state/slskd.yml` outranks `SLSKD_SHARED_DIR` (slskd reads its file after its environment), so if you set shares up there yourself, those stay.

### Cover art aggregator

Three sources tried in order; first hit wins:

1. **Deezer**: broad international catalog, picks 1000×1000 covers.
2. **iTunes**: limit=5, scored by artist match (avoids "Karaoke Version" hits).
3. **Last.fm**: track-level images, skips the deprecated artist-image placeholder.

Cached cross-source so a queue scroll doesn't trigger N external API calls per visible song.

### FAQ

**Do downloaded songs get tagged?**
Yes. Every download is matched against the fingerprint service, the music database and Deezer, tagged with the full release set and ReplayGain, given the largest cover found, and filed per `FolderStructure` before the Navidrome rescan. See [Do downloaded songs get tagged correctly?](#do-downloaded-songs-get-tagged-correctly) above.

**What if all 5 Soulseek peers reject?**
The next source in your heart order is tried. If every one fails and notifications are set up, you get a **Download failed** message; your music app itself hears nothing, because the heart was answered straight away. The heart may clear on the app's next sync, since Navidrome never stored a favorite for a song it doesn't have. Try again later or grab the file by hand.

**Can it run without Soulseek?**
Yes. Enable YouTube for MP3 downloads, Lidarr for album-level heart acquisition, or disable every song-heart source to keep discovery without automatic acquisition.

**Can it run without Last.fm?**
Yes. Existing snapshots are served first; Starter and pinned stations can fall back to accessible local seeds/genres, but fresh external discovery is degraded. The Last.fm pane reports that state explicitly.

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
| `octo/Services/LastFm/` | Last.fm client, Radio state/recommendations, in-process refresh queue and worker |
| `octo/Services/Subsonic/` | request parsing, response building |
| `octo/Services/Admin/` | settings file writer (atomic, deep-merge) |
| `octo/wwwroot/admin/` | the admin UI (vanilla JS, hand-rolled CSS, no build step) |
| `yt-dlp-shim/` | Python/Flask sidecar (~200 lines) |

</details>

---

## License

[GPL-3.0](LICENSE)

## Acknowledgments

- [**Navidrome**](https://www.navidrome.org/): the music server Octo proxies.
- [**slskd**](https://github.com/slskd/slskd): Soulseek with a REST API.
- [**Lidarr**](https://github.com/Lidarr/Lidarr): optional album acquisition and import manager.
- [**yt-dlp**](https://github.com/yt-dlp/yt-dlp): makes YouTube preview feasible.
- [**Last.fm**](https://www.last.fm/api): similar-tracks API.
- [**V1ck3s/octo-fiesta**](https://github.com/V1ck3s/octo-fiesta): the upstream root of this lineage. The Qobuz/Deezer/Yandex Subsonic-proxy concept that Octo eventually rebuilt around YouTube + Soulseek started here.
- [**bransoned/octo-fiestarr**](https://github.com/bransoned/octo-fiestarr): the intermediate fork of octo-fiesta whose codebase Octo's earliest commits descended from.
