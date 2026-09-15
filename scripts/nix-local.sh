#!/usr/bin/env bash
# Evaluate an untracked checkout without copying target/ into the Nix store.
# Tracked checkouts can use ordinary `nix build`, `nix develop`, and `nix run`.
set -euo pipefail

ttybird_repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
ttybird_action=${1:-check}
if [[ $# -gt 0 ]]; then shift; fi
case "$ttybird_action" in
  build|check|develop|run) ;;
  *)
    echo "Usage: $0 [build|check|develop|run] [arguments...]" >&2
    exit 2
    ;;
esac

ttybird_stage=$(mktemp -d "${TMPDIR:-/tmp}/ttybird-nix-local.XXXXXX")
ttybird_stage=$(cd "$ttybird_stage" && pwd -P)
trap 'rm -rf "$ttybird_stage"' EXIT
for ttybird_source in flake.nix flake.lock Cargo.toml Cargo.lock LICENSE src tests examples nix; do
  cp -R "$ttybird_repo/$ttybird_source" "$ttybird_stage/"
done

cd "$ttybird_repo"
case "$ttybird_action" in
  build) nix build "path:$ttybird_stage" --out-link "$ttybird_repo/result" "$@" ;;
  check) nix flake check "path:$ttybird_stage" "$@" ;;
  develop) nix develop "path:$ttybird_stage" "$@" ;;
  run) nix run "path:$ttybird_stage" -- "$@" ;;
esac
