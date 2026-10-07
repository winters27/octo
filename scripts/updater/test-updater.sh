#!/usr/bin/env bash
# Tests octo-updater.sh against a throwaway git origin, with docker, flock and sleep
# faked, so it runs anywhere bash and git do (CI runs it beside shellcheck).
#   scripts/updater/test-updater.sh
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
script="$here/octo-updater.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/bin" "$work/origin-src/octo"
failures=0

# docker logs what it was asked. FAKE_MODE picks a built or a pulled Octo (FAKE_REGISTRY is
# where a pulled one comes from), DOCKER_FAIL makes one compose verb fail, FAKE_NO_IMAGE
# names services whose pull fails (no network to ghcr.io), FAKE_NO_SONIC leaves octo-sonic
# out of the compose file, and FAKE_BAD_VERSION is a release whose container never stays up.
cat > "$work/bin/docker" <<'EOF'
#!/usr/bin/env bash
echo "docker $*" >> "$FAKE_LOG"
if [ "$1" = compose ]; then
  case "$2" in
    config)
      if [ "${3:-}" = --services ]; then
        if [ "${FAKE_MODE:-build}" = build ]; then printf 'octo\nslskd\nyt-dlp-shim\n'; else printf 'octo\nslskd\nunrelated\nyt-dlp-shim\n'; fi
        [ -n "${FAKE_NO_SONIC:-}" ] || echo octo-sonic
      elif [ "${FAKE_MODE:-build}" = build ]; then
        printf 'name: octo\nservices:\n  octo:\n    build:\n      context: .\n  slskd:\n    image: slskd/slskd\n'
      else
        # Shaped like docker compose config prints it: services in name order, the sidecars
        # published (octo-sonic quoted, the shim not), slskd someone else's image, a volume
        # list after. The -source services are in a profile that is off, so they are absent.
        r="${FAKE_REGISTRY:-ghcr.io/winters27}"
        printf 'name: octo\nservices:\n'
        printf '  octo:\n    image: %s/octo:latest\n    depends_on:\n      slskd:\n        condition: service_started\n' "$r"
        printf '  octo-sonic:\n    image: "%s/octo-sonic:latest"\n' "$r"
        printf '  slskd:\n    image: slskd/slskd:latest\n'
        printf '  unrelated:\n    image: %s/octopus:1\n' "$r"
        printf '  yt-dlp-shim:\n    image: %s/octo-yt-dlp-shim:latest\n' "$r"
        printf 'volumes:\n  ytdlp-bin:\n    name: octo_ytdlp-bin\n'
      fi ;;
    ps) echo cid ;;
    pull)
      for service in ${FAKE_NO_IMAGE:-}; do
        [ "$3" = "$service" ] && { echo "Error response from daemon: $service: not found"; exit 1; }
      done
      if [ "${DOCKER_FAIL:-}" = pull ]; then echo "boom: pull failed"; exit 1; fi ;;
    *) if [ "${DOCKER_FAIL:-}" = "$2" ]; then echo "boom: $2 failed"; exit 1; fi ;;
  esac
  exit 0
fi
if [ "$1" = inspect ]; then
  if [ -n "${FAKE_BAD_VERSION:-}" ] && grep -q "$FAKE_BAD_VERSION" "$OCTO_DIR/octo/octo.csproj"; then
    echo restarting
  else
    echo "${FAKE_STATE:-running}"
  fi
fi
EOF
printf '#!/usr/bin/env bash\nexit 0\n' > "$work/bin/flock"
printf '#!/usr/bin/env bash\nexit 0\n' > "$work/bin/sleep"
chmod +x "$work/bin/"*
export PATH="$work/bin:$PATH" FAKE_LOG="$work/docker.log"

# An origin whose main is at 2026.10.01, with a newer release 2026.10.04 tagged.
(
  cd "$work/origin-src" || exit 1
  git init -q -b main . && git config user.email test@example.com && git config user.name test
  version() { printf '<Project>\n  <PropertyGroup>\n    <InformationalVersion>%s</InformationalVersion>\n  </PropertyGroup>\n</Project>\n' "$1" > octo/octo.csproj; }
  version 2026.10.01 && git add -A && git commit -qm one && git tag 2026.10.01
  version 2026.10.04 && git add -A && git commit -qm two && git tag 2026.10.04
  git reset -q --hard 2026.10.01
) || { echo "could not build the test origin"; exit 1; }
git clone -q --bare "$work/origin-src" "$work/origin.git"

