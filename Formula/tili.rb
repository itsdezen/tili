# Kept in sync with the `itsdezen/homebrew-tap` repository's Formula/tili.rb
# (the one `brew install itsdezen/tap/tili` actually reads — see
# CONTRIBUTING.md's "Release Engineering" section) — this copy is versioned
# alongside the code it packages so it's easy to diff against a release.
# release.yml's `sync-homebrew-tap` job updates both this file and the
# homebrew-tap copy automatically after every tagged release; don't hand-
# edit `version`/`sha256` here, they get overwritten on the next release.
class Tili < Formula
  desc "i3-like tiling window manager for macOS"
  homepage "https://github.com/itsdezen/tili"
  version "0.1.9"
  license "MIT"

  on_arm do
    url "https://github.com/itsdezen/tili/releases/download/v#{version}/tili-#{version}-aarch64-apple-darwin.tar.gz"
    sha256 "e690949413fe914a702f24472faa2d4d64c1d16ac43b06d307c64b6a086d3619"
  end

  on_intel do
    url "https://github.com/itsdezen/tili/releases/download/v#{version}/tili-#{version}-x86_64-apple-darwin.tar.gz"
    sha256 "a4682c81cfb44ba09106c4732167fe423cf1993080ffe432bed964c51eb59d58"
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
    #
    # post_install runs inside Homebrew's install sandbox, which fakes
    # `$HOME`/`Dir.home` to a throwaway temp dir and denies filesystem
    # writes outside the Cellar/temp/log dirs (confirmed on real hardware:
    # a plain `Dir.home` here resolves to a `/private/tmp/...` sandbox
    # scratch dir, and writing to the real `~/Library/LaunchAgents` fails
    # with EPERM even once that's corrected) — so `tili stop`/`tili start`,
    # which need to rewrite/reload the LaunchAgent plist, can't run from
    # here. `Dir.home(ENV.fetch("USER"))` (the same trick Homebrew's own
    # sandbox.rb uses) resolves the *real* home via the user database
    # instead of the faked `$HOME` env var, which is enough for the
    # read-only existence check below. For the actual restart, just kill
    # the running processes instead of touching any LaunchAgent file:
    # `KeepAlive` in the already-loaded plist makes launchd relaunch them
    # immediately, through the same `bin/tili-daemon`/`bin/tili-menubar`
    # symlinks Homebrew has already relinked to this version by the time
    # post_install runs — sending a signal isn't a sandboxed filesystem
    # operation, so this works where a plist rewrite doesn't.
    real_home = Dir.home(ENV.fetch("USER"))
    daemon_plist = "#{real_home}/Library/LaunchAgents/com.tili.daemon.plist"
    return unless File.exist?(daemon_plist)

    system "pkill", "-x", "tili-daemon"
    system "pkill", "-x", "tili-menubar"
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
