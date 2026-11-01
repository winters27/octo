#!/usr/bin/env bash
# Installs Octo's update helper on this host, so the dashboard's Update now
# button can update Octo: a systemd path unit watches Octo's config folder for
# an update request, and octo-updater.sh does the update (see that file).
#
#   scripts/updater/install-updater.sh             install, or refresh an install
#   scripts/updater/install-updater.sh --remove    take it off again
#
# Options:
#   --config DIR    Octo's config folder (default: OCTO_CONFIG_DIR from .env,
#                   else ./octo-config in the Octo folder)
#   --dry-run       the helper checks and reports, but changes nothing
#   --no-dry-run    back to real updates
#   --quiet         print only problems
#   --tidy          only remove the Mornye images (octo-mornye) this folder's release no
#                   longer runs, as every install and refresh does too; needs no systemd
#
# Needs Linux with systemd, and root (it asks for sudo when it is not root).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
octo_dir="$(cd "$here/../.." && pwd)"
lib_dir=/usr/local/lib/octo
unit_dir=/etc/systemd/system
config_dir="" remove=0 quiet=0 dryrun="" tidy=0

say() { [ "$quiet" = 1 ] || printf '%s\n' "$*"; }
die() { printf 'install-updater: %s\n' "$*" >&2; exit 1; }

# Octo's Mornye images (the spotiflac service): every octo-mornye tag but the one this folder's
# docker-compose.yml names, and the untagged ones earlier builds left (labeled
# app.octo.mornye=1), whether or not the spotiflac profile is on. An image a container still
# uses stays, and nothing here fails the install. The update into a release runs this from the
# new release, when the helper it replaces reinstalls the new one. A .env that still sets
# SPOTIFLAC_BUILD_REF, which nothing reads now, is told what to use instead.
tidy_mornye() {
  local ref current image
  ref="$({ sed -n 's/^SPOTIFLAC_BUILD_REF=//p' "$octo_dir/.env" 2>/dev/null || true; } | tail -n 1 | tr -d '"\r ')"
  if [ -n "$ref" ]; then
    printf '%s\n' "SPOTIFLAC_BUILD_REF is no longer read; Octo's Mornye is 1.0.5. To run another Mornye, set SPOTIFLAC_IMAGE."
  fi
  command -v docker > /dev/null 2>&1 || return 0
  current="$({ (cd "$octo_dir" && docker compose --profile spotiflac config --images spotiflac) 2>/dev/null || true; } | head -n 1)"
  [ -n "$current" ] || return 0
  for image in $({ docker image ls --format '{{.Repository}}:{{.Tag}}' octo-mornye 2>/dev/null || true; }); do
    case "$image" in "$current" | *:"<none>") continue ;; esac
    if docker image rm "$image" > /dev/null 2>&1; then
      say "Removed $image, which this release no longer runs."
    else
      say "$image is still in use, so it stays."
    fi
  done
  docker image prune -f --filter label=app.octo.mornye=1 > /dev/null 2>&1 || true
}

while [ $# -gt 0 ]; do
  case "$1" in
    --config) config_dir="${2:?--config needs a folder}"; shift 2 ;;
    --remove) remove=1; shift ;;
    --dry-run) dryrun=1; shift ;;
    --no-dry-run) dryrun=0; shift ;;
    --quiet) quiet=1; shift ;;
    --tidy) tidy=1; shift ;;
    *) die "unknown option $1 (see the top of this file)" ;;
  esac
done

if [ "$tidy" = 1 ]; then
  tidy_mornye
  exit 0
fi

[ -d /run/systemd/system ] || die "this host does not run systemd, so the helper cannot be installed. Update by hand: see \"Updating\" in the README."
if [ "$(id -u)" != 0 ]; then
  command -v sudo > /dev/null 2>&1 || die "run this as root"
  args=()
  [ -n "$config_dir" ] && args+=(--config "$config_dir")
  [ "$remove" = 1 ] && args+=(--remove)
  [ "$dryrun" = 1 ] && args+=(--dry-run)
  [ "$dryrun" = 0 ] && args+=(--no-dry-run)
  [ "$quiet" = 1 ] && args+=(--quiet)
  exec sudo "$0" "${args[@]}"
