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
    read -rsp "$prompt [keep existing]: " reply; echo
    echo "${reply:-$default}"
  else
    read -rsp "$prompt: " reply; echo
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
declare -A EXISTING
if [ -f .env ]; then
  while IFS='=' read -r k v; do
    [[ "$k" =~ ^[A-Z_]+$ ]] || continue
    v="${v%\"}"; v="${v#\"}"
    EXISTING[$k]="$v"
  done < .env
fi
existing() { echo "${EXISTING[$1]-}"; }

# ─────────────────────────────────────────────────────────────────
# Run
# ─────────────────────────────────────────────────────────────────
clear
bold "═══════════════════════════════════════════════════════════"
bold "  Octo · installer"
bold "═══════════════════════════════════════════════════════════"
echo
echo "Sets up Octo (admin UI + proxy), yt-dlp shim, and slskd in"
echo "one Docker Compose stack. Talks to your existing Navidrome."
echo
echo "What you'll need handy:"
echo "  • A Navidrome server URL (running already)"
echo "  • A free Last.fm API key — https://www.last.fm/api/account/create"
echo "  • A free Soulseek (slsknet.org) account"
echo
require_docker
green "✓ Docker + Compose v2 ready"
echo

# ─────────────────────────────────────────────────────────────────
# Required: Navidrome URL + music directory
# ─────────────────────────────────────────────────────────────────
bold "─── Required ───────────────────────────────────────────────"
while true; do
  SUBSONIC_URL=$(ask "Navidrome URL" "$(existing SUBSONIC_URL || echo "http://192.168.1.10:4533")")
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
echo

DOWNLOAD_PATH_RAW=$(ask "Music directory on this host (where downloads will land)" \
  "$(existing DOWNLOAD_PATH || echo "./downloads")")
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

# ─────────────────────────────────────────────────────────────────
# Storage / layout (keep the simple defaults visible)
# ─────────────────────────────────────────────────────────────────
bold "─── Storage / layout ───────────────────────────────────────"
echo "  Stream     — preview only; star a song to download (recommended)"
echo "  Permanent  — download every song you play"
echo "  Cache      — temporary, auto-cleanup"
STORAGE_MODE=$(ask "Storage mode" "$(existing STORAGE_MODE || echo "Stream")")
echo
echo "  Flat       — Artist - Title.flac (no subfolders, easier to browse)"
echo "  Organized  — Artist/Title/file.flac"
FOLDER_STRUCTURE=$(ask "Folder layout" "$(existing FOLDER_STRUCTURE || echo "Flat")")
echo

# slskd web UI admin — auto-generate on first run, preserve on re-run
SLSKD_USERNAME="$(existing SLSKD_USERNAME || echo "admin")"
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

# === Required ===
SUBSONIC_URL=$SUBSONIC_URL
DOWNLOAD_PATH=$DOWNLOAD_PATH

# === Last.fm ===
LASTFM_API_KEY=$LASTFM_API_KEY
LASTFM_ENABLE_RADIO=true
LASTFM_RADIO_TRACK_COUNT=50
LASTFM_RADIO_CACHE_HOURS=24

# === Soulseek (slskd) ===
SLSKD_USERNAME=$SLSKD_USERNAME
SLSKD_PASSWORD=$SLSKD_PASSWORD
SLSKD_SEARCH_WAIT_SECONDS=6
SLSKD_MIN_FILE_SIZE_BYTES=5242880
SLSKD_PREFERRED_EXTENSION=flac
SLSKD_DOWNLOAD_TIMEOUT_SECONDS=180
SLSKD_SOULSEEK_USERNAME=$SLSKD_SOULSEEK_USERNAME
SLSKD_SOULSEEK_PASSWORD="$SLSKD_SOULSEEK_PASSWORD"

# === Storage / layout ===
STORAGE_MODE=$STORAGE_MODE
DOWNLOAD_MODE=Track
DOWNLOAD_ON_STAR=true
FOLDER_STRUCTURE=$FOLDER_STRUCTURE
USE_LOCAL_STAGING=false
EXPLICIT_FILTER=All
CACHE_DURATION_HOURS=1
ENABLE_EXTERNAL_PLAYLISTS=false

# === yt-dlp shim (defaults are fine) ===
YTDLP_MAX_CONCURRENT=5
YTDLP_SEARCH_CACHE_MAX=1024
YTDLP_URL_CACHE_MAX=512
YTDLP_URL_CACHE_TTL=3600
EOF
chmod 600 .env
green "✓ wrote .env (chmod 600)"

# Make sure the bind-mount targets exist so docker doesn't create them root-owned.
mkdir -p octo-config slskd-state

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
    echo "  Inspect:   docker compose logs octorr"
    echo "  Restart:   docker compose down && docker compose up -d"
    exit 1
  fi
done

echo
bold "─── Service health ─────────────────────────────────────────"
status_json=$(curl -sS -m 5 "http://localhost:5274/api/admin/status" 2>/dev/null || echo "{}")
check_svc() {
  local name="$1" key="$2"
  if echo "$status_json" | grep -q "\"$key\":{\"ok\":true"; then
    printf "  %-14s " "$name"; green "✓ ok"
  else
    local detail
    detail=$(echo "$status_json" | sed -n "s/.*\"$key\":{[^}]*\"detail\":\"\\([^\"]*\\)\".*/\\1/p" | head -c 100)
    printf "  %-14s " "$name"; yellow "⚠ ${detail:-not reachable}"
  fi
}
check_svc "Navidrome"  "navidrome"
check_svc "Last.fm"    "lastfm"
check_svc "yt-dlp shim" "ytDlpShim"
check_svc "slskd"      "slskd"

# ─────────────────────────────────────────────────────────────────
# Done
# ─────────────────────────────────────────────────────────────────
echo
bold "═══════════════════════════════════════════════════════════"
green "  Done. Three things to do next:"
echo
echo "  1. Open the admin dashboard to review settings:"
bold  "       http://localhost:5274/admin"
echo
echo "  2. Point your Subsonic apps (Feishin, Arpeggi, Narjo, …) at:"
bold  "       http://<this-host>:5274"
echo "     Use your existing Navidrome credentials — Octo just proxies."
echo
echo "  3. Test it: search for an artist you don't fully own. Owned tracks come"
echo "     up first; recommendations from Last.fm fill the rest. Tap one to hear"
echo "     the YouTube preview. Heart it to download via Soulseek."
echo
dim "  slskd web UI:    http://<this-host>:5030    (admin / shown above)"
dim "  Stop:            docker compose down"
dim "  Update later:    git pull && ./install.sh"
bold "═══════════════════════════════════════════════════════════"
