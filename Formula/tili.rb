# Kept in sync with the `itsdezen/homebrew-tap` repository's Formula/tili.rb
# (the one `brew install itsdezen/tap/tili` actually reads — see
# CONTRIBUTING.md's "Release Engineering" section) — this copy is versioned
# alongside the code it packages so it's easy to diff against a release,
# but publishing a change means copying it into that separate repo too
# (manually today; automating that copy in release.yml's `publish` job is
# a reasonable future improvement).
class Tili < Formula
  desc "i3-like tiling window manager for macOS"
  homepage "https://github.com/itsdezen/tili"
  version "0.1.3"
  license "MIT"

  on_arm do
    url "https://github.com/itsdezen/tili/releases/download/v#{version}/tili-#{version}-aarch64-apple-darwin.tar.gz"
    sha256 "da5b3d4b0ae18eca9cc67c9fd33295bda5aa90aab1a86cb190919a3cbddfc7cd"
  end

  on_intel do
    url "https://github.com/itsdezen/tili/releases/download/v#{version}/tili-#{version}-x86_64-apple-darwin.tar.gz"
    sha256 "b33efa4b7f50cfed3c83c582f99a069816f1fd91ed626d70cb811e2dea97b536"
  end

  def install
    # The release tarball contains `tili.app/` (a minimal bundle wrapping
    # both binaries — see xtask/src/main.rs's `bundle` command, which
    # exists specifically so Accessibility permission and codesigning have
    # a stable, nameable bundle identifier to attach to rather than a bare
    # Unix executable). Install the bundle itself into the Cellar and
    # symlink the two executables into `bin`, same as any other formula's
    # binaries — a symlink doesn't invalidate the target's own signature.
    prefix.install "tili.app"
    bin.install_symlink prefix/"tili.app/Contents/MacOS/tili"
    bin.install_symlink prefix/"tili.app/Contents/MacOS/tili-daemon"
    bin.install_symlink prefix/"tili.app/Contents/MacOS/tili-menubar"
  end

  def post_install
    # Homebrew calls post_install after both a fresh `brew install` and
    # every `brew upgrade` — this is the hook that lets an upgrade take
    # effect immediately. If tili was already running under the previous
    # version (its LaunchAgent plist is present), restart it now so the
    # daemon/menu bar pick up the freshly installed binaries right away
    # instead of continuing to run the old ones until the user remembers
    # to `tili stop && tili start` by hand. A fresh install has no plist
    # yet, so this is a no-op then.
    daemon_plist = "#{Dir.home}/Library/LaunchAgents/com.tili.daemon.plist"
    return unless File.exist?(daemon_plist)

    system bin/"tili", "stop"
    system bin/"tili", "start"
  end

  def caveats
    <<~EOS
      tili-daemon needs Accessibility permission to manage windows:
        System Settings > Privacy & Security > Accessibility > add tili-daemon

      Try it out (also installs the menu bar workspace badge):
        tili start

      Remove tili's config, logs, socket, and Accessibility grant:
        tili uninstall
    EOS
  end

  test do
    system "#{bin}/tili", "--help"
  end
end
