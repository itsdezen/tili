//! Release/signing tooling for tili (M11). The single entry point CI (and
//! contributors cutting a local test build) calls instead of duplicating
//! bundling/signing/packaging logic as inline shell in a workflow file.
//!
//! **Certificate generation itself is deliberately not part of this
//! tool.** The whole point of the self-signed-cert release strategy (see
//! CONTRIBUTING.md's Release Engineering section) is that the certificate
//! is created *once*, by a human, and never regenerated except on forced
//! expiry — every regeneration resets every user's Accessibility grant.
//! Automating that step here would make it too easy to accidentally redo.

use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Parser, Subcommand};

const BUNDLE_ID: &str = "com.tili.daemon";

#[derive(Parser)]
#[command(name = "xtask", about = "Release/signing tooling for tili")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Wraps the already-built `tili-daemon`/`tili` release binaries for
    /// `target` in a minimal `.app` bundle at
    /// `target/<target>/release/tili.app` — gives Accessibility permission
    /// a stable, nameable entry to grant (bare Unix executables show up in
    /// System Settings with no icon/name and, more importantly, give
    /// codesigning nothing to attach an identity/bundle id to).
    Bundle {
        #[arg(long)]
        target: String,
        #[arg(long)]
        version: String,
    },
    /// Codesigns an existing `.app` bundle with a locally-installed
    /// identity (hardened runtime, minimal entitlements). Does *not*
    /// create or import the identity — that's the one-time human step
    /// documented in CONTRIBUTING.md.
    Codesign {
        #[arg(long)]
        app_path: PathBuf,
        #[arg(long)]
        identity: String,
    },
    /// `bundle`, then `codesign` if `TILI_SIGN_IDENTITY` is set in the
    /// environment (else ships unsigned with a warning, matching what
    /// release.yml already tells users in the draft release body), then
    /// tars up the bundle + docs and writes a sha256 — the single command
    /// release.yml's `build` job runs per target.
    Package {
        #[arg(long)]
        target: String,
        #[arg(long)]
        version: String,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Bundle { target, version } => bundle(&target, &version),
        Commands::Codesign { app_path, identity } => codesign(&app_path, &identity),
        Commands::Package { target, version } => package(&target, &version),
    }
}

fn app_bundle_path(target: &str) -> PathBuf {
    PathBuf::from(format!("target/{target}/release/tili.app"))
}

fn bundle(target: &str, version: &str) {
    let app = app_bundle_path(target);
    let macos_dir = app.join("Contents/MacOS");
    std::fs::create_dir_all(&macos_dir)
        .unwrap_or_else(|e| panic!("create {}: {e}", macos_dir.display()));

    for bin in ["tili-daemon", "tili"] {
        let src = PathBuf::from(format!("target/{target}/release/{bin}"));
        let dst = macos_dir.join(bin);
        std::fs::copy(&src, &dst).unwrap_or_else(|e| panic!("copy {}: {e}", src.display()));
    }

    // LSUIElement: tili-daemon is a background process, not a Dock app —
    // no icon, no app-switcher entry. CFBundleExecutable points at the
    // daemon specifically (not the CLI) since that's what a LaunchAgent
    // and Accessibility permission actually target.
    let info_plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>{BUNDLE_ID}</string>
    <key>CFBundleName</key>
    <string>tili</string>
    <key>CFBundleDisplayName</key>
    <string>tili</string>
    <key>CFBundleVersion</key>
    <string>{version}</string>
    <key>CFBundleShortVersionString</key>
    <string>{version}</string>
    <key>CFBundleExecutable</key>
    <string>tili-daemon</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSUIElement</key>
    <true/>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>NSHumanReadableCopyright</key>
    <string>MIT License</string>
</dict>
</plist>
"#
    );
    std::fs::write(app.join("Contents/Info.plist"), info_plist)
        .unwrap_or_else(|e| panic!("write Info.plist: {e}"));

    println!("xtask: bundled {}", app.display());
}

/// `xtask/entitlements.plist` is a bare `<dict/>` — tili isn't sandboxed
/// (it needs the Accessibility API and a Unix socket outside any
/// container) and needs no special entitlements beyond what hardened
/// runtime requires by default; the file only exists so `--options
/// runtime` has something to point at. Accessibility itself is a TCC
/// grant tied to the bundle id + signing identity, not a codesign
/// entitlement. Keep that file free of comments/extra content — the
/// `AMFIUnserializeXML` parser codesign uses for entitlements is far
/// stricter than a normal XML parser and rejects even well-formed XML
/// comments ("syntax error near line N").
fn codesign(app_path: &Path, identity: &str) {
    let status = Command::new("codesign")
        .args([
            "--force",
            "--deep",
            "--options",
            "runtime",
            "--entitlements",
            "xtask/entitlements.plist",
            "--sign",
            identity,
        ])
        .arg(app_path)
        .status()
        .expect("run codesign — is Xcode command line tools installed?");
    if !status.success() {
        eprintln!("xtask: codesign failed with {status}");
        std::process::exit(1);
    }
    println!("xtask: codesigned {} as '{identity}'", app_path.display());
}

fn package(target: &str, version: &str) {
    bundle(target, version);

    let app = app_bundle_path(target);
    match std::env::var("TILI_SIGN_IDENTITY") {
        Ok(identity) if !identity.is_empty() => codesign(&app, &identity),
        _ => println!(
            "xtask: TILI_SIGN_IDENTITY not set — packaging unsigned \
             (see CONTRIBUTING.md's Release Engineering section)"
        ),
    }

    let stage = format!("tili-{version}-{target}");
    let stage_dir = PathBuf::from(&stage);
    let _ = std::fs::remove_dir_all(&stage_dir);
    copy_dir(&app, &stage_dir.join("tili.app"))
        .unwrap_or_else(|e| panic!("copy app bundle into {stage}: {e}"));
    for doc in ["README.md", "LICENSE", "CHANGELOG.md"] {
        std::fs::copy(doc, stage_dir.join(doc)).unwrap_or_else(|e| panic!("copy {doc}: {e}"));
    }

    let tarball = format!("{stage}.tar.gz");
    let status = Command::new("tar")
        .args(["-czf", &tarball, &stage])
        .status()
        .expect("run tar");
    if !status.success() {
        eprintln!("xtask: tar failed with {status}");
        std::process::exit(1);
    }

    let sha_output = Command::new("shasum")
        .args(["-a", "256", &tarball])
        .output()
        .expect("run shasum");
    std::fs::write(format!("{tarball}.sha256"), &sha_output.stdout)
        .unwrap_or_else(|e| panic!("write {tarball}.sha256: {e}"));

    println!("xtask: packaged {tarball}");
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let dest_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}
