#!/usr/bin/env bash
# Octo installer.
# Prompts for required configuration, validates inputs, writes .env, and brings
# the stack up. Idempotent — re-running offers to keep existing values.
set -euo pipefail

cd "$(dirname "$0")"

# ─────────────────────────────────────────────────────────────────
# Helpers
# ─────────────────────────────────────────────────────────────────
bold()  { printf "\033[1m%s\033[0m\n" "$*"; }
green() { printf "\033[32m%s\033[0m\n" "$*"; }
red()   { printf "\033[31m%s\033[0m\n" "$*"; }
yellow() { printf "\033[33m%s\033[0m\n" "$*"; }
dim()   { printf "\033[2m%s\033[0m\n" "$*"; }

ask() {
  local prompt="$1" default="${2-}" reply
  if [ -n "$default" ]; then
    read -rp "$prompt [$default]: " reply
    echo "${reply:-$default}"
  else
    read -rp "$prompt: " reply
    echo "$reply"
  fi
}

ask_secret() {
  local prompt="$1" default="${2-}" reply
  if [ -n "$default" ]; then
    read -rsp "$prompt [keep existing]: " reply; echo >&2
    echo "${reply:-$default}"
  else
    read -rsp "$prompt: " reply; echo >&2
    echo "$reply"
  fi
}

ask_yn() {
  local prompt="$1" default="${2:-n}" reply
  while true; do
    read -rp "$prompt [y/n] (default: $default): " reply
    reply="${reply:-$default}"
    case "$reply" in
      [yY]|[yY][eE][sS]) return 0;;
      [nN]|[nN][oO])     return 1;;
      *) red "  please answer y or n";;
    esac
  done
}

random_password() {
  if command -v openssl >/dev/null 2>&1; then
    openssl rand -base64 18 | tr -d '/+=' | cut -c1-24
  else
    LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom | head -c 24
  fi
}

