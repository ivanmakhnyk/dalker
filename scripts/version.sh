#!/bin/sh
# The release version lives in one place: [workspace.package] version in Cargo.toml. Every crate
# inherits it (version.workspace = true); the .deb, the MSI and the agent's /api/info read it too.
#   scripts/version.sh            print the version
#   scripts/version.sh check      fail if a crate or Cargo.lock disagrees with it
#   scripts/version.sh set 1.2.0  change it (and Cargo.lock)
set -e
ROOT=$(cd "$(dirname "$0")/.." && pwd)
TOML="$ROOT/Cargo.toml"

current() {
    # (sub(/\r$/) keeps this working on a Windows checkout with CRLF line endings)
    awk '{ sub(/\r$/, "") } /^\[/ { pkg = ($0 == "[workspace.package]") }
         pkg && /^version[ \t]*=/ { sub(/^[^"]*"/, ""); sub(/".*/, ""); print; exit }' "$TOML"
}

case "${1:-}" in
"")
    current
    ;;
check)
    v=$(current)
    [ -n "$v" ] || { echo "no version in [workspace.package] of Cargo.toml" >&2; exit 1; }
    echo "$v" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$' ||
        { echo "version $v is not X.Y.Z or X.Y.Z-pre" >&2; exit 1; }
    bad=0
    for f in "$ROOT"/crates/*/Cargo.toml; do
        grep -q '^version.workspace = true' "$f" ||
            { echo "$f: use version.workspace = true" >&2; bad=1; }
    done
    # Cargo.lock records each workspace crate's version; --locked builds fail if it is stale.
    for c in $(sed -n 's/^name = "\(dalker-[a-z]*\)"\r*$/\1/p' "$ROOT/Cargo.lock"); do
        lv=$(awk -v c="$c" '{ sub(/\r$/, "") } $0 == "name = \"" c "\"" { getline; sub(/^version = "/, ""); sub(/"\r?$/, ""); print; exit }' "$ROOT/Cargo.lock")
        [ "$lv" = "$v" ] || { echo "Cargo.lock: $c is $lv, Cargo.toml says $v (run scripts/version.sh set $v)" >&2; bad=1; }
    done
    [ $bad = 0 ] && echo "version $v"
    exit $bad
    ;;
set)
    new=${2:?usage: version.sh set X.Y.Z}
    awk -v new="$new" '/^\[/ { pkg = ($0 ~ /^\[workspace\.package\]\r?$/) }
        pkg && !done && /^version[ \t]*=/ { $0 = "version = \"" new "\""; done = 1 } { print }' \
        "$TOML" > "$TOML.tmp" && mv "$TOML.tmp" "$TOML"
    # Rewrite the workspace crates' entries in Cargo.lock without touching dependencies. Whatever
    # version they have: a merge can bring in a crate that still has an older one.
    awk -v new="$new" '/^name = "dalker-/ { print; getline;
        sub(/^version = "[^"]*"/, "version = \"" new "\"") } { print }' \
        "$ROOT/Cargo.lock" > "$ROOT/Cargo.lock.tmp" && mv "$ROOT/Cargo.lock.tmp" "$ROOT/Cargo.lock"
    sh "$0" check
    ;;
*)
    echo "usage: version.sh [check | set X.Y.Z]" >&2
    exit 2
    ;;
esac
