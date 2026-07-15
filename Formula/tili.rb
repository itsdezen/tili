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
  version "0.1.1"
  license "MIT"

  on_arm do
    url "https://github.com/itsdezen/tili/releases/download/v#{version}/tili-#{version}-aarch64-apple-darwin.tar.gz"
    sha256 "39a500ec3e13e0b5f32d01e11a6ca59e49d2ed83a8cf88897a2e4a31c0741eb9"
  end

  on_intel do
    url "https://github.com/itsdezen/tili/releases/download/v#{version}/tili-#{version}-x86_64-apple-darwin.tar.gz"
    sha256 "5971746c351520eef58b7d3ad55d91dab3446899e842d73b5b24d117e4d45e3c"
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
