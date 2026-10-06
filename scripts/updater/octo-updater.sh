#!/usr/bin/env bash
# Octo's update helper. Runs on the host, started by octo-updater.path when the
# dashboard writes an update request into Octo's config folder (update/request).
# It runs fixed steps (fetch the release, check it, pull its sidecars, build Octo,
# restart it) and writes its progress back (update/status, update/log) for the
# dashboard to show.
#
# Octo never gets the Docker socket. All it can do is ask, by writing a file, and
# the request names only a release; nothing in it is ever run as a command. The
# tag must look like a dated release, exist as a tag on origin, and be newer than
# what this folder runs.
#
# Environment (set by octo-updater.service, written by install-updater.sh):
#   OCTO_DIR             the Octo folder, a git clone with docker-compose.yml
#   OCTO_UPDATE_DIR      Octo's config folder's update/ subfolder
#   OCTO_UPDATER_DRYRUN  1 runs every check and reports every step, but changes nothing
#
# "octo-updater.sh --describe" only writes update/helper, so the dashboard knows
# the helper is here. install-updater.sh calls it.
set -Eeuo pipefail
umask 022

HELPER_VERSION=1
OCTO_DIR="${OCTO_DIR:?OCTO_DIR is not set}"
UPDATE_DIR="${OCTO_UPDATE_DIR:?OCTO_UPDATE_DIR is not set}"
DRYRUN="${OCTO_UPDATER_DRYRUN:-0}"

TAG_PATTERN='^[0-9]{4}\.[0-9]{2}\.[0-9]{2}(\.[0-9]{1,4})?$'
ID_PATTERN='^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'

REQUEST="$UPDATE_DIR/request"
TAKEN="$UPDATE_DIR/request.taken"
STATUS="$UPDATE_DIR/status"
HELPER="$UPDATE_DIR/helper"
LOG="$UPDATE_DIR/log"

id="" tag="" from="" started=""

now() { date -u +%Y-%m-%dT%H:%M:%SZ; }

# The value of the first "key=" line in a file, and nothing else.
value_of() { sed -n "/^$1=/{s/^$1=//;s/\r\$//;p;q}" "$2"; }

# One line per value, so a value can never add a key of its own.
one_line() { printf '%s' "$1" | tr '\r\n' '  '; }

write_file() { # path, then the lines to write
  local path="$1" tmp="$1.tmp"
  shift
  printf '%s\n' "$@" > "$tmp"
  mv -f "$tmp" "$path"
}

write_status() { # state, step, error
  local finished=""
  case "$1" in done|failed) finished="$(now)" ;; esac
  write_file "$STATUS" "id=$id" "tag=$tag" "from=$from" "state=$1" "step=$(one_line "$2")" \
    "error=$(one_line "${3:-}")" "started=$started" "finished=$finished"
}

log() { printf '%s %s\n' "$(now)" "$*" >> "$LOG"; }

# Runs a command with its output in the log.
run() {
  log "\$ $*"
  "$@" >> "$LOG" 2>&1
}

# The failure is reported to the dashboard, so the unit itself ends cleanly.
fail() {
  log "failed: $1"
  write_status failed "${2:-}" "$1"
  exit 0
}

# git runs as whoever owns the clone, so a clone owned by a normal user never
# trips git's ownership check or gains root-owned files.
git_in_clone() {
  local owner
  owner="$(stat -c %U "$OCTO_DIR")"
  if [ "$(id -un)" != "$owner" ] && command -v runuser >/dev/null 2>&1; then
    runuser -u "$owner" -- git -C "$OCTO_DIR" "$@"
  else
    git -C "$OCTO_DIR" "$@"
  fi
}

# "build" when Compose builds Octo from this folder, "image" when it pulls a published image.
compose_mode() {
  local config
  config="$(docker compose config 2>/dev/null)" || { echo build; return; }
  printf '%s\n' "$config" | awk '
    /^services:/ { in_services = 1; next }
    in_services && /^[^ ]/ { in_services = 0 }
    in_services && /^  [^ ]/ { in_octo = ($0 ~ /^  octo:/) }
    in_services && in_octo && /^    build:/ { built = 1 }
    END { print (built ? "build" : "image") }'
}

