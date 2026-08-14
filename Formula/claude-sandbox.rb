# Homebrew formula for claude-sandbox.
#
# This repository doubles as its own tap. It is not named `homebrew-*`, so it is
# tapped with the two-argument form of `brew tap`, which takes the clone URL
# explicitly instead of deriving it from the tap name:
#
#   brew tap dpowers/claude-sandbox https://github.com/dpowers/claude_sandbox
#   brew install claude-sandbox
#
# The formula is head-only on purpose: there is no `url`/`sha256` pair naming a
# release tarball, so `brew install` always builds the current `main` and a
# release is just a push. The cost is that `brew upgrade` skips HEAD kegs unless
# it is told to re-check the remote — see the caveats below.
class ClaudeSandbox < Formula
  # Kept short on purpose: Homebrew caps "<name>: <desc>" at 80 characters.
  desc "Disposable, network-restricted VM per project, with Claude Code"
  homepage "https://github.com/dpowers/claude_sandbox"
  head "https://github.com/dpowers/claude_sandbox.git", branch: "main"

  # No livecheck block: it exists to spot new stable releases, and a head-only
  # formula has no stable version to compare against.

  depends_on "rust" => :build
  # Every VM is an Apple `container` VM, which exists on Apple silicon only.
  depends_on arch: :arm64
  depends_on :macos

  # The launcher is a single self-contained binary — the Dockerfile and
  # entrypoint.sh are `include_str!`d into it at compile time — so there is
  # nothing to install but the executable.
  def install
    system "cargo", "install", *std_cargo_args
  end

  def caveats
    <<~EOS
      This formula tracks `main`, and `brew upgrade` leaves HEAD installs alone
      because there is no version number to compare. To pick up new commits:

        brew upgrade --fetch-HEAD claude-sandbox

      claude-sandbox drives Apple's `container` CLI, which is deliberately not a
      formula dependency: an existing install from Apple's signed .pkg would end
      up shadowed by a second copy, and installing the formula stops any running
      container services. Install it yourself if you have not already:

        brew install container
        container system start

      Opening a project (`claude-sandbox <dir>`) also needs Zed's `zed` CLI on
      PATH. `claude-sandbox shell <dir>` needs only `ssh`.
    EOS
  end

  test do
    help = shell_output("#{bin}/claude-sandbox --help")
    assert_match "Usage: claude-sandbox", help
    assert_match "Rebuild every image layer", help

    assert_match "claude-sandbox", shell_output("#{bin}/claude-sandbox --version")

    # Neither a directory nor a subcommand is a clap usage error: clap prints to
    # stderr and exits 2, which is not the 1 a hand-rolled usage error would use.
    assert_match "Usage: claude-sandbox", shell_output("#{bin}/claude-sandbox 2>&1", 2)
  end
end