# Resolve a path argument to absolute (handle ~, relative, missing-trailing-slash).
abs_path() {
  local p="$1"
  # Expand leading ~
  case "$p" in "~"*) p="${HOME}${p#~}";; esac
  # If it exists, use realpath; otherwise compose with $PWD.
  if [ -e "$p" ]; then
    (cd "$p" 2>/dev/null && pwd) || readlink -f "$p" 2>/dev/null || echo "$p"
  else
    case "$p" in /*) echo "$p";; *) echo "$PWD/${p#./}";; esac
  fi
}

# ─────────────────────────────────────────────────────────────────
# Prereq: Docker + Compose v2
# ─────────────────────────────────────────────────────────────────
require_docker() {
  if ! command -v docker >/dev/null 2>&1; then
    red "Docker isn't installed. Install Docker Engine first:"
    echo "    https://docs.docker.com/engine/install/"
    exit 1
  fi
  if ! docker info >/dev/null 2>&1; then
    red "Docker is installed but the daemon isn't running, or your user can't reach it."
    echo "  • Linux: start the daemon:   sudo systemctl start docker"
    echo "  • Linux: add yourself to the docker group:   sudo usermod -aG docker \$USER  (then log out + back in)"
    echo "  • Mac/Win: open Docker Desktop and wait for it to finish starting"
    exit 1
  fi
  if ! docker compose version >/dev/null 2>&1; then
    red "Docker Compose v2 isn't available."
    if command -v docker-compose >/dev/null 2>&1; then
      yellow "  You have the old 'docker-compose' (v1). Octo needs the integrated 'docker compose' (v2)."
      echo  "  Update Docker Engine — v2 ships built in:   https://docs.docker.com/compose/install/"
    else
      echo "  Install instructions:   https://docs.docker.com/compose/install/"
    fi
    exit 1
  fi
}

# ─────────────────────────────────────────────────────────────────
# Validation probes (non-fatal — warn but continue)
# ─────────────────────────────────────────────────────────────────
probe_navidrome() {
  local url="$1"
  local code
  code=$(curl -sS -m 5 -o /dev/null -w '%{http_code}' "${url%/}/rest/ping?u=probe&p=probe&v=1.16.1&c=octo-installer&f=json" 2>/dev/null || echo "000")
  if [ "$code" = "200" ]; then
    green "  ✓ reached Navidrome at $url"
    return 0
  elif [ "$code" = "000" ]; then
    yellow "  ⚠ couldn't reach $url — check the URL is right and Navidrome is running"
    return 1
  else
    yellow "  ⚠ got HTTP $code from $url — URL is reachable but doesn't look like Navidrome"
    return 1
  fi
}

probe_lastfm() {
  local key="$1"
  [ -z "$key" ] && return 0
  local body
  body=$(curl -sS -m 5 "https://ws.audioscrobbler.com/2.0/?method=track.getInfo&artist=cher&track=believe&api_key=$key&format=json" 2>/dev/null || echo "")
  if echo "$body" | grep -q '"track"'; then
    green "  ✓ Last.fm API key works"
    return 0
  elif echo "$body" | grep -q '"error":10'; then
    yellow "  ⚠ Last.fm rejected that key (invalid). Discovery + radio will fall back to local-only."
    return 1
  else
    yellow "  ⚠ couldn't validate Last.fm key (network issue?). Continuing anyway."
    return 1
  fi
}

# ─────────────────────────────────────────────────────────────────
# Load existing .env if present so re-runs preserve values
# ─────────────────────────────────────────────────────────────────
# EXISTING holds each value with its quotes taken off, for the questions below.
# OLD_LINE holds each line exactly as it was, so the rewrite can put it back.
declare -A EXISTING OLD_LINE
OLD_KEYS=()
if [ -f .env ]; then
  while IFS= read -r line || [ -n "$line" ]; do
    line="${line%$'\r'}"
    [[ "$line" =~ ^([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]] || continue
    k="${BASH_REMATCH[1]}"; v="${BASH_REMATCH[2]}"
    v="${v%\"}"; v="${v#\"}"
    [ -n "${OLD_LINE[$k]+set}" ] || OLD_KEYS+=("$k")
    EXISTING[$k]="$v"
    OLD_LINE[$k]="$line"
  done < .env
fi
# The old value of a setting, or the default given when the old .env had none.
existing() { local v="${EXISTING[$1]-}"; echo "${v:-${2-}}"; }
# A whole .env line for a setting the installer does not ask about: the old line
# when there was one, so a value changed by hand survives a re-run.
setting() {
  if [ -n "${OLD_LINE[$1]+set}" ]; then echo "${OLD_LINE[$1]}"; else echo "$1=$2"; fi
}

# ─────────────────────────────────────────────────────────────────
# Run
# ─────────────────────────────────────────────────────────────────
clear 2>/dev/null || true   # a terminal without the capability (TERM=dumb, CI) must not end the install
bold "═══════════════════════════════════════════════════════════"
bold "  Octo · installer"
bold "═══════════════════════════════════════════════════════════"
echo
echo "Sets up your own music service in one Docker Compose stack: Octo"
echo "(search, radio, previews and downloads), a yt-dlp shim, slskd, and, if"
echo "you don't run one yet, a Navidrome music server."
echo
echo "What you'll need handy:"
echo "  • Your Navidrome address, if you already run one"
echo "  • A free Last.fm API key: https://www.last.fm/api/account/create"
echo "  • A free Soulseek (slsknet.org) account"
echo "  • Optionally, an existing Lidarr server"
echo
require_docker
green "✓ Docker + Compose v2 ready"
echo

# ─────────────────────────────────────────────────────────────────
# Required: Navidrome URL + music directory
# ─────────────────────────────────────────────────────────────────
bold "─── Your music server ──────────────────────────────────────"
echo "  Octo plays your library through Navidrome, a self-hosted music server."
echo "  Already run one? Octo sits in front of it. If not, Octo starts one for"
echo "  you in the same stack, reading the same music folder."
HAVE_NAVIDROME_DEFAULT="n"
if [ -n "$(existing SUBSONIC_URL)" ] && [ "$(existing SUBSONIC_URL)" != "http://navidrome:4533" ]; then
  HAVE_NAVIDROME_DEFAULT="y"
fi
STARTER=false
COMPOSE_PROFILES=""
NAVIDROME_ADMIN_PASSWORD=""
SUBSONIC_ADMIN_USERNAME="$(existing SUBSONIC_ADMIN_USERNAME)"
SUBSONIC_ADMIN_PASSWORD="$(existing SUBSONIC_ADMIN_PASSWORD)"
if ask_yn "  Do you already run Navidrome?" "$HAVE_NAVIDROME_DEFAULT"; then
  echo
else
  STARTER=true
fi
echo

if [ "$STARTER" = true ]; then
  SUBSONIC_URL="http://navidrome:4533"
  COMPOSE_PROFILES="navidrome"
  # Creates Navidrome's "admin" user on its first start; kept on re-runs so it
  # always matches what Navidrome already has.
  NAVIDROME_ADMIN_PASSWORD="$(existing NAVIDROME_ADMIN_PASSWORD)"
  if [ -z "$NAVIDROME_ADMIN_PASSWORD" ]; then
    NAVIDROME_ADMIN_PASSWORD="$(random_password)"
    green "  ✓ generated a password for Navidrome's admin user (saved in .env)"
  fi
  SUBSONIC_ADMIN_USERNAME="admin"
  SUBSONIC_ADMIN_PASSWORD="$NAVIDROME_ADMIN_PASSWORD"
  green "  ✓ Navidrome will start beside Octo"
else
while true; do
  SUBSONIC_URL=$(ask "Navidrome URL" "$(existing SUBSONIC_URL "http://192.168.1.10:4533")")
  # localhost trap: containers can't reach the host's loopback by default
  if [[ "$SUBSONIC_URL" =~ ^https?://(localhost|127\.0\.0\.1) ]]; then
    yellow "  ⚠ 'localhost' inside the Octo container won't reach Navidrome on the host."
    echo  "    Use your machine's LAN IP (e.g. http://192.168.1.10:4533) or the special host:"
    echo  "      • Mac/Windows: http://host.docker.internal:4533"
    echo  "      • Linux:       use the LAN IP, or add 'extra_hosts' to compose"
    if ask_yn "  Continue with that URL anyway?" "n"; then break; fi
    echo
    continue
  fi
  if probe_navidrome "$SUBSONIC_URL"; then break; fi
  if ask_yn "  Continue with this URL anyway?" "n"; then break; fi
  echo
done
fi
echo

DOWNLOAD_PATH_RAW=$(ask "Music directory on this host (where downloads will land)" \
  "$(existing DOWNLOAD_PATH "./downloads")")
DOWNLOAD_PATH=$(abs_path "$DOWNLOAD_PATH_RAW")
if [ "$DOWNLOAD_PATH" != "$DOWNLOAD_PATH_RAW" ]; then
  dim "  resolved to absolute: $DOWNLOAD_PATH"
fi
mkdir -p "$DOWNLOAD_PATH" 2>/dev/null || {
  red "  Couldn't create $DOWNLOAD_PATH — pick a location you can write to (or run with sudo)."
  exit 1
}
if [ ! -w "$DOWNLOAD_PATH" ]; then
  red "  $DOWNLOAD_PATH isn't writable. Pick a different location or fix permissions."
  exit 1
fi
green "  ✓ $DOWNLOAD_PATH is ready"
echo

# ─────────────────────────────────────────────────────────────────
# Last.fm
# ─────────────────────────────────────────────────────────────────
bold "─── Last.fm (powers radio + discovery) ─────────────────────"
echo "  Get a free key in 30 seconds: https://www.last.fm/api/account/create"
echo "  (leave blank to skip — search/radio will fall back to local-only)"
LASTFM_API_KEY=$(ask_secret "Last.fm API key" "$(existing LASTFM_API_KEY)")
[ -n "$LASTFM_API_KEY" ] && probe_lastfm "$LASTFM_API_KEY" || true
echo

# ─────────────────────────────────────────────────────────────────
# Soulseek
# ─────────────────────────────────────────────────────────────────
bold "─── Soulseek (downloads when you star a song) ──────────────"
echo "  Sign up free at https://www.slsknet.org/news/node/1"
echo "  These are your Soulseek-network credentials — slskd uses them to log in."
SLSKD_SOULSEEK_USERNAME=$(ask "Your Soulseek username" "$(existing SLSKD_SOULSEEK_USERNAME)")
SLSKD_SOULSEEK_PASSWORD=$(ask_secret "Your Soulseek password" "$(existing SLSKD_SOULSEEK_PASSWORD)")
echo
echo "  Soulseek runs on sharing: many users will not send files to someone who"
echo "  shares nothing. slskd can share your music folder back, read-only, with"
echo "  4 uploads at a time. You can turn it on or off later on the dashboard's"
echo "  Soulseek page."
SHARE_DEFAULT="y"
[ "$(existing SLSKD_SHARE_LIBRARY)" = "false" ] && SHARE_DEFAULT="n"
if ask_yn "  Share your music library on Soulseek?" "$SHARE_DEFAULT"; then
  SLSKD_SHARE_LIBRARY=true
  echo "  Forward TCP port 50300 on your router to this machine so people can"
  echo "  connect. Never forward 5030."
else
  SLSKD_SHARE_LIBRARY=false
fi
echo

# ─────────────────────────────────────────────────────────────────
# Optional Lidarr heart source
# ─────────────────────────────────────────────────────────────────
bold "─── Heart download source ──────────────────────────────────"
echo "  Soulseek — individual lossless tracks (default)"
echo "  Lidarr   — your existing Lidarr server; always fetches the full album"
DOWNLOAD_SOURCE=$(ask "Heart download source" "$(existing DOWNLOAD_SOURCE "Soulseek")")
LIDARR_URL="$(existing LIDARR_URL)"
LIDARR_API_KEY="$(existing LIDARR_API_KEY)"
LIDARR_ROOT_FOLDER_PATH="$(existing LIDARR_ROOT_FOLDER_PATH)"
LIDARR_QUALITY_PROFILE_ID="$(existing LIDARR_QUALITY_PROFILE_ID "0")"
LIDARR_METADATA_PROFILE_ID="$(existing LIDARR_METADATA_PROFILE_ID "0")"
LIDARR_COMPLETION_MODE="$(existing LIDARR_COMPLETION_MODE "Accepted")"
LIDARR_IMPORT_TIMEOUT_SECONDS="$(existing LIDARR_IMPORT_TIMEOUT_SECONDS "1800")"
if [ "${DOWNLOAD_SOURCE,,}" = "lidarr" ]; then
  echo "  Lidarr must already have working indexers and a download client."
  LIDARR_URL=$(ask "Lidarr URL (reachable from the Octo container)" "$LIDARR_URL")
  LIDARR_API_KEY=$(ask_secret "Lidarr API key" "$LIDARR_API_KEY")
  dim "  Choose the Lidarr root folder and profiles in Octo's admin UI after startup."
fi
echo

# ─────────────────────────────────────────────────────────────────
# Storage / layout (keep the simple defaults visible)
# ─────────────────────────────────────────────────────────────────
bold "─── Storage / layout ───────────────────────────────────────"
echo "  Stream     — preview only; star a song to download (recommended)"
echo "  Permanent  — download every song you play"
echo "  Cache      — temporary, auto-cleanup"
STORAGE_MODE=$(ask "Storage mode" "$(existing STORAGE_MODE "Stream")")
echo
echo "  Flat       — Artist - Title.flac (no subfolders, easier to browse)"
echo "  Organized  — Artist/Title/file.flac"
FOLDER_STRUCTURE=$(ask "Folder layout" "$(existing FOLDER_STRUCTURE "Flat")")
echo

# slskd web UI admin — auto-generate on first run, preserve on re-run
SLSKD_USERNAME="$(existing SLSKD_USERNAME "admin")"
SLSKD_PASSWORD="$(existing SLSKD_PASSWORD)"
if [ -z "$SLSKD_PASSWORD" ]; then
  SLSKD_PASSWORD="$(random_password)"
  green "  ✓ generated random slskd web admin password (saved in .env)"
fi

# ─────────────────────────────────────────────────────────────────
# Write .env
# ─────────────────────────────────────────────────────────────────
echo
bold "─── Writing .env ───────────────────────────────────────────"
cat > .env <<EOF
# Generated by install.sh — re-run the script to update values.
# The admin UI at http://<host>:5274/admin/ can also edit settings live.
# A re-run keeps every line here it does not ask about, including ones you add.

# === Required ===
SUBSONIC_URL=$SUBSONIC_URL
DOWNLOAD_PATH=$DOWNLOAD_PATH
# Navidrome admin login Octo uses for rescans and its own checks.
SUBSONIC_ADMIN_USERNAME=$SUBSONIC_ADMIN_USERNAME
SUBSONIC_ADMIN_PASSWORD="$SUBSONIC_ADMIN_PASSWORD"

# === Starter stack (Navidrome started beside Octo) ===
COMPOSE_PROFILES=$COMPOSE_PROFILES
NAVIDROME_ADMIN_PASSWORD="$NAVIDROME_ADMIN_PASSWORD"

# === Last.fm ===
LASTFM_API_KEY=$LASTFM_API_KEY
$(setting LASTFM_ENABLE_RADIO true)
$(setting LASTFM_RADIO_TRACK_COUNT 50)
$(setting LASTFM_RADIO_CACHE_HOURS 24)
$(setting LASTFM_ENABLE_PERSONALIZED_STATIONS true)
$(setting LASTFM_ENABLE_DISCOVERY_STATIONS true)
$(setting LASTFM_EXPOSE_AS_PLAYLISTS true)
$(setting LASTFM_EXPOSE_AS_STREAMS true)
$(setting LASTFM_RADIO_STREAM_BITRATE_KBPS 192)
$(setting LASTFM_HISTORY_RETENTION_DAYS 90)
$(setting LASTFM_DISCOVERY_PERCENT 35)
$(setting LASTFM_REFRESH_INTERVAL_HOURS 12)
$(setting LASTFM_MINIMUM_PLAYS 10)

# === Soulseek (slskd) ===
SLSKD_USERNAME=$SLSKD_USERNAME
SLSKD_PASSWORD=$SLSKD_PASSWORD
$(setting SLSKD_SEARCH_WAIT_SECONDS 6)
$(setting SLSKD_MIN_FILE_SIZE_BYTES 5242880)
$(setting SLSKD_PREFERRED_EXTENSION flac)
$(setting SLSKD_DOWNLOAD_TIMEOUT_SECONDS 180)
SLSKD_SOULSEEK_USERNAME=$SLSKD_SOULSEEK_USERNAME
SLSKD_SOULSEEK_PASSWORD="$SLSKD_SOULSEEK_PASSWORD"
SLSKD_SHARE_LIBRARY=$SLSKD_SHARE_LIBRARY

# === Existing Lidarr (optional) ===
LIDARR_URL=$LIDARR_URL
LIDARR_API_KEY=$LIDARR_API_KEY
LIDARR_ROOT_FOLDER_PATH=$LIDARR_ROOT_FOLDER_PATH
LIDARR_QUALITY_PROFILE_ID=$LIDARR_QUALITY_PROFILE_ID
LIDARR_METADATA_PROFILE_ID=$LIDARR_METADATA_PROFILE_ID
LIDARR_COMPLETION_MODE=$LIDARR_COMPLETION_MODE
LIDARR_IMPORT_TIMEOUT_SECONDS=$LIDARR_IMPORT_TIMEOUT_SECONDS

# === Storage / layout ===
STORAGE_MODE=$STORAGE_MODE
$(setting DOWNLOAD_MODE Track)
DOWNLOAD_SOURCE=$DOWNLOAD_SOURCE
$(setting DOWNLOAD_ON_STAR true)
$(setting DOWNLOAD_ALBUM_ON_STAR true)
$(setting WAIT_FOR_LOSSLESS_ON_PLAY false)
FOLDER_STRUCTURE=$FOLDER_STRUCTURE
$(setting USE_LOCAL_STAGING false)
$(setting EXPLICIT_FILTER All)
$(setting CACHE_DURATION_HOURS 1)
$(setting ENABLE_EXTERNAL_PLAYLISTS false)

# === yt-dlp shim (defaults are fine) ===
$(setting YTDLP_MAX_CONCURRENT 5)
$(setting YTDLP_SEARCH_CACHE_MAX 1024)
$(setting YTDLP_URL_CACHE_MAX 512)
$(setting YTDLP_URL_CACHE_TTL 3600)
EOF
# Every other line the old .env had goes back in as it was: settings this installer
# never writes (LASTFM_API_SECRET, OCTO_CONFIG_DIR, one added by a later release),
# so a re-run never drops something the user set.
kept=()
for key in "${OLD_KEYS[@]}"; do
  grep -q "^$key=" .env || kept+=("${OLD_LINE[$key]}")
done
if [ "${#kept[@]}" -gt 0 ]; then
  printf '\n# === Kept from your earlier .env ===\n' >> .env
  printf '%s\n' "${kept[@]}" >> .env
fi
chmod 600 .env
green "✓ wrote .env (chmod 600)"

# Make sure the bind-mount targets exist so docker doesn't create them root-owned.
mkdir -p "$(existing OCTO_CONFIG_DIR octo-config)" "$(existing SLSKD_STATE_DIR slskd-state)"
if [ "$STARTER" = true ]; then
  mkdir -p "$(existing NAVIDROME_DATA_DIR navidrome-data)"
fi

# ─────────────────────────────────────────────────────────────────
# Build + start
# ─────────────────────────────────────────────────────────────────
echo
bold "─── Building images ────────────────────────────────────────"
dim "  This is the slowest step — 2-3 minutes the first time, ~10 seconds on re-runs."
docker compose build

echo
bold "─── Starting stack ─────────────────────────────────────────"
docker compose up -d

# ─────────────────────────────────────────────────────────────────
# Wait for Octo + verify each backend
# ─────────────────────────────────────────────────────────────────
echo
echo -n "Waiting for Octo to come online "
for i in $(seq 1 60); do
  if curl -s -m 2 -o /dev/null -w '%{http_code}' "http://localhost:5274/api/admin/status" 2>/dev/null | grep -q '^2'; then
    echo
    green "✓ Octo responded on http://localhost:5274"
    break
  fi
  echo -n "."
  sleep 2
  if [ "$i" = "60" ]; then
    echo
    red "Octo did not respond within 2 minutes."
    echo "  Inspect:   docker compose logs octo"
    echo "  Restart:   docker compose down && docker compose up -d"
    exit 1
  fi
done

echo
bold "─── Service health ─────────────────────────────────────────"
status_json=$(curl -sS -m 5 "http://localhost:5274/api/admin/status" 2>/dev/null || echo "{}")
check_svc() {
  local name="$1" key="$2"
  if echo "$status_json" | grep -q "\"$key\":{\"ok\":true,\"configured\":false"; then
    printf "  %-14s " "$name"; dim "- off (optional)"
  elif echo "$status_json" | grep -q "\"$key\":{\"ok\":true"; then
    printf "  %-14s " "$name"; green "✓ ok"
  else
    local detail
    detail=$(echo "$status_json" | sed -n "s/.*\"$key\":{[^}]*\"detail\":\"\\([^\"]*\\)\".*/\\1/p" | head -c 100)
    printf "  %-14s " "$name"; yellow "⚠ ${detail:-not reachable, see the dashboard for why}"
  fi
}
check_svc "Navidrome"  "navidrome"
check_svc "Last.fm"    "lastfm"
check_svc "yt-dlp shim" "ytDlpShim"
check_svc "octo-sonic" "sonic"
check_svc "slskd"      "slskd"
if [ "${DOWNLOAD_SOURCE,,}" = "lidarr" ]; then
  check_svc "Lidarr" "lidarr"