fi

if [ -z "$config_dir" ] && [ -f "$octo_dir/.env" ]; then
  config_dir="$(sed -n '/^OCTO_CONFIG_DIR=/{s/^OCTO_CONFIG_DIR=//;s/^"//;s/"$//;s/\r$//;p}' "$octo_dir/.env" | tail -n 1)"
fi
config_dir="${config_dir:-./octo-config}"
case "$config_dir" in /*) ;; *) config_dir="$octo_dir/${config_dir#./}" ;; esac
update_dir="${config_dir%/}/update"

# systemd reads % and quotes in unit files its own way, and | and & would upset the sed that
# fills the units in; such folders are refused rather than mangled.
for path in "$octo_dir" "$update_dir"; do
  case "$path" in *%*|*\"*|*\\*|*'|'*|*'&'*|*$'\n'*) die "the folder $path has a character systemd units cannot hold; move Octo to a plainer path" ;; esac
done

if [ "$remove" = 1 ]; then
  systemctl disable --now octo-updater.path > /dev/null 2>&1 || true
  rm -f "$unit_dir/octo-updater.path" "$unit_dir/octo-updater.service"
  rm -rf "$lib_dir"
  rm -f "$update_dir/helper"
  systemctl daemon-reload
  say "The update helper is removed. The dashboard shows the update command instead."
  exit 0
fi

[ -f "$octo_dir/docker-compose.yml" ] || die "$octo_dir has no docker-compose.yml; run this from Octo's folder"
command -v docker > /dev/null 2>&1 || die "docker is not installed"
# git only where the folder is a clone; a folder copied by hand is updated by pulling alone.
[ ! -e "$octo_dir/.git" ] || command -v git > /dev/null 2>&1 || die "git is not installed"
command -v flock > /dev/null 2>&1 || die "flock is not installed (util-linux)"

# A refresh keeps the dry run setting it had, unless told otherwise.
if [ -z "$dryrun" ]; then
  dryrun="$(sed -n 's/^Environment="OCTO_UPDATER_DRYRUN=\(.*\)"$/\1/p' "$unit_dir/octo-updater.service" 2>/dev/null || true)"
  [ "$dryrun" = 1 ] || dryrun=0
fi

# Every file is written aside and renamed into place, so a running helper never reads half of one.
place() { # mode, source, destination
  install -m "$1" "$2" "$3.tmp"
  mv -f "$3.tmp" "$3"
}
render() { # template, destination
  sed -e "s|@OCTO_DIR@|$octo_dir|g" -e "s|@UPDATE_DIR@|$update_dir|g" \
      -e "s|@LIB_DIR@|$lib_dir|g" -e "s|@DRYRUN@|$dryrun|g" "$1" > "$2.tmp"
  chmod 0644 "$2.tmp"
  mv -f "$2.tmp" "$2"
}

install -d -m 0755 "$lib_dir"
place 0755 "$here/octo-updater.sh" "$lib_dir/octo-updater.sh"
# Kept beside the script as they are, so a later release can tell whether they changed.
place 0644 "$here/octo-updater.path" "$lib_dir/octo-updater.path"
place 0644 "$here/octo-updater.service" "$lib_dir/octo-updater.service"
render "$here/octo-updater.path" "$unit_dir/octo-updater.path"
render "$here/octo-updater.service" "$unit_dir/octo-updater.service"
install -d -m 0755 "$update_dir"

systemctl daemon-reload
systemctl enable octo-updater.path > /dev/null 2>&1
# A restart picks up a changed folder in an existing install.
systemctl restart octo-updater.path

OCTO_DIR="$octo_dir" OCTO_UPDATE_DIR="$update_dir" "$lib_dir/octo-updater.sh" --describe
tidy_mornye

say "The update helper is installed."
say "  Octo folder:   $octo_dir"
say "  Watching:      $update_dir/request"
[ "$dryrun" = 1 ] && say "  Dry run:       on (nothing is changed; --no-dry-run turns it off)"
say "The dashboard's About page can now update Octo. To remove it: $0 --remove"
