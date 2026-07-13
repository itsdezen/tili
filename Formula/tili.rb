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
  version "0.11.0"
  license "MIT"

  on_arm do
    url "https://github.com/itsdezen/tili/releases/download/v#{version}/tili-#{version}-aarch64-apple-darwin.tar.gz"
    sha256 "1a1a5f3c18e355301e342f4db6a83db32420ce0c2d7177252fffefa19be181cb"
  end

  on_intel do
    url "https://github.com/itsdezen/tili/releases/download/v#{version}/tili-#{version}-x86_64-apple-darwin.tar.gz"
    sha256 "c12dcf3df5a506f9828f0567524024cfc30486605326b6ce42f64a96753a76db"
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
