#!/usr/bin/env bash
# Tests octo-chromium, the launcher Mornye starts its check browser through: the browser gets
# every argument as it was, keeps the launcher's process (so the port file's pid is the
# browser's), and its DevTools port is noted for the helper. A folder it cannot write never
# stops the browser. Runs anywhere sh does (CI runs it beside the helper's tests).
#   mornye/tests/test_launcher.sh
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
launcher="$here/../octo-chromium"
work="$(mktemp -d)"
trap 'chmod -R u+w "$work" 2>/dev/null; rm -rf "$work"' EXIT
failures=0

pass() { echo "ok    $1"; }
fail() { echo "FAIL  $1"; failures=$((failures + 1)); }

# The browser: prints its pid and its arguments, one per line, and exits with FAKE_EXIT.
cat > "$work/browser" <<'EOF'
#!/bin/sh
echo "pid $$"
for arg in "$@"; do echo "arg $arg"; done
exit "${FAKE_EXIT:-0}"
EOF
chmod +x "$work/browser"

launch() { # data folder, then the browser's arguments
  local data="$1"
  shift
  MORNYE_DATA_DIR="$data" OCTO_CHROMIUM="$work/browser" sh "$launcher" "$@" > "$work/out" 2> "$work/err"
}
pid_of_run() { sed -n 's/^pid //p' "$work/out"; }
args_of_run() { sed -n 's/^arg //p' "$work/out" | paste -sd '|' -; }

data="$work/data"
launch "$data" --headless=new "--user-data-dir=/tmp/a b" --remote-debugging-port=9333 --no-first-run
[ $? = 0 ] && pass "the browser runs" || fail "the browser did not run"
[ "$(args_of_run)" = "--headless=new|--user-data-dir=/tmp/a b|--remote-debugging-port=9333|--no-first-run" ] \
  && pass "the browser gets every argument as it was" || fail "the arguments changed: $(args_of_run)"
pid="$(pid_of_run)"
[ -n "$pid" ] && [ "$(cat "$data/octo-assist/browser-$pid.port" 2>/dev/null)" = 9333 ] \
  && pass "the port is noted under the browser's own pid" || fail "no port file for pid $pid: $(ls "$data/octo-assist" 2>&1)"
[ -z "$(find "$data/octo-assist" -name '*.tmp')" ] \
  && pass "no half written file is left" || fail "a .tmp file was left"

rm -rf "$data"
launch "$data" --remote-debugging-port 9444
pid="$(pid_of_run)"
[ "$(cat "$data/octo-assist/browser-$pid.port" 2>/dev/null)" = 9444 ] \
  && pass "the port given as its own argument is noted too" || fail "the two argument form was not noted"

rm -rf "$data"
launch "$data" --headless=new
[ "$(args_of_run)" = "--headless=new" ] && [ ! -e "$data/octo-assist" ] \
  && pass "without a DevTools port nothing is noted" || fail "a browser without a port left a file"

rm -rf "$data"
launch "$data" "--remote-debugging-port=12;rm -rf /"
[ ! -e "$data/octo-assist" ] && pass "a port that is not a number is never noted" || fail "a port that is not a number was noted"

# A data folder under a plain file cannot be made, by root either.
: > "$work/plain-file"
launch "$work/plain-file/data" --remote-debugging-port=9555
[ $? = 0 ] && [ "$(args_of_run)" = "--remote-debugging-port=9555" ] \
  && pass "a folder it cannot make never stops the browser" || fail "a folder it cannot make stopped the browser"
[ ! -s "$work/err" ] && pass "and it says nothing about it" || fail "it printed: $(cat "$work/err")"

if [ "$(id -u)" != 0 ]; then
  mkdir -p "$work/locked/octo-assist"
  chmod 0555 "$work/locked/octo-assist"
  launch "$work/locked" --remote-debugging-port=9666
  [ $? = 0 ] && [ -z "$(ls -A "$work/locked/octo-assist")" ] \
    && pass "a folder it cannot write never stops the browser" || fail "a folder it cannot write stopped the browser"
fi

FAKE_EXIT=7 launch "$data" --remote-debugging-port=9777
[ $? = 7 ] && pass "the browser's exit status is the launcher's" || fail "the exit status changed"

echo
[ "$failures" = 0 ] && echo "All launcher tests passed." || echo "$failures launcher test(s) failed."
[ "$failures" = 0 ]
