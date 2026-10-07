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

# docker logs what it was asked. Its compose file is shaped like the real one: Octo and its
# sidecars are pulled, at the tag the checked out release names (its octo.csproj version),
# or FAKE_IMAGE_TAG (OCTO_IMAGE_TAG in .env). FAKE_REGISTRY is where they come from,
# FAKE_OCTO=build is an override that builds Octo from the folder, DOCKER_FAIL makes one
# compose verb fail, FAKE_NO_IMAGE names services whose pull fails (no network to ghcr.io),
# FAKE_NO_SONIC leaves octo-sonic out, and FAKE_BAD_VERSION is a release whose container
# never stays up.
cat > "$work/bin/docker" <<'EOF'
#!/usr/bin/env bash
echo "docker $*" >> "$FAKE_LOG"
if [ "$1" = compose ]; then
  shift
  # The global --profile flag comes before the verb.
  while [ "${1:-}" = --profile ]; do shift 2; done
  case "$1" in
    config)
      if [ "${2:-}" = --services ]; then
        printf 'octo\n'
        [ -n "${FAKE_NO_SONIC:-}" ] || echo octo-sonic
        printf 'slskd\nunrelated\nyt-dlp-shim\n'
      else
        # As docker compose config prints it: services in name order, octo-sonic's image
        # quoted and the shim's not, slskd someone else's image, a volume list after. The
        # -source services are in a profile that is off, so they are absent.
        r="${FAKE_REGISTRY:-ghcr.io/winters27}"
        t="${FAKE_IMAGE_TAG:-$(sed -n 's:.*<InformationalVersion>\(.*\)</InformationalVersion>.*:\1:p' "$OCTO_DIR/octo/octo.csproj" | head -n 1)}"
        printf 'name: octo\nservices:\n'
        printf '  octo:\n'
        [ "${FAKE_OCTO:-pull}" = build ] && printf '    build:\n      context: %s\n' "$OCTO_DIR"
        printf '    image: %s/octo:%s\n    depends_on:\n      slskd:\n        condition: service_started\n' "$r" "$t"
        [ -n "${FAKE_NO_SONIC:-}" ] || printf '  octo-sonic:\n    image: "%s/octo-sonic:%s"\n' "$r" "$t"
        printf '  slskd:\n    image: slskd/slskd:latest\n'
        printf '  unrelated:\n    image: %s/octopus:1\n' "$r"
        printf '  yt-dlp-shim:\n    image: %s/octo-yt-dlp-shim:%s\n' "$r" "$t"
        printf 'volumes:\n  ytdlp-bin:\n    name: octo_ytdlp-bin\n'
      fi ;;
    ps) echo cid ;;
    pull)
      for service in ${FAKE_NO_IMAGE:-}; do
        [ "$2" = "$service" ] && { echo "Error response from daemon: $service: denied"; exit 1; }
      done
      if [ "${DOCKER_FAIL:-}" = pull ]; then echo "boom: pull failed"; exit 1; fi ;;
    *) if [ "${DOCKER_FAIL:-}" = "$1" ]; then echo "boom: $1 failed"; exit 1; fi ;;
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

