#!/usr/bin/env bash
# Prepare hooks for hyperfine. Not timed. Paths are absolute.
#
#   reset.sh fresh <work> <src> [cache-dir...]   empty <work> (keeping the dir itself, which is
#                                                hyperfine's cwd), copy <src>/. into it, and
#                                                delete the given cache dirs
#   reset.sh nm <work> <src>                     delete every node_modules under <work>, then
#                                                restore project files (manifests, lockfile) from <src>
set -euo pipefail

mode=$1 work=$2 src=$3
shift 3

clear_dir() {
  find "$1" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
}

case "$mode" in
  fresh)
    mkdir -p "$work"
    clear_dir "$work"
    for cache in "$@"; do rm -rf "$cache"; done
    cp -a "$src/." "$work/"
    ;;
  nm)
    find "$work" -name node_modules -type d -prune -exec rm -rf {} +
    cp -a "$src/." "$work/"
    ;;
  *)
    echo "reset.sh: unknown mode $mode" >&2
    exit 2
    ;;
esac
