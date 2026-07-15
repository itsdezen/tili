#!/usr/bin/env bash
# Builds, bundles, (optionally) signs, and (re)starts tili from this
# checkout — for on-device testing without a real `brew install`. Mirrors
# what `xtask package` does for a release build, then finishes the loop by
# pointing PATH at the freshly built `tili.app` and restarting the daemon.
#
# Signing: set TILI_SIGN_IDENTITY to a Common Name in your keychain to sign
# with it (defaults to "tili Self-Signed", the CN convention documented in
# CONTRIBUTING.md's Release Engineering section). If no matching certificate
# exists, the app is bundled unsigned instead of failing — same as `xtask
# package`. Certificate generation itself is a one-time, human, Keychain
# Access step (see CONTRIBUTING.md) — this script never creates one.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

target="$(rustc -vV | awk '/^host:/ { print $2 }')"
version="$(awk -F'"' '/^version/ { print $2; exit }' Cargo.toml)"
identity="${TILI_SIGN_IDENTITY:-tili Self-Signed}"

echo "==> building release binaries for $target"
cargo build --release --target "$target" -p tili-daemon -p tili-cli

echo "==> bundling tili.app (version $version)"
cargo run -p xtask -- bundle --target "$target" --version "$version"

app="target/$target/release/tili.app"

if security find-certificate -c "$identity" >/dev/null 2>&1; then
    echo "==> signing with '$identity'"
    cargo run -p xtask -- codesign --app-path "$app" --identity "$identity"
else
    echo "==> no '$identity' certificate found in keychain — leaving unsigned"
    echo "    (see CONTRIBUTING.md's Release Engineering section to set one up)"
fi

export PATH="$PWD/$app/Contents/MacOS:$PATH"

echo "==> restarting daemon"
tili stop || true
tili start

cat <<MSG

==> done. This script's PATH export only applies to its own subprocess —
    run this in your current shell to keep using \`tili\` interactively:

    export PATH="$PWD/$app/Contents/MacOS:\$PATH"
MSG
