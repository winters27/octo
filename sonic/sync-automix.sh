#!/usr/bin/env bash
# Copies the transition analysis from the Octo apps' audio engine into
# src/automix, with the shared test vectors, so octo-sonic analyzes songs
# exactly as the players do.
#
#   ./sync-automix.sh [path to the OctoPlayer checkout]
#
# The default is OCTOPLAYER, else ../../OctoPlayer beside this repo. Run
# `cargo test` afterwards: the parity test checks the copy against the
# vectors. testdata/transition-profile.json is octo-sonic's own profile of
# its fixture song, written by `OCTO_WRITE_FIXTURE=1 cargo test`, so it is
# not copied.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
apps="${1:-${OCTOPLAYER:-$here/../../OctoPlayer}}"
engine="$apps/audio-engine/src/automix"
resources="$apps/shared/core/src/commonTest/resources"
files=(dsp.rs envelope.rs features.rs profile.rs synthetic.rs)

if [[ ! -f "$engine/profile.rs" ]]; then
  echo "No audio engine analysis at $engine" >&2
  exit 1
fi

for file in "${files[@]}"; do
  # Line endings as the repository keeps them.
  tr -d '\r' < "$engine/$file" > "$here/src/automix/$file"
done

# The constants the copied files take from the engine's automix/mod.rs
# (`super::NAME`), which is octo-sonic's own here: each must be one line
# there, and goes into engine_constants.rs as it is.
names="$(cd "$here/src/automix" && cat "${files[@]}" | grep -o 'super::[A-Z][A-Z0-9_]*' | cut -d: -f3 | sort -u || true)"
constants="//! The constants of the audio engine's \`automix/mod.rs\` that the copied
//! files use, written by \`sync-automix.sh\`."
for name in $names; do
  line="$(tr -d '\r' < "$engine/mod.rs" | grep -E "^pub const $name: [^=]+ = [^;]+;$" || true)"
  if [[ -z "$line" ]]; then
    echo "No one-line constant $name in $engine/mod.rs" >&2
    exit 1
  fi
  constants+=$'\n\n'"$line"
done
printf '%s\n' "$constants" > "$here/src/automix/engine_constants.rs"

tr -d '\r' < "$resources/automix-vectors.json" > "$here/testdata/automix-vectors.json"
echo "Copied the analysis from $apps ($(git -C "$apps" rev-parse --short HEAD 2>/dev/null || echo 'not a git checkout'))."
