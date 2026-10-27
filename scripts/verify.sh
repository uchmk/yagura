#!/usr/bin/env bash
# Every check a push to main must pass, in the order CI would fail them, from
# a Linux machine (a cloud session) or the owner's Windows PC. The first
# failure stops with its output.
#
#   scripts/verify.sh
#
# The same steps as kura's. Besides the host's own clippy, every other target
# CI builds (Linux, Windows, macOS) gets one: those are type checks only --
# they never run that code. No dependency compiles C for any of them, so plain
# clippy does it from any machine (ito, tsumugi and minato go through
# cargo-zigbuild for that). Before 2026-10-15 a run on the Windows PC never
# compiled Linux and still said ALL OK, and v0.62.0 went red on Linux.

set -o pipefail
cd "$(dirname "$0")/.." || exit 1

host=$(rustc -vV | sed -n 's/^host: //p')
others=
for t in x86_64-unknown-linux-gnu x86_64-pc-windows-msvc aarch64-apple-darwin; do
    [ "$t" = "$host" ] && continue
    others="$others $t"
    rustup target list --installed 2>/dev/null | grep -qx "$t" || rustup target add "$t" >/dev/null 2>&1
done

run() {
    local out rc
    out=$("$@" 2>&1)
    rc=$?
    if [ $rc -ne 0 ]; then
        echo "FAILED: $*"
        echo "$out" | tail -30
        exit 1
    fi
    last=$out
}

# `--locked` first: CI builds that way, and a plain cargo command quietly
# rewrites a stale Cargo.lock. When this fails, run `cargo build` and commit
# Cargo.lock with the version bump.
run cargo build -q --locked --workspace
# the shared scripts are ito's, byte for byte (scripts/lanes.sh)
run scripts/lanes.sh check
run python3 scripts/merge-lanes.py --self-test
run cargo fmt --check
run cargo test -q --workspace
tests=$(echo "$last" | grep -m1 'test result')
run cargo +stable clippy -q --workspace --all-targets -- -D warnings
for t in $others; do
    run cargo +stable clippy -q --workspace --all-targets --target "$t" -- -D warnings
done
# The Windows test lane's scripts (scripts/check-ps1.ps1; CI runs it too).
if command -v pwsh >/dev/null; then
    run pwsh -NoProfile -File scripts/check-ps1.ps1
else
    echo "(no pwsh: scripts/check-ps1.ps1 left to CI)"
fi
echo "ALL OK: $tests"