fresh() { # a clone on main at 2026.10.01 that has not fetched 2026.10.04 yet
  rm -rf "$work/octo" "$work/config"
  git clone -q "$work/origin.git" "$work/octo"
  git -C "$work/octo" tag -d 2026.10.04 > /dev/null 2>&1 || true
  mkdir -p "$work/config/update"
  : > "$FAKE_LOG"
}
request() { # tag, id
  printf 'id=%s\ntag=%s\nby=test\nat=2026-10-03T12:00:00Z\n' "${2:-0b7c7a8e-1111-4222-8333-444455556666}" "$1" > "$work/config/update/request"
}
status_of() { sed -n "s/^$1=//p" "$work/config/update/status" 2>/dev/null | head -n 1; }
check() { # name, expected state, expected checkout, words the error must hold
  local name="$1" state head error
  OCTO_DIR="$work/octo" OCTO_UPDATE_DIR="$work/config/update" bash "$script" > /dev/null 2>&1
  state="$(status_of state)"
  head="$(git -C "$work/octo" describe --tags --exact-match 2>/dev/null || echo none)"
  error="$(status_of error)"
  if [ "$state" = "$2" ] && [ "$head" = "$3" ] && [[ "$error" == *"$4"* ]] && [ ! -f "$work/config/update/request" ]; then
    echo "ok    $name"
  else
    echo "FAIL  $name: state=$state (want $2), checkout=$head (want $3), error=$error"
    failures=$((failures + 1))
  fi
}

# The compose commands a run gave, in order, joined with |.
compose_log() { sed -n 's/^docker compose //p' "$FAKE_LOG" | grep -Ev '^(config|ps)' | paste -sd '|' -; }
expect_compose() { # name, the compose commands wanted, in order, joined with |
  local got
  got="$(compose_log)"
  if [ "$got" = "$2" ]; then echo "ok    $1"; else echo "FAIL  $1: ran $got (want $2)"; failures=$((failures + 1)); fi
}

fresh; request 2026.10.04
check "updates to a newer release" "done" 2026.10.04 ""
grep -q '^mode=build$' "$work/config/update/helper" && echo "ok    describes itself as a built install" || { echo "FAIL  helper file"; failures=$((failures + 1)); }
expect_compose "pulls the sidecars, builds only Octo, then restarts" "pull yt-dlp-shim|pull octo-sonic|build octo|up -d"

fresh; request 2026.10.04
FAKE_NO_IMAGE=octo-sonic check "a sidecar that will not pull is built instead" "done" 2026.10.04 ""
expect_compose "builds octo-sonic from the folder, the shim still pulled" "pull yt-dlp-shim|pull octo-sonic|build octo octo-sonic-source|up -d"
grep -q 'octo-sonic could not be pulled' "$work/config/update/log" && echo "ok    the log says why it built" || { echo "FAIL  the log does not say why"; failures=$((failures + 1)); }

fresh; request 2026.10.04
FAKE_NO_IMAGE="yt-dlp-shim octo-sonic" check "without ghcr.io everything is built, as before" "done" 2026.10.04 ""
expect_compose "builds Octo and both sidecars" "pull yt-dlp-shim|pull octo-sonic|build octo yt-dlp-shim-source octo-sonic-source|up -d"

fresh; request 2026.10.04
FAKE_NO_IMAGE=octo-sonic DOCKER_FAIL=build check "a failed sidecar build changes nothing" failed 2026.10.01 "nothing was restarted"
grep -q 'compose up' "$FAKE_LOG" && { echo "FAIL  restarted after a failed build"; failures=$((failures + 1)); } || echo "ok    nothing restarted after the failed build"

fresh; request 2026.10.04
FAKE_NO_SONIC=1 check "a release without octo-sonic" "done" 2026.10.04 ""
expect_compose "pulls only the sidecars the release has" "pull yt-dlp-shim|build octo|up -d"

fresh; request "2026.10.04; touch $work/pwned"
check "a tag that is not a release is ignored" failed 2026.10.01 "did not name a dated release"
[ ! -e "$work/pwned" ] && echo "ok    nothing in a request is run" || { echo "FAIL  a request ran a command"; failures=$((failures + 1)); }