fi

# ─────────────────────────────────────────────────────────────────
# Update helper (optional)
# ─────────────────────────────────────────────────────────────────
# Lets the dashboard's About page install a new release with one button. Linux
# with systemd only; elsewhere the dashboard shows the command to run instead.
if [ -d /run/systemd/system ]; then
  echo
  bold "─── Updates ────────────────────────────────────────────────"
  echo "  Octo can install new releases from its dashboard, through a small"
  echo "  service on this machine (scripts/updater). Octo itself never gets"
  echo "  access to Docker; it can only ask for the newest release."
  if ask_yn "  Let the dashboard update Octo?" "y"; then
    scripts/updater/install-updater.sh || yellow "  ⚠ The update helper was not installed; the dashboard will show the command instead."
  fi
fi

# ─────────────────────────────────────────────────────────────────
# Done
# ─────────────────────────────────────────────────────────────────
echo
bold "═══════════════════════════════════════════════════════════"
green "  Done. Three things to do next:"
echo
if [ "$STARTER" = true ]; then
  echo "  Your music server is Navidrome, started beside Octo. Its admin login,"
  echo "  for the dashboard and your music apps:"
  bold  "       admin / $NAVIDROME_ADMIN_PASSWORD"
  echo "     (also in .env as NAVIDROME_ADMIN_PASSWORD)"
  echo