# An origin whose main is at 2026.10.01, with a newer release 2026.10.04 tagged. The newer
# release carries a helper of its own, whose installer only says it ran (or fails, with
# FAKE_REFRESH_FAIL), so a run can be seen reinstalling the helper.
(
  cd "$work/origin-src" || exit 1
  git init -q -b main . && git config user.email test@example.com && git config user.name test
  git config core.autocrlf false
  version() { printf '<Project>\n  <PropertyGroup>\n    <InformationalVersion>%s</InformationalVersion>\n  </PropertyGroup>\n</Project>\n' "$1" > octo/octo.csproj; }
  version 2026.10.01 && git add -A && git commit -qm one && git tag 2026.10.01
  mkdir -p scripts/updater
  printf '#!/usr/bin/env bash\necho "install-updater $*" >> "$FAKE_LOG"\n[ -z "${FAKE_REFRESH_FAIL:-}" ]\n' > scripts/updater/install-updater.sh
  echo "# the next helper" > scripts/updater/octo-updater.sh
  touch scripts/updater/octo-updater.path scripts/updater/octo-updater.service
  version 2026.10.04 && git add -A && git update-index --chmod=+x scripts/updater/install-updater.sh \
    && git commit -qm two && git tag 2026.10.04
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
copied() { # the same folder, copied by hand: not a git clone
  fresh
  rm -rf "$work/octo/.git"
}
request() { # tag, id
  printf 'id=%s\ntag=%s\nby=test\nat=2026-10-03T12:00:00Z\n' "${2:-0b7c7a8e-1111-4222-8333-444455556666}" "$1" > "$work/config/update/request"
}
status_of() { sed -n "s/^$1=//p" "$work/config/update/status" 2>/dev/null | head -n 1; }
helper_says() { sed -n "s/^$1=//p" "$work/config/update/helper" 2>/dev/null | head -n 1; }
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
expect() { # name, then a command that must succeed
  local name="$1"
  shift
  if "$@"; then echo "ok    $name"; else echo "FAIL  $name"; failures=$((failures + 1)); fi
}
in_log() { grep -q -- "$1" "$work/config/update/log"; }
never() { ! grep -Eq -- "$1" "$FAKE_LOG"; }

# The compose commands a run gave, in order, joined with |.
compose_log() { sed -n 's/^docker compose //p' "$FAKE_LOG" | grep -Ev '^(config|ps)' | paste -sd '|' -; }
expect_compose() { # name, the compose commands wanted, in order, joined with |
  local got
  got="$(compose_log)"
  if [ "$got" = "$2" ]; then echo "ok    $1"; else echo "FAIL  $1: ran $got (want $2)"; failures=$((failures + 1)); fi
}

# ----- A git clone: fetch, check out, pull -----

fresh; request 2026.10.04
check "updates to a newer release" "done" 2026.10.04 ""
expect "describes itself as version 2 in a clone" test "$(helper_says version) $(helper_says mode)" = "2 git"
expect_compose "pulls Octo and its sidecars, builds nothing, then restarts" "pull octo|pull yt-dlp-shim|pull octo-sonic|up -d"
expect "says what it now runs" test "$(status_of step)" = "Octo now runs 2026.10.04"

fresh; request 2026.10.04
FAKE_NO_IMAGE=octo check "Octo that will not pull is built from the folder" "done" 2026.10.04 ""
expect_compose "builds octo-source, the sidecars still pulled" "pull octo|pull yt-dlp-shim|pull octo-sonic|--profile source build octo-source|up -d"
expect "the log says why it built" in_log 'octo could not be pulled'
expect "the dashboard says it was built here" test "$(status_of step)" = "Octo now runs 2026.10.04, built on this server because its image could not be pulled"

fresh; request 2026.10.04
FAKE_NO_IMAGE=octo-sonic check "a sidecar that will not pull is built instead" "done" 2026.10.04 ""
expect_compose "builds octo-sonic from the folder, Octo and the shim still pulled" "pull octo|pull yt-dlp-shim|pull octo-sonic|--profile source build octo-sonic-source|up -d"
expect "a sidecar built here is not called Octo's build" test "$(status_of step)" = "Octo now runs 2026.10.04"

fresh; request 2026.10.04
FAKE_NO_IMAGE="octo yt-dlp-shim octo-sonic" check "without ghcr.io everything is built" "done" 2026.10.04 ""
expect_compose "builds Octo and both sidecars" "pull octo|pull yt-dlp-shim|pull octo-sonic|--profile source build octo-source yt-dlp-shim-source octo-sonic-source|up -d"

fresh; request 2026.10.04
FAKE_NO_IMAGE=octo DOCKER_FAIL=build check "a failed build changes nothing" failed 2026.10.01 "nothing was restarted"
expect "nothing restarted after the failed build" never 'compose up'

fresh; request 2026.10.04
FAKE_NO_SONIC=1 check "a release without octo-sonic" "done" 2026.10.04 ""
expect_compose "pulls only the images the release has" "pull octo|pull yt-dlp-shim|up -d"

fresh; request 2026.10.04
FAKE_OCTO=build check "an override that builds Octo from the folder" "done" 2026.10.04 ""
expect_compose "builds Octo and pulls the sidecars" "pull yt-dlp-shim|pull octo-sonic|--profile source build octo|up -d"
expect "the log says why it built" in_log 'An override builds Octo'

fresh; request 2026.10.04
FAKE_IMAGE_TAG=latest check "OCTO_IMAGE_TAG=latest in a clone" "done" 2026.10.04 ""
expect_compose "pulls latest with the release checked out" "pull octo|pull yt-dlp-shim|pull octo-sonic|up -d"

fresh; request 2026.10.04
FAKE_IMAGE_TAG=2026.10.01 check "OCTO_IMAGE_TAG pinned to an older release is refused" failed 2026.10.01 "asks for Octo 2026.10.01"
expect "nothing pulled or restarted while pinned" never 'compose (pull|build|up)'

fresh; request "2026.10.04; touch $work/pwned"
check "a tag that is not a release is ignored" failed 2026.10.01 "did not name a dated release"
expect "nothing in a request is run" test ! -e "$work/pwned"

fresh; request 2026.10.04 not-a-guid
check "a request without a valid id is ignored" failed 2026.10.01 "no valid id"

fresh; request 2026.10.09
check "a release that does not exist" failed 2026.10.01 "is not a release tag"

fresh; request 2026.10.01
check "the same release is refused" failed 2026.10.01 "not older than"

fresh; echo "<!-- edit -->" >> "$work/octo/octo/octo.csproj"; request 2026.10.04
check "local changes to Octo's own files are left alone" failed 2026.10.01 "(octo/octo.csproj)"
expect "a changed folder is never pulled over" never 'compose (pull|build|up)'

fresh; echo "KEY=value" > "$work/octo/.env"; request 2026.10.04
check "untracked files such as .env are fine" "done" 2026.10.04 ""

fresh; request 2026.10.04
FAKE_BAD_VERSION=2026.10.04 check "a release that will not stay up is rolled back" failed 2026.10.01 "went back to 2026.10.01"
# The old release's images are still here; a plain build builds what it built, if anything.
expect_compose "the rollback restarts the old release" "pull octo|pull yt-dlp-shim|pull octo-sonic|up -d|build|up -d"

fresh; request 2026.10.04
FAKE_STATE=restarting check "a failed rollback says so" failed 2026.10.01 "failed too"

fresh; request 2026.10.04
OCTO_UPDATER_DRYRUN=1 check "a dry run changes nothing" "done" 2026.10.01 ""
expect "the dry run never builds or pulls" never 'compose (build|pull|up)'

fresh; request 2026.10.04
check "a release with a new helper" "done" 2026.10.04 ""
expect "reinstalls the helper from the release" grep -q '^install-updater --config .* --quiet$' "$FAKE_LOG"
expect "the log says so" in_log 'reinstalling it'

fresh; request 2026.10.04
FAKE_REFRESH_FAIL=1 check "a helper that cannot be reinstalled leaves the update done" "done" 2026.10.04 ""
expect "the log says the old helper stays" in_log 'the old one stays'

fresh; git -C "$work/octo" remote remove origin; request 2026.10.04
FAKE_IMAGE_TAG=latest check "a clone without an origin only pulls" "done" 2026.10.01 ""
expect "describes itself as an image install" test "$(helper_says mode)" = image

# ----- Copied by hand, not a git clone: pull only -----

copied; request 2026.10.04
FAKE_IMAGE_TAG=latest check "an install that is not a clone pulls instead" "done" none ""
expect "describes itself as version 2, image" test "$(helper_says version) $(helper_says mode)" = "2 image"
expect "pulled Octo and its sidecars, not slskd" grep -qx 'docker compose pull octo octo-sonic yt-dlp-shim' "$FAKE_LOG"
expect "restarted Octo and its sidecars" grep -qx 'docker compose up -d octo octo-sonic yt-dlp-shim' "$FAKE_LOG"
expect "it never builds" never 'compose .*build'

copied; request 2026.10.04
check "a copied compose file that names an older release is refused" failed none "would not reach 2026.10.04"
expect "nothing pulled when it would not move" never 'compose (pull|up)'

copied; request 2026.10.04
FAKE_IMAGE_TAG=2026.10.04 check "a copied compose file already at the release pulls it" "done" none ""

copied; request 2026.10.04
FAKE_IMAGE_TAG=latest FAKE_REGISTRY=localhost:5000/me check "a registry with a port" "done" none ""
expect "the port is not taken for a tag" grep -qx 'docker compose pull octo octo-sonic yt-dlp-shim' "$FAKE_LOG"

copied; request 2026.10.04
FAKE_IMAGE_TAG=latest DOCKER_FAIL=pull check "a failed pull restarts nothing" failed none "docker compose pull failed"
expect "nothing restarted" never 'compose up'

copied; request 2026.10.04
FAKE_IMAGE_TAG=latest FAKE_OCTO=build check "Octo built from a folder that is not a clone is refused" failed none "not a git clone"

copied; request 2026.10.04
FAKE_IMAGE_TAG=latest OCTO_UPDATER_DRYRUN=1 check "an image install's dry run changes nothing" "done" none ""
expect "the image dry run never pulls" never 'compose (pull|up)'

fresh
OCTO_DIR="$work/octo" OCTO_UPDATE_DIR="$work/config/update" bash "$script" > /dev/null 2>&1
expect "no request, no run" test ! -f "$work/config/update/status"

fresh
OCTO_DIR="$work/octo" OCTO_UPDATE_DIR="$work/config/update" bash "$script" --describe > /dev/null 2>&1
expect "--describe writes version 2" test "$(helper_says version) $(helper_says mode)" = "2 git"

echo
[ "$failures" = 0 ] && echo "All update helper tests passed." || echo "$failures update helper test(s) failed."
[ "$failures" = 0 ]