fresh; request 2026.10.04 not-a-guid
check "a request without a valid id is ignored" failed 2026.10.01 "no valid id"

fresh; request 2026.10.09
check "a release that does not exist" failed 2026.10.01 "is not a release tag"

fresh; request 2026.10.01
check "the same release is refused" failed 2026.10.01 "not older than"

fresh; echo "<!-- edit -->" >> "$work/octo/octo/octo.csproj"; request 2026.10.04
check "local changes to Octo's own files are left alone" failed 2026.10.01 "(octo/octo.csproj)"

fresh; echo "KEY=value" > "$work/octo/.env"; request 2026.10.04
check "untracked files such as .env are fine" "done" 2026.10.04 ""

fresh; request 2026.10.04
DOCKER_FAIL=build check "a failed build changes nothing" failed 2026.10.01 "nothing was restarted"

fresh; request 2026.10.04
FAKE_BAD_VERSION=2026.10.04 check "a release that will not stay up is rolled back" failed 2026.10.01 "went back to 2026.10.01"
# The old release builds the way it always did; its sidecar images are still here.
expect_compose "the rollback rebuilds and restarts the old release" "pull yt-dlp-shim|pull octo-sonic|build octo|up -d|build|up -d"

fresh; request 2026.10.04
FAKE_STATE=restarting check "a failed rollback says so" failed 2026.10.01 "failed too"

fresh; request 2026.10.04
OCTO_UPDATER_DRYRUN=1 check "a dry run changes nothing" "done" 2026.10.01 ""
grep -Eq 'compose (build|pull)' "$FAKE_LOG" && { echo "FAIL  the dry run built or pulled"; failures=$((failures + 1)); } || echo "ok    the dry run never builds or pulls"

fresh; request 2026.10.04
FAKE_MODE=image check "an image install pulls instead" "done" 2026.10.01 ""
grep -qx 'docker compose pull octo octo-sonic yt-dlp-shim' "$FAKE_LOG" && echo "ok    pulled Octo and its sidecars, not slskd" || { echo "FAIL  pulled: $(grep 'compose pull' "$FAKE_LOG")"; failures=$((failures + 1)); }
grep -qx 'docker compose up -d octo octo-sonic yt-dlp-shim' "$FAKE_LOG" && echo "ok    restarted Octo and its sidecars" || { echo "FAIL  restarted: $(grep 'compose up' "$FAKE_LOG")"; failures=$((failures + 1)); }
grep -q 'compose build' "$FAKE_LOG" && { echo "FAIL  an image install built"; failures=$((failures + 1)); } || echo "ok    an image install never builds"

fresh; request 2026.10.04
FAKE_MODE=image FAKE_REGISTRY=localhost:5000/me check "a registry with a port" "done" 2026.10.01 ""
grep -qx 'docker compose pull octo octo-sonic yt-dlp-shim' "$FAKE_LOG" && echo "ok    the port is not taken for a tag" || { echo "FAIL  pulled: $(grep 'compose pull' "$FAKE_LOG")"; failures=$((failures + 1)); }

fresh; request 2026.10.04
FAKE_MODE=image DOCKER_FAIL=pull check "a failed pull restarts nothing" failed 2026.10.01 "docker compose pull failed"
grep -q 'compose up' "$FAKE_LOG" && { echo "FAIL  restarted after a failed pull"; failures=$((failures + 1)); } || echo "ok    nothing restarted"

fresh; request 2026.10.04
FAKE_MODE=image OCTO_UPDATER_DRYRUN=1 check "an image install's dry run changes nothing" "done" 2026.10.01 ""
grep -Eq 'compose (pull|up)' "$FAKE_LOG" && { echo "FAIL  the dry run pulled or restarted"; failures=$((failures + 1)); } || echo "ok    the image dry run never pulls"

fresh
OCTO_DIR="$work/octo" OCTO_UPDATE_DIR="$work/config/update" bash "$script" > /dev/null 2>&1
[ ! -f "$work/config/update/status" ] && echo "ok    no request, no run" || { echo "FAIL  ran without a request"; failures=$((failures + 1)); }

echo
[ "$failures" = 0 ] && echo "All update helper tests passed." || echo "$failures update helper test(s) failed."
[ "$failures" = 0 ]