fi
echo "  1. Open the dashboard to review settings (sign in with a Navidrome admin):"
bold  "       http://localhost:5274/admin"
echo
echo "  2. Point your music apps (Octo's own, Feishin, Arpeggi, Narjo, …) at:"
bold  "       http://<this-host>:5274"
if [ "$STARTER" = true ]; then
  echo "     Add the people who listen in Navidrome's own pages: http://<this-host>:4533"
else
  echo "     Sign in with your Navidrome login; Octo passes it through."
fi
echo
echo "  3. Test it: search for an artist you don't fully own. Owned tracks come"
echo "     up first; recommendations from Last.fm fill the rest. Tap one to hear"
if [ "${DOWNLOAD_SOURCE,,}" = "lidarr" ]; then
  echo "     the YouTube preview. Heart it to send its full album to Lidarr."
else
  echo "     the YouTube preview. Heart it to download via Soulseek."
fi
echo
dim "  slskd web UI:    http://<this-host>:5030    (SLSKD_USERNAME and SLSKD_PASSWORD in .env; or Open slskd on the dashboard)"
dim "  Sharing:         forward TCP 50300 to this machine; the dashboard's Soulseek page tests it"
if [ "$STARTER" = true ]; then
  dim "  Navidrome:       http://<this-host>:4533   (add people, change passwords; music apps use 5274)"
fi
dim "  Stop:            docker compose down"
dim "  Update later:    admin dashboard, About (or see Updating in the README)"
bold "═══════════════════════════════════════════════════════════"