# The services that run one of Octo's own published images, on one line: octo, and
# every service whose image is named after Octo's (ghcr.io/winters27/octo-yt-dlp-shim
# beside ghcr.io/winters27/octo). Others, such as slskd, are not Octo's to update.
# A service whose profile is off is not in the config, so it is never started here.
published_services() {
  { docker compose config 2>/dev/null || true; } | awk '
    function repo(image) {
      gsub(/["\047]/, "", image)
      sub(/@.*/, "", image)
      if (match(image, /:[^\/]*$/)) image = substr(image, 1, RSTART - 1)
      return image
    }
    /^services:/ { in_services = 1; next }
    in_services && /^[^ ]/ { in_services = 0 }
    in_services && /^  [^ ]/ { name = $1; sub(/:$/, "", name); order[++count] = name }
    in_services && /^    image:/ { image[name] = repo($2) }
    END {
      octo = image["octo"]
      line = "octo"
      for (i = 1; octo != "" && i <= count; i++)
        if (order[i] != "octo" && index(image[order[i]], octo "-") == 1) line = line " " order[i]
      print line
    }'
}

# The sidecars Octo publishes with every release. A built install pulls them rather than
# compiling them: octo-sonic is Rust and FFmpeg, minutes of building and more memory than a
# small machine has. Each has a <name>-source service in docker-compose.yml that builds it
# from the folder instead, for when the pull fails. A sidecar a later release adds and this
# list lacks is still pulled, by `up -d`, because its image is missing.
SIDECARS=(yt-dlp-shim octo-sonic)

# Pulls each sidecar the checked out release runs, and prints the -source service to build
# for every one that could not be pulled (no network to ghcr.io, or not published yet).
pull_sidecars() {
  local services service
  services=" $({ docker compose config --services 2>/dev/null || true; } | tr '\n' ' ') "
  for service in "${SIDECARS[@]}"; do
    [[ "$services" == *" $service "* ]] || continue
    run docker compose pull "$service" && continue
    log "$service could not be pulled, so it is built here instead"
    echo "$service-source"
  done
}

# The release this folder holds, from octo.csproj.
folder_version() {
  sed -n '/<InformationalVersion>/{s:.*<InformationalVersion>\(.*\)</InformationalVersion>.*:\1:p;q}' "$OCTO_DIR/octo/octo.csproj" 2>/dev/null || true
}

describe() {
  local installed=""
  [ -f "$HELPER" ] && installed="$(value_of installed "$HELPER")"
  write_file "$HELPER" "version=$HELPER_VERSION" "mode=$1" "dir=$OCTO_DIR" "installed=${installed:-$(now)}"
}

# Waits until the Octo container has stayed up for 20 seconds in a row.
wait_until_running() {
  local steady=0 container state
  for _ in $(seq 1 150); do
    container="$(docker compose ps -q octo 2>/dev/null || true)"
    state="$(docker inspect -f '{{.State.Status}}' "$container" 2>/dev/null || echo missing)"
    if [ "$state" = running ]; then steady=$((steady + 1)); else steady=0; fi
    [ "$steady" -ge 20 ] && return 0
    sleep 1
  done
  return 1
}

# Reinstalls this helper from the new release when it changed there, last, so the
# run that is ending never reads a half replaced file.
refresh_helper() {
  local source="$OCTO_DIR/scripts/updater" installed changed=0 name
  installed="$(dirname "$0")"
  [ -x "$source/install-updater.sh" ] || return 0
  for name in octo-updater.sh octo-updater.path octo-updater.service; do
    cmp -s "$source/$name" "$installed/$name" || changed=1
  done
  [ "$changed" = 1 ] || return 0
  log "The helper changed in this release; reinstalling it"
  run "$source/install-updater.sh" --config "$(dirname "$UPDATE_DIR")" --quiet || log "Reinstalling the helper failed; the old one stays"
}

mkdir -p "$UPDATE_DIR"
cd "$OCTO_DIR"

if [ "${1:-}" = "--describe" ]; then
  describe "$(compose_mode)"
  exit 0
fi

# One run at a time; systemd already ensures it, this covers a run started by hand.
exec 9> "$UPDATE_DIR/.lock"
flock 9

[ -f "$REQUEST" ] || exit 0
# Taken first, so the path unit cannot start again on the same request.
mv -f "$REQUEST" "$TAKEN"
id="$(value_of id "$TAKEN")"
tag="$(value_of tag "$TAKEN")"
rm -f "$TAKEN"

: > "$LOG"
started="$(now)"
# Anything that stops the script unplanned still reaches the dashboard.
trap 'log "stopped at line $LINENO"; write_status failed "" "The helper stopped unexpectedly. The log has the details."; exit 0' ERR
[[ "$id" =~ $ID_PATTERN ]] || id="invalid"
log "Update request $id for ${tag:-nothing}"
[ "$id" != invalid ] || fail "The request had no valid id, so it was ignored."
[[ "$tag" =~ $TAG_PATTERN ]] || { tag=""; fail "The request did not name a dated release, so it was ignored."; }

mode="$(compose_mode)"
describe "$mode"
write_status accepted "Starting the update to $tag"

if [ "$mode" = image ]; then
  # Octo and its sidecars move together, so the yt-dlp shim never stays on an old release.
  read -r -a services <<< "$(published_services)"
  write_status fetching "Pulling the new Octo images"
  if [ "$DRYRUN" = 1 ]; then
    log "Dry run: would pull and restart ${services[*]}"
  else
    run docker compose pull "${services[@]}" || fail "docker compose pull failed. The log has the details." "Pulling the new Octo images"
    write_status restarting "Restarting Octo"
    run docker compose up -d "${services[@]}" || fail "docker compose up failed. The log has the details." "Restarting Octo"
    wait_until_running || fail "Octo was started but did not stay running. 'docker compose logs octo' says why." "Restarting Octo"
  fi
  write_status "done" "$([ "$DRYRUN" = 1 ] && echo "Dry run: nothing was changed" || echo "Octo restarted on the new images")"
  exit 0
fi

from="$(folder_version)"
changed="$(git_in_clone status --porcelain --untracked-files=no)"
if [ -n "$changed" ]; then
  # awk reads every line, so a long list never cuts git off mid-write under pipefail.
  changed="$(printf '%s\n' "$changed" | awk 'NR <= 5 { printf "%s%s", (NR > 1 ? ", " : ""), substr($0, 4) } END { if (NR > 5) printf ", and %d more", NR - 5 }')"
  # Usually docker-compose.yml edited for ports or paths. Those belong in
  # docker-compose.override.yml, which is the server's own and never blocks an update.
  fail "Octo's own files were changed in $OCTO_DIR ($changed), so the helper left the folder alone. Move compose changes into docker-compose.override.yml and undo the edits (git checkout -- <file>), then try again." "Checking the Octo folder"
fi

write_status fetching "Fetching $tag from GitHub"
run git_in_clone fetch --tags --force origin || fail "git fetch failed. The log has the details." "Fetching $tag from GitHub"
git_in_clone rev-parse -q --verify "refs/tags/$tag^{commit}" > /dev/null \
  || fail "$tag is not a release tag on this folder's origin." "Fetching $tag from GitHub"
if [[ "$from" =~ $TAG_PATTERN ]]; then
  if [ "$from" = "$tag" ] || [ "$(printf '%s\n%s\n' "$from" "$tag" | sort -V | tail -n 1)" != "$tag" ]; then
    fail "This folder already holds $from, which is not older than $tag." "Fetching $tag from GitHub"
  fi
fi

if [ "$DRYRUN" = 1 ]; then
  write_status building "Dry run: would build $tag"
  log "Dry run: would check out $tag, pull ${SIDECARS[*]} (building any that will not pull), build octo, and restart"
  write_status "done" "Dry run: nothing was changed"
  exit 0
fi

# Where to go back to if the build fails: the branch when there is one.
previous="$(git_in_clone symbolic-ref -q --short HEAD || git_in_clone rev-parse HEAD)"
run git_in_clone checkout --quiet --detach "$tag" || fail "git could not check out $tag. The log has the details." "Fetching $tag from GitHub"

# Pulled and built before anything stops, so a failure leaves the running Octo alone. Only
# Octo is built, and a sidecar only when its image could not be pulled.
write_status building "Pulling the sidecars for $tag"
read -r -a fallback <<< "$(pull_sidecars | tr '\n' ' ')"
write_status building "Building Octo $tag"
if ! run docker compose build octo "${fallback[@]}"; then
  run git_in_clone checkout --quiet "$previous" || log "Could not check out $previous again"
  fail "The build failed, so nothing was restarted and Octo still runs ${from:-the old version}. The log has the details." "Building Octo $tag"
fi

write_status restarting "Restarting Octo"
if ! run docker compose up -d || ! wait_until_running; then
  # The new release will not run here, so the one that did goes back in.
  log "Octo $tag did not stay running; going back to ${from:-$previous}"
  write_status restarting "Octo $tag did not stay running; going back to ${from:-the old version}"
  if run git_in_clone checkout --quiet "$previous" && run docker compose build && run docker compose up -d && wait_until_running; then
    fail "Octo $tag did not stay running, so the helper went back to ${from:-the old version}, which is running again. The log has the details." "Restarting Octo"
  fi
  fail "Octo $tag did not stay running, and going back to ${from:-the old version} failed too. 'docker compose logs octo' on the host says why." "Restarting Octo"
fi

write_status "done" "Octo now runs $tag"
log "Done: $from to $tag"
refresh_helper
exit 0
