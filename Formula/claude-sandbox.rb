# Homebrew formula for claude-sandbox.
#
# This repository doubles as its own tap. It is not named `homebrew-*`, so it is
# tapped with the two-argument form of `brew tap`, which takes the clone URL
# explicitly instead of deriving it from the tap name:
#
#   brew tap dpowers/claude-sandbox https://github.com/dpowers/claude_sandbox
#   brew trust dpowers/claude-sandbox
#   brew install --HEAD claude-sandbox
#
# The formula is head-only on purpose: there is no `url`/`sha256` pair naming a
# release tarball, so an install always builds the current `main` and a release
# is just a push. Two costs, both in the caveats below: `--HEAD` is required on
# the install (Homebrew 6 refuses a bare `brew install` for a head-only formula
# rather than inferring it), and `brew upgrade` skips HEAD kegs unless told to
# re-check the remote.
class ClaudeSandbox < Formula
  # Kept short on purpose: Homebrew caps "<name>: <desc>" at 80 characters.
  desc "Disposable, network-restricted VM per project, with Claude Code"
  homepage "https://github.com/dpowers/claude_sandbox"
  head "https://github.com/dpowers/claude_sandbox.git", branch: "main"

  # No livecheck block: it exists to spot new stable releases, and a head-only
  # formula has no stable version to compare against.

  # Where rustup puts its shims and its settings, honouring CARGO_HOME and
  # RUSTUP_HOME the way rustup itself does. Resolved at load time, while the
  # real HOME is still in scope: the build runs with HOME pointed at a sandbox
  # temp dir, so `~` cannot be expanded from inside `install`.
  #
  # The ENV.fetches are dead code in practice: bin/brew re-execs under
  # `env -i`, and a user's CARGO_HOME or RUSTUP_HOME survives only as a
  # renamed HOMEBREW_* copy that nothing on the formula path reads. A machine
  # with a non-default CARGO_HOME therefore misses this fast path and gets the
  # brewed Rust instead — harmless, just not the honouring the fetch suggests.
  CARGO_BIN = File.expand_path("#{ENV.fetch("CARGO_HOME", "~/.cargo")}/bin").freeze
  RUSTUP_CARGO = "#{CARGO_BIN}/cargo".freeze
  RUSTUP_ROOT = File.expand_path(ENV.fetch("RUSTUP_HOME", "~/.rustup")).freeze

  # Homebrew resolves `depends_on` against its own prefix and ignores rustup on
  # purpose — a formula's build is meant to be reproducible from Homebrew-managed
  # inputs alone. That costs a machine with a working `~/.cargo/bin/cargo` a
  # second Rust plus its runtime tail (llvm, python, z3, …), several GB to
  # duplicate a compiler that is already there. This tap is personal, so prefer
  # the toolchain on the machine and fall back to the brewed one when there is
  # none. The trade-off is accepted knowingly: the build now depends on whatever
  # rustup's default toolchain happens to be, so an old or broken one surfaces
  # here as a `brew install` failure. `brew audit` would object; it is not run.
  depends_on "rust" => :build unless File.executable?(RUSTUP_CARGO)
  # Every VM is an Apple `container` VM, which exists on Apple silicon only.
  depends_on arch: :arm64
  depends_on :macos

  # The launcher is a single self-contained binary — the Dockerfile and
  # entrypoint.sh are `include_str!`d into it at compile time — so there is
  # nothing to install but the executable.
  def install
    # superenv strips PATH back to Homebrew's own bin dirs, so rustup's shims
    # are invisible during the build unless they are put back. Harmless when the
    # brewed Rust is in play: that one is earlier in PATH regardless.
    #
    # RUSTUP_HOME rides along because PATH alone is not enough: the shim picks
    # its toolchain from $RUSTUP_TOOLCHAIN, a rust-toolchain file, or
    # $RUSTUP_HOME/settings.toml (defaulting to $HOME/.rustup), and the
    # build's HOME is a sandbox temp dir — without this the build dies with
    # "rustup could not choose a version of cargo to run". The real ~/.rustup
    # stays readable inside the build sandbox, which denies only credential
    # paths under the real home. Do NOT give CARGO_HOME the same treatment:
    # cargo *writes* the registry cache there during the build, writes to the
    # real home are denied, and the fake-HOME default is exactly where those
    # writes belong.
    if File.executable?(RUSTUP_CARGO)
      ENV.prepend_path "PATH", CARGO_BIN
      ENV["RUSTUP_HOME"] = RUSTUP_ROOT
    end
    system "cargo", "install", *std_cargo_args
  end

  def caveats
    <<~EOS
      This formula tracks `main`, and `brew upgrade` leaves HEAD installs alone
      because there is no version number to compare. To pick up new commits:

        brew upgrade --fetch-HEAD claude-sandbox

      Rust came from `~/.cargo/bin` if rustup had put one there, and from the
      `rust` formula otherwise — `brew install` would have said so either way.

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
