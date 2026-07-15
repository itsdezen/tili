#!/usr/bin/env bash
# Builds, bundles, and signs tili from this checkout — for on-device
# testing without a real `brew install`, without re-granting Accessibility,
# and without racing whatever's currently installed (e.g. the Homebrew
# release).
#
# Fully automated — just run it, then:
#   tili-dev start
#
# Stops whatever's currently running *first*, before anything else: signing
# below can trigger a macOS Keychain "always allow" password prompt
# (SecurityAgent) — confirmed on real hardware that a still-running
# tili-daemon mis-tiles that prompt. Doing the stop as this script's very
# first action, unconditionally, closes that race regardless of what was
# running or in what state.
#
# Installs the freshly built app as `tili-dev` — a thin wrapper (not a
# symlink) at /opt/homebrew/bin, already on PATH, so `tili-dev <command>`
# works in any terminal with no PATH export/sourcing needed. Deliberately a
# wrapper script, not `ln -s`: tili-cli's `sibling_binary_path` resolves
# `tili-daemon`/`tili-menubar` relative to `std::env::current_exe()`'s own
# directory — a symlink would make that resolve to /opt/homebrew/bin
# (the Homebrew release's siblings) instead of this app bundle's own
# Contents/MacOS (the freshly built ones), since `exec`ing a wrapper script
# hands the kernel the real path directly, with no symlink for
# current_exe() to second-guess.
#
# Signing: set TILI_SIGN_IDENTITY to a Common Name in your keychain to sign
# with it (defaults to "tili Self-Signed" — the same identity real releases
# use, so Accessibility permission carries over without a re-grant). If no
# matching certificate exists, the app is bundled unsigned instead of
# failing.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

echo "==> stopping whatever tili is currently running (if any)"
tili stop 2>/dev/null || true

target="$(rustc -vV | awk '/^host:/ { print $2 }')"
version="$(awk -F'"' '/^version/ { print $2; exit }' Cargo.toml)"
identity="${TILI_SIGN_IDENTITY:-tili Self-Signed}"

echo "==> building release binaries for $target"
cargo build --release --target "$target" -p tili-daemon -p tili-cli -p tili-menubar

echo "==> bundling tili.app (version $version)"
cargo run -p xtask -- bundle --target "$target" --version "$version"

app="$PWD/target/$target/release/tili.app"

if security find-certificate -c "$identity" >/dev/null 2>&1; then
    echo "==> signing with '$identity'"
    cargo run -p xtask -- codesign --app-path "$app" --identity "$identity"
else
    echo "==> no '$identity' certificate found in keychain — leaving unsigned"
fi

rm -f /opt/homebrew/bin/tili-local # old wrapper name from an earlier version of this script

wrapper="/opt/homebrew/bin/tili-dev"
echo "==> installing 'tili-dev' wrapper at $wrapper"
cat >"$wrapper" <<WRAPPER
#!/bin/sh
exec "$app/Contents/MacOS/tili" "\$@"
WRAPPER
chmod +x "$wrapper"

cat <<MSG

==> build ready — same signing identity as before, so Accessibility
    permission carries over (no re-grant needed). Now run:

    tili-dev start
MSG
