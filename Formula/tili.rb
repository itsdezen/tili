# Template for the `itsdezen/homebrew-tap` repository (M11) — see
# CONTRIBUTING.md's "Release Engineering" section. This file lives here so
# it's versioned alongside the code it packages; publishing it means
# copying the current version of this file into that separate tap repo's
# `Formula/tili.rb` after each release (or, later, automating that copy in
# release.yml's `publish` job).
#
# `sha256` values below MUST be replaced with the real values printed by
# `xtask package` (or the matching `*.tar.gz.sha256` file attached to the
# GitHub release) after each release — never left as placeholders in the
# actual tap repo.
class Tili < Formula
  desc "i3-like tiling window manager for macOS"
  homepage "https://github.com/itsdezen/tili"
  version "0.11.0"
  license "MIT"

  on_arm do
    url "https://github.com/itsdezen/tili/releases/download/v#{version}/tili-#{version}-aarch64-apple-darwin.tar.gz"
    sha256 "REPLACE_WITH_AARCH64_TARBALL_SHA256"
  end

  on_intel do
    url "https://github.com/itsdezen/tili/releases/download/v#{version}/tili-#{version}-x86_64-apple-darwin.tar.gz"
    sha256 "REPLACE_WITH_X86_64_TARBALL_SHA256"
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
  end

  def caveats
    <<~EOS
      tili-daemon needs Accessibility permission to manage windows:
        System Settings > Privacy & Security > Accessibility > add tili-daemon

      To start it automatically at login (opt-in, not done by this install):
        tili daemon install

      Otherwise, start it manually:
        tili-daemon &
    EOS
  end

  test do
    system "#{bin}/tili", "--help"
  end
end
