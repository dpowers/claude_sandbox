# Homebrew formula for claude-sandbox.
#
# This repository doubles as its own tap. It is not named `homebrew-*`, so it is
# tapped with the two-argument form of `brew tap`, which takes the clone URL
# explicitly instead of deriving it from the tap name:
#
#   brew tap dpowers/claude-sandbox https://github.com/dpowers/claude_sandbox
#   brew install claude-sandbox
#
# `url` and `sha256` are rewritten by scripts/bump-formula.sh, which the release
# workflow runs on every `v*` tag — edit the version in Cargo.toml, tag, and let
# CI point the formula at the new tarball.
class ClaudeSandbox < Formula
  # Kept short on purpose: Homebrew caps "<name>: <desc>" at 80 characters.
  desc "Disposable, network-restricted VM per project, with Claude Code"
  homepage "https://github.com/dpowers/claude_sandbox"
  url "https://github.com/dpowers/claude_sandbox/archive/refs/tags/v0.1.0.tar.gz"
  # All zeros means the tag above has not been published yet — GitHub only
  # generates a tag's tarball once the tag exists, so the checksum cannot be
  # known before then. Install with `--HEAD` until it is.
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  head "https://github.com/dpowers/claude_sandbox.git", branch: "main"

  # No livecheck block: the default Git strategy already reads `v*` tags off
  # the repository behind the archive URL, and does it without spending GitHub
  # API requests the way the `github_latest` strategy would.

  depends_on "rust" => :build
  depends_on :macos
  # Every VM is an Apple `container` VM, which exists on Apple silicon only.
  depends_on arch: :arm64

  # The launcher is a single self-contained binary — the Dockerfile and
  # entrypoint.sh are `include_str!`d into it at compile time — so there is
  # nothing to install but the executable.
  def install
    system "cargo", "install", *std_cargo_args
  end

  def caveats
    <<~EOS
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
    assert_match "claude-sandbox shell", shell_output("#{bin}/claude-sandbox --help")
    # No arguments is a usage error: usage on stderr, exit 1.
    assert_match "usage:", shell_output("#{bin}/claude-sandbox 2>&1", 1)
  end
end
