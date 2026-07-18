# xtask — release/signing tooling (M11)

Part of the [architecture notes](../ARCHITECTURE.md).

`bundle` wraps `tili-daemon`/`tili` in a minimal `tili.app` at
`target/<target>/release/tili.app` (bundle id `com.tili.daemon` — the same
id `tili-cli`'s LaunchAgent uses, M10). `codesign` signs it with hardened
runtime + `xtask/entitlements.plist` (a bare `<dict/>` — **keep it free of
XML comments**; `codesign`'s entitlements parser, `AMFIUnserializeXML`, is
much stricter than a normal XML parser and rejects well-formed comments
with an opaque "syntax error near line N"). `package` runs `bundle`, then
`codesign` only if `TILI_SIGN_IDENTITY` is set in the environment, then
tars + sha256s — the single command `release.yml`'s `build` job calls per
target. `bundle`/`package` refuse to build if the release tag doesn't match
`Cargo.toml`'s workspace version (0.1.5), so a forgotten bump fails the
release instead of silently shipping a stale `--version` string.

`bundle` also converts the committed `assets/icon.png` (1024x1024) into
`Contents/Resources/AppIcon.icns` via `sips`/`iconutil` and sets
`CFBundleIconFile`/`CFBundleIconName` in `Info.plist` — this is what gives
`tili-daemon`/`tili-menubar` a real icon (instead of a generic one) in
System Settings > Privacy & Security > Accessibility and > General > Login
Items & Extensions, since both processes run from inside this same bundle.

Certificate generation itself is deliberately *not* automated anywhere (see
CONTRIBUTING.md's "Release engineering" section) — it's a one-time, human,
Keychain Access step, because the entire point of the self-signed-cert
strategy is that the identity never changes; automating its creation would
make it too easy to accidentally regenerate (which resets every user's
Accessibility grant, since TCC grants permission per signing identity).

`Formula/tili.rb` here is a copy of the real formula that lives in the
separate `itsdezen/homebrew-tap` repo (not auto-published — see that file's
own header comment for the sync process).
