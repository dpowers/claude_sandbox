# claude-sandbox

Run a project directory inside a disposable, network-restricted VM with Claude
Code installed, and open [Zed](https://zed.dev) on the host connected to it over
SSH.

```
claude-sandbox ~/Projects/my-app
```

That command builds the image if needed, boots a VM for the project, bind-mounts
the directory into it, waits for `sshd`, and execs `zed ssh://…`. Close the Zed
window and the VM stops and deletes itself.

## Purpose

Claude Code is most useful when it can run commands, but a coding agent with a
shell is also a machine on your LAN. `claude-sandbox` gives each project its own
throwaway Linux VM where:

- **the local network is unreachable** — outbound connections to RFC1918,
  link-local, CGNAT, and multicast space are rejected by an `nftables` rule
  applied at boot, so nothing in the VM can reach your router, NAS, printers,
  or other machines;
- **nothing in the VM can become root** — the account you ssh in as has no
  `sudo`, which is what keeps the rule above from being one command away from
  deletion. [`--sudo`](#root-inside-the-vm) opts back in when you need it;
- **the Internet still works** — public destinations are allowed, plus DNS to
  the configured resolvers (which usually *are* on the LAN), so `npm install`,
  `git push`, and the Claude API all work normally;
- **the blast radius is one directory** — the only host path mounted is the
  project you named;
- **it fails closed** — if the firewall rules can't be applied, the entrypoint
  exits and `sshd` never starts.

Your Claude Code login is shared across VMs (via `~/.claude-sandbox`), so you
authenticate once rather than per project. A project that needs more than the
base image can carry its own Dockerfile, and you can layer one onto every
project at once — see [Image overlays](#image-overlays). A project's is gated
behind an explicit acceptance step, for the same reasons as everything above.

## Requirements

- **macOS on Apple silicon** with Apple's [`container`](https://github.com/apple/container)
  CLI (developed against v1.2.2; installation below). Each container gets its
  own VM, which is what makes the in-guest firewall trustworthy and lets the
  guest hold `CAP_NET_ADMIN` without weakening the host.
- **Rust** (stable, edition 2021) to build the launcher — Homebrew installs it
  for you if you go that route.
- **[Zed](https://zed.dev)** on the host, with the `zed` CLI on `PATH` — only
  for the default `up` command; `claude-sandbox shell` needs just `ssh`. Each
  VM is a host Zed has never seen, so projects open in Restricted Mode until
  you trust them — see [Restricted Mode](#5-lift-zeds-restricted-mode).
- **macOS Local Network permission** for the terminal app you launch from:
  System Settings → Privacy & Security → Local Network. VMs live on
  `192.168.64.0/24`; without the grant, connections fail as "No route to host"
  rather than as a permission error. `claude-sandbox` detects this case and
  says so. Zed.app carries its own grant, so the VM may be reachable from Zed
  even when your terminal can't reach it.

## Getting started

### 1. Install the `container` CLI

```sh
brew install container
container system start      # first start downloads a Linux kernel for the VMs
```

`container` is Apple's container runtime for Apple silicon; if you prefer not
to use Homebrew, a signed installer package is available from its
[releases page](https://github.com/apple/container/releases).
`container system start` launches its background services — the launcher
starts them automatically when they're down, so this is a one-time sanity
check. Verify with `container system status`.

### 2. Create the local DNS domain (optional but recommended)

VMs get hostnames under a local DNS domain served by the runtime. The default
domain is `container` (the launcher reads it from `[dns] domain` in
`container system property ls`). Creating it lets ssh and Zed reach VMs by
name — `claude-sandbox-my-app-a1b2c3.container` — and also writes the
`/etc/resolver/` entry macOS needs to route those lookups:

```sh
sudo container system dns create container
container system dns list        # should now list: container
```

Skipping this is fine: the launcher pins each VM's IP into its managed ssh
config, so everything still works — you just get a startup warning and can't
reach VMs by bare hostname. If name resolution breaks later (say the
`/etc/resolver/` file went missing), recreate the domain:

```sh
sudo container system dns delete container
sudo container system dns create container
```

### 3. Install the launcher

With Homebrew — this repository is also its own tap:

```sh
brew tap dpowers/claude-sandbox https://github.com/dpowers/claude_sandbox
brew install claude-sandbox
```

The URL is not optional: the short `brew tap dpowers/claude-sandbox` form looks
for a repository named `homebrew-claude-sandbox`, and this one is called
`claude_sandbox`. Passing the URL says where the tap actually lives; you only
do it once, and `brew update` follows it from then on. Homebrew builds from
source and pulls in Rust as a build dependency (`brew autoremove` drops it
again later, if you have no other use for it).

The formula is head-only: there is no release tarball, so `brew install` always
builds the current `main`. That also means upgrades need `--fetch-HEAD`, because
plain `brew upgrade` skips HEAD installs — there is no version number for it to
compare against:

```sh
brew upgrade --fetch-HEAD claude-sandbox
```

Or from a clone, without Homebrew:

```sh
cargo build --release
ln -s "$PWD/target/release/claude-sandbox" /usr/local/bin/claude-sandbox
```

### 4. Run it

```sh
claude-sandbox ~/Projects/my-app     # first run builds the image (a few minutes)
```

The first connection to a VM may trigger the macOS Local Network permission
prompt for your terminal — allow it, or every later connection fails with
"No route to host" (see [Requirements](#requirements)).

The VM gets an account named after whoever runs the launcher — your host
username, lowercased and sanitized for Linux — so the project lands at
`~/Projects/<name>` inside the VM just as it does on the host. Run `claude`
there. The first run will prompt you to log in to Claude Code; that login
persists to `~/.claude-sandbox` on the host and is reused by every later VM.

### 5. Lift Zed's Restricted Mode

Zed opens any worktree it has not been told to trust in **Restricted Mode**:
language servers and MCP servers are not downloaded or started, and the
project's `.zed/settings.json` is ignored. Trust is tracked per host, and a
fresh VM is a host Zed has never seen, so every new sandbox starts restricted.

Two ways out, both host-side:

- **Per project** — click the Restricted Mode indicator in Zed's title bar (or
  run `workspace::ToggleWorktreeSecurity`) and trust the worktree. Zed stores
  that against the VM's ssh name, which is derived from the project path and
  so is identical for every future VM of that project: trust it once and it
  stays trusted across Zed restarts and VM recreations.
- **For everything** — add to `~/.config/zed/settings.json`:

  ```json
  "session": { "trust_all_worktrees": true }
  ```

  No prompt ever again, at the cost of being global: projects you open
  locally are auto-trusted too. Auto-trust is not persisted per worktree, so
  turning the setting back off restores per-project decisions.

Neither knob lives in the VM or in this repo — trust is Zed's own state on the
host, which is why the launcher can't grant it for you. It prints a note about
this the first time it opens Zed and then keeps quiet; delete
`~/.config/claude-sandbox/zed-trust-notice` to see it again.

### Commands

| Command | What it does |
| --- | --- |
| `claude-sandbox <dir>` | Start the VM (building/creating as needed) and open Zed |
| `claude-sandbox shell <dir> [cmd…]` | Same, but `ssh` in instead of opening Zed |
| `claude-sandbox stop <dir>` | Stop the project's VM (it deletes itself on stop) |
| `claude-sandbox rm <dir>` | Stop and delete the VM, and drop its ssh-config block |
| `claude-sandbox overlay [action] <dir>` | Inspect or manage the project's [image overlay](#image-overlays) |
| `claude-sandbox rebuild [opts] <dir>` | Rebuild every layer from scratch — no build cache, and the OS image is re-pulled — then stop. Builds images and nothing else: no VM is created and no editor is opened. See [Keeping images up to date](#keeping-images-up-to-date) |

### Options

Accepted before the directory, and (except with `stop`/`rm`/`overlay`, which
reject them) after it as well. Both `-m 12g` and `--memory=12g` forms work.

| Option | Effect |
| --- | --- |
| `-m`, `--memory <size>` | Memory ceiling for the VM, default `8g`. A `K`/`M`/`G`/`T`/`P` suffix is required — the runtime reads a bare number as mebibytes, so `--memory 8` would mean 8 MiB |
| `-c`, `--cpus <n>` | vCPUs for the VM, default `6`. The runtime adds one vCPU of overhead, so the guest kernel reports `n + 1` |
| `--sudo` | Give the VM's account passwordless root — see [Root inside the VM](#root-inside-the-vm) |
| `--use-cache` | Only in the `rebuild` mode: let the builder answer from cache, so the rebuild picks up Dockerfile edits and nothing else. Seconds instead of minutes |
| `--no-overlay` | Ignore both [image overlays](#image-overlays) for this run and boot the plain base image |
| `--accept-overlay` | Accept the project overlay's current contents without prompting — for scripts, where there is no one to ask |
| `-h`, `--help` | Usage for the launcher, or for one mode with `claude-sandbox <mode> --help` |
| `-V`, `--version` | Print the launcher's version |

`-m`/`-c` are read **when the VM is created**. A running VM keeps the limits it
was created with, so passing different ones prints a note telling you to
`claude-sandbox rm <dir>` first rather than silently doing nothing. The modes
that never create a VM — `stop`, `rm`, `overlay`, `rebuild` — reject them
outright instead of accepting them and doing nothing.

The defaults are deliberately not the host's full core count and RAM. There is
one VM per project, so the ceilings multiply across everything you have open;
vCPUs are time-sliced against the host anyway, and past the performance-core
count the guest only schedules onto efficiency cores it cannot tell apart. The
runtime's own defaults (1 GiB / 4 cpus) are the opposite problem — Claude Code
is a Node process before `rustc` or a language server starts, and a single link
step can outgrow the rest of that gigabyte.

### Environment

| Variable | Effect |
| --- | --- |
| `CLAUDE_SANDBOX_IDLE` | Idle timeout in seconds, applied when the VM is created (`0` = never reap) |
| `CLAUDE_SANDBOX_DEBUG` | Keep failed VMs around (skips `--rm`) so `container logs <name>` works |
| `CLAUDE_SANDBOX_USER` | Account name inside the VM (default: your host username) |
| `CLAUDE_SANDBOX_STATE` | Override the state directory (default `~/.config/claude-sandbox`) |
| `CLAUDE_SANDBOX_RESEED` | Overwrite the sandbox's Claude credentials from the host keychain. Normally the host's copy is only written when it outlives the sandbox's, since replacing a fresher token with a staler one can invalidate the newer session |
| `CLAUDE_SANDBOX_ACCEPT_OVERLAY` | Accept overlay contents without prompting, same as `--accept-overlay` |
| `CLAUDE_SANDBOX_SUDO` | Passwordless root in every VM, same as `--sudo`. Unlike the others, the value is read: `0`, `false`, `no`, `off` and empty mean off |

### Claude Code config in the VM

Claude Code inside the VM is seeded from the host on every run: OAuth tokens
exported from the login Keychain, plus `settings.json`, `CLAUDE.md`, and your
`agents`, `commands`, `skills`, `output-styles` and `plugins`.

It is a one-way copy into `~/.claude-sandbox`, which is what gets mounted as
`~/.claude` in the guest — never a mount of `~/.claude` itself. So transcripts
stay on the host, and nothing the VM writes can reach config the host later
executes. Credentials are the one exception to "every run": the host's copy is
only written when it outlives the sandbox's, since overwriting a fresher token
with a staler one can invalidate the newer session. `CLAUDE_SANDBOX_RESEED=1`
forces it.

### Root inside the VM

By default the account you ssh in as is not in the `sudo` group and has no
sudoers entry, so nothing inside the VM can become root. Root login over SSH is
off and root's own password is locked, so there is no other way in either.

That default is what the egress firewall rests on. The `nftables` rules are
enforced by the guest's own kernel and the guest holds `CAP_NET_ADMIN`, so root
in there could delete the whole table with the same one-line command the
entrypoint uses to replace it. The same goes for the read-only
`.claude-sandbox/` mount, which root can simply remount `rw`. Without the sudo
default, both are guardrails against accident rather than barriers against a
prompt-injected or otherwise adversarial agent.

The cost is that anything writing outside your home directory fails:
`apt-get install`, `npm install -g`, `mount`, binding a port below 1024. The
intended home for those is an [image overlay](#image-overlays), which runs as
root at build time on the host.

`--sudo` (or `CLAUDE_SANDBOX_SUDO=1`) restores the previous behaviour —
`NOPASSWD:ALL` for the account:

```
claude-sandbox --sudo ~/Projects/my-app
```

Nothing enforced *inside* the VM survives that. What still holds is everything
enforced outside it: the VM boundary itself, the fact that the only host paths
mounted are the project directory and `~/.claude-sandbox`, and the
[acceptance gate](#accepting-an-overlay) that keeps the host from building
image contents a human has not read.

The two variants are separate images (`claude-sandbox:<user>` and
`claude-sandbox:<user>-sudo`). Adding or dropping the flag therefore builds a
new one and recreates the VM — cheap, because they diverge only in the last few
small layers and share the whole toolchain beneath. The launcher prints a line
on every launch where `--sudo` is in effect, since it can arrive from the
environment rather than from the command you typed.

### Lifecycle

A VM deletes itself about **15 seconds** after its last SSH session ends (Zed
window closed, shell exited). A freshly booted VM gets **2 minutes** to receive
its first connection. Reopening a project recreates it in a couple of seconds —
these VMs are meant to be disposable, and all durable state lives in the two
mounts.

## Image overlays

The base image is deliberately thin — Ubuntu, Node, git, the Claude Code CLI,
and a C/C++ toolchain (`build-essential` + `pkg-config`) for the native builds
most language ecosystems fall back to. Two layers can extend it, and a VM's
image is whichever of them exist, stacked:

```
claude-sandbox:<user>                     base (…-sudo under --sudo)
  └─ ~/.config/claude-sandbox/global/     yours, applied to every project
       └─ <project>/.claude-sandbox/      the project's own
```

These are also where system packages belong now that [nothing in the VM runs as
root](#root-inside-the-vm): an overlay's `RUN` executes as root on the host's
builder, so `apt-get install` works there even though it does not inside the
running VM.

### The global overlay

Most people want the same handful of tools in every sandbox. Put them in a
Dockerfile in your own config directory, and every project picks them up:

```dockerfile
# ~/.config/claude-sandbox/global/Dockerfile
RUN apt-get update && apt-get install -y --no-install-recommends \
        ripgrep fd-find jq \
    && rm -rf /var/lib/apt/lists/*
```

The rules are the same as for a project overlay below — no `FROM` line,
`$USERNAME` available, and the directory is the build context, so a `COPY`
reads files sitting next to the Dockerfile. Edit it and the next launch of any
project rebuilds. It gets a directory of its own rather than a bare
`~/.config/claude-sandbox/Dockerfile` because a build context is a whole
directory, and using the state directory itself would ship your SSH private key
to the builder.

**No acceptance step, unlike a project overlay.** This file is not in any
repository, and no sandbox can reach it: a VM mounts only the project directory
and `~/.claude-sandbox`, never the state directory. There is nobody to gate
against, and prompting you to approve a file you just edited yourself would
only train the reflex that dismisses the prompt that does matter.

The exception is sandboxing a directory that *contains* the state directory —
your home directory, or a dotfiles repo. The guest can rewrite the global
overlay then, so the launcher says so and skips it. Be aware that in that
situation the mount also hands the guest `~/.config/claude-sandbox/id_ed25519`,
which is the key every other sandbox is reached with; the launcher warns about
that too, but it does not currently refuse the mount.

### The per-project overlay

When one project needs more than your standard set, put a Dockerfile in the
project itself:

```
<project>/.claude-sandbox/Dockerfile
```

It is layered **on top of** the global overlay (or the base image, if you have
no global one), so it starts where that leaves off. Write only the extra
layers — there is **no `FROM` line**, because the launcher supplies it:

```dockerfile
RUN apt-get update && apt-get install -y --no-install-recommends \
        golang-go postgresql-client \
    && rm -rf /var/lib/apt/lists/*

# The account you ssh in as is available as $USERNAME.
COPY gitconfig /home/$USERNAME/.gitconfig
RUN chown "$USERNAME:$USERNAME" "/home/$USERNAME/.gitconfig"
```

The directory is the build context, so that `COPY` reads
`.claude-sandbox/gitconfig`. The project itself is *not* in the context — it is
bind-mounted at run time, long after the image is built — so an overlay can
install toolchains but cannot bake in your source.

`RUN` executes as **root**, and the file is committed with the project, so a
team shares one sandbox definition. If you would rather have a file your editor
and `hadolint` will parse standalone, write `FROM claude-sandbox-base` as the
first line; the launcher recognizes that placeholder and replaces it. Any other
`FROM` is rejected — it would start a fresh build stage and silently discard
the layer beneath, leaving a VM with no `sshd`, no firewall, and no Claude Code.

### Accepting an overlay

An overlay is build instruction that lives in a repository, so opening a
project would otherwise mean running whatever that repository says to run.
Nothing is built until you have seen the contents and accepted them:

```
This project carries an image overlay that has not been accepted:

--- Dockerfile
    RUN apt-get update && apt-get install -y --no-install-recommends golang-go
    …

  /Users/you/Projects/my-app/.claude-sandbox/Dockerfile

  These instructions run as root while building this project's VM image.
  The directory is mounted read-only inside the VM, so a change here is
  expected to have come from the host.

[a] accept and build  [s] skip  [q] quit >
```

What you accept is recorded under
`~/.config/claude-sandbox/overlays/<project>/` — a SHA-256 fingerprint plus a
byte-for-byte snapshot — and every later launch compares against it. Change
so much as a flag and the next launch shows you a diff and asks again, this
time also offering `[r] revert`, which puts the accepted contents back. Bare
Enter always takes the option that changes nothing.

The whole directory is covered, not just the Dockerfile: a script that the
Dockerfile `COPY`s and runs is build instruction too, and gating one while
ignoring the other would be a gate in name only.

Builds run from the accepted snapshot rather than from the project, so the
bytes handed to the builder are exactly the ones you were shown.

Acceptance is keyed to the project's path, so moving a project or cloning it
onto another machine asks again — a fresh clone's overlay is genuinely
unreviewed there.

### Read-only inside the VM

`.claude-sandbox/` is bind-mounted read-only in the guest, so writes to it from
inside the VM fail with `EROFS`. Ordinary agent behavior — editing a file,
running a formatter, an errant `rm` — bounces off it.

**How much of a wall this is depends on the guest.** By default the sandbox
account cannot become root, so `mount -o remount,rw` is not available to it at
all. Under [`--sudo`](#root-inside-the-vm) it is one command away, and what the
mount buys there is only that touching the file takes a deliberate, conspicuous
act rather than an ordinary write. Either way the control that actually holds
is the acceptance prompt above: whatever happens in the VM, nothing gets built
on the host without a human reading a diff first.

One consequence worth knowing: **a `git checkout`, `pull`, `stash`, or `rebase`
inside the VM that would change `.claude-sandbox/` fails.** Git only writes
files that differ, so this is confined to moving between branches whose
overlays differ — which is exactly the case where you would want to re-review
anyway. Do that operation on the host. (Git otherwise behaves normally in
there: the files are visible and readable, so nothing looks deleted.)

### Managing overlays

| Command | What it does |
| --- | --- |
| `claude-sandbox overlay <dir>` | Status of both layers: where the global one is, and whether the project's is accepted, changed (with a diff), or never accepted |
| `claude-sandbox overlay --accept <dir>` | Accept the current contents without launching a VM |
| `claude-sandbox overlay --revert <dir>` | Restore the last accepted contents into the project |
| `claude-sandbox overlay --forget <dir>` | Drop the acceptance record and snapshot |

The actions apply to the *project's* overlay. The global one needs no
management commands — it is a file in your config directory with no gate on it,
so you create, edit, and delete it with your editor.

These are pure host-side file operations — they need neither the `container`
runtime nor a running VM. `claude-sandbox rm` deliberately leaves the record
alone: it is config you authored, not derived state, and deleting a VM should
not discard the Dockerfile you wrote for it. `--forget` is how you drop it, and
it works after the project directory itself is gone.

### Rebuilds

Each overlay layer's image is tagged with a fingerprint of its own contents, of
the image it sits on, of the base image's build stamp, and of the Dockerfile
the launcher generates around the fragment you wrote — so "is this image
current?" is answered by whether that tag exists. Editing a layer and editing
it back costs nothing, and changing one invalidates everything stacked above
it: edit the global overlay and every project's rebuilds; rebuild the base and
both do. If a VM is running on an image that is no longer current, the launcher
says so and recreates it — a couple of seconds, and all durable state is in the
mounts.

Superseded tags are deleted as each layer is rebuilt, so they don't accumulate
one per edit. Only the layers unique to them are freed; everything underneath
is shared.

The base image is the exception, and it needs one. It keeps the same tag across
rebuilds — that is what lets every project find it — so unlike an overlay's, its
tag cannot say which `Dockerfile` produced it. Left at that, editing `Dockerfile`
or `entrypoint.sh`, rebuilding the binary, and never getting round to a
`rebuild` would leave every project booting the old image indefinitely, with
nothing on screen to suggest it. So each base build stamps a digest of those two
files into the image as a `claude-sandbox.source` label, and every launch reads
it back before starting a VM. If it doesn't match the sources compiled into the
binary — or the image predates the label, or was built by hand — the launch is
refused and names the command to fix it:

```
claude-sandbox: claude-sandbox:you was built from a different Dockerfile or entrypoint.sh
  than this build of claude-sandbox carries.
  Refusing to start a VM from it: it is not the image this build of claude-sandbox describes.
  rebuild it:            claude-sandbox rebuild ~/Projects/my-app
  or keep cached layers: claude-sandbox rebuild --use-cache ~/Projects/my-app
```

A refusal rather than an automatic rebuild because a base build is minutes of
apt and npm, and having that start on its own out of `claude-sandbox <dir>` is
worse than being told which command to run. The check happens before the overlay
acceptance prompt, so a launch that is going to be refused doesn't spend the one
prompt here that wants your full attention. `rebuild` itself is never refused —
it is the way out — and the `--use-cache` form is the one to reach for when the
sources moved but you don't want every package refetched.

The authorized keys are deliberately not part of that digest. They are a build
arg rather than a source file and they move whenever a new `~/.ssh/id_*.pub`
appears; an image whose set has fallen behind costs at most a manual `ssh` by a
key that isn't listed yet — never the launcher's own — and a full rebuild is too
much to charge for that.

That machinery answers "have these instructions changed?", which is a different
question from "have their *results* changed" — see [Keeping images up to
date](#keeping-images-up-to-date).

### Keeping images up to date

Every version an image installs is pinned the moment its layer is built.
`RUN npm install -g @anthropic-ai/claude-code` resolves `latest` exactly once;
`apt-get install ripgrep` in your global overlay resolves it once. After that
the builder answers both from its layer cache, and it will keep doing so for as
long as the instructions above them don't change — which, for a Dockerfile
nobody is editing, is forever. Nothing about this is specific to Claude Code;
it is true of every package any layer pulls in.

The `rebuild` mode is the way out. It re-pulls the OS image named in the base
`FROM`, then re-runs every step of every layer with the cache discarded, so
what lands in the image is what upstream is publishing today:

```sh
claude-sandbox rebuild ~/Projects/my-app
```

That takes a few minutes — apt, Node, the CLI, and anything your overlays add,
all fetched again. It is the right hammer when the question is "why am I still
on last month's CLI", and the wrong one for everything else.

It builds images and stops: no VM is created, nothing is seeded, no editor
opens. The one thing it does beyond building is delete the project's VM if one
is running, because the base image keeps its tag across a rebuild — the
image-change check that normally retires a stale VM cannot see it, so a running
VM would go on serving exactly the layers you just replaced. The next launch
recreates it in seconds.

For everything else there is `rebuild --use-cache`, which re-runs the build
but lets the builder answer from cache. Edits to a Dockerfile invalidate their
own layer and everything below it, so the edit lands; nothing else moves. That
is the fast path after changing an overlay.

A failed pull is a warning rather than an error — offline, or a registry having
a bad day, gets you a build on the OS image already on disk rather than no
build at all.

### What an overlay can and cannot do

An overlay runs as root while building the image, so it **can** undo anything
the base image set up — replace the entrypoint and lose the egress firewall,
add an authorized key, write its own sudoers entry and hand the VM back the
root the default withholds, anything. The launcher re-asserts `USER`, `EXPOSE`,
and `ENTRYPOINT` after your fragment so that a stray directive cannot strand a VM
without its firewall by accident, but that is a guard against mistakes, not
against a hostile overlay, and no amount of generated boilerplate could make it
one. That is the whole reason the acceptance prompt exists: an overlay is code
you are choosing to run, and the launcher makes sure it is a choice.

If you would rather not deal with any of this for a particular project, pass
`--no-overlay` and it boots the plain base image, skipping the global overlay
too. The read-only mount still applies.

## Architecture

Three files, one binary:

| File | Role |
| --- | --- |
| `src/main.rs` | The `claude-sandbox` launcher: name derivation, image build, container lifecycle, readiness polling, ssh-config management |
| `Dockerfile` | The guest image: Ubuntu 24.04 + Node 22 + Claude Code CLI + `build-essential` + `sshd`, key-only login, and no route to root unless built with `SUDO=1` |
| `entrypoint.sh` | Runs in the guest at boot: applies the egress firewall, makes `.claude-sandbox/` read-only, starts the idle watchdog, execs `sshd` |

`Dockerfile` and `entrypoint.sh` are embedded into the binary with
`include_str!`, so the release binary is self-contained — **and so editing
either one requires `cargo build` before a rebuild picks up the change.** At
build time they are written to a temp context directory that `container build`
consumes.

### Flow of `claude-sandbox <dir>`

1. **Preflight** — make sure the `container` CLI exists and its services are
   running (starting them if not); read the DNS domain from
   `container system property ls` and warn if the host isn't set up to resolve
   it.
2. **Identity** — canonicalize the directory and derive a container name:
   `claude-sandbox-<sanitized-basename>-<first 6 hex of md5(abs path)>`. The
   hash keeps two projects with the same basename apart; the 63-byte cap comes
   from container names doubling as DNS labels.
3. **Key** — generate a dedicated ed25519 key in `~/.config/claude-sandbox` on
   first use.
4. **Overlays** — if the project has a `.claude-sandbox/` directory, read it and
   compare it against the accepted snapshot, prompting if it is new or changed
   (see [Image overlays](#image-overlays)). Asked before anything is built, so
   "skip" never arrives after a five-minute build. The global overlay is read
   here too, without a prompt.
5. **Image** — build `claude-sandbox:<user>` if absent (`…-sudo` under
   `--sudo`); if it is already there, check its `claude-sandbox.source` label
   against a digest of the embedded `Dockerfile` and `entrypoint.sh` and refuse
   the launch if they disagree (see [Rebuilds](#rebuilds)). That check actually
   runs before the overlay step above, so a launch that is going to be refused
   never asks you to review an overlay first.
   The account name is baked into the image, so it goes in the tag:
   changing `CLAUDE_SANDBOX_USER` builds a second image rather than booting one
   whose only account you can't log in as. So is whether that account can
   become root, for the same reason — reusing one variant for the other would
   silently either break every `apt-get` or hand back the root the default
   withholds. Authorized keys are the dedicated key plus any `~/.ssh/id_*.pub`,
   joined with a literal `\n` escape that the Dockerfile expands with
   `printf '%b'` (a raw newline in a `--build-arg` value crashes Apple's
   builder). The global overlay is then built on top as
   `claude-sandbox:glb-<user>-<fingerprint>`, and an accepted project overlay on
   top of *that* as `claude-sandbox:ovl-<project>-<fingerprint>` — the latter
   from the snapshot rather than from the project directory. Under `rebuild`,
   the reference in the base `FROM` is re-pulled first (parsed out of the
   embedded Dockerfile, so it can't drift), and every one of these builds gets
   `--no-cache` unless `--use-cache` says otherwise.
6. **Container** — reuse only a *running* container, and only if it was created
   from the image this run wants; anything else is torn down and recreated.
   Created with `--rm`, `--cap-add CAP_NET_ADMIN`, and two bind mounts: the
   project → `~/Projects/<name>`, and `~/.claude-sandbox` → `~/.claude`.
7. **Readiness** — poll for up to 60s, re-reading the IP from
   `container inspect` on every attempt (the runtime assigns it shortly after
   the container appears and can briefly report a previous incarnation's
   address), and TCP-connect to port 22. If the container stops during the
   wait, dump its logs. After ~15 consecutive `EHOSTUNREACH` failures, verify
   `sshd` from inside via `container exec` (which travels over vsock and needs
   no local-network access) and print the Local Network permission hint.
8. **SSH config** — write a per-container block into
   `~/.config/claude-sandbox/ssh_config` pinning `HostName` to the address that
   actually answered, plus a shared `claude-sandbox-*` defaults block (identity,
   `IdentitiesOnly`, no host-key checking — the host key is new every boot).
   `~/.ssh/config` gets an `Include` line prepended if it doesn't already have
   one. Other projects' blocks are preserved.
9. **Hand off** — `exec` into `zed ssh://user@host/path`, or into `ssh` for the
   `shell` subcommand.

### Why connect by IP rather than name

The runtime publishes `<container>.container` DNS records, but the record
appears seconds *after* the container does. A lookup in that window returns
NXDOMAIN, which mDNSResponder caches — poisoning every subsequent retry, and
Zed's own connect, for the negative TTL. So the launcher takes the address from
`container inspect`, confirms it by connecting, and pins it as `HostName`; the
DNS name survives only as a stable alias. (As a fallback, after five failed
attempts the launcher *also* tries resolving the name each round, since the two
mechanisms have failed independently — whatever answers first wins.)

A freshly booted VM is also briefly unreachable while the host is still
learning its MAC — connections fail with `EHOSTUNREACH`, the same error macOS
returns when Local Network permission is denied. The launcher tells the two
apart by duration: only a sustained run of failures (~15s) means the route is
barred rather than still settling, and even then it confirms `sshd` over vsock
before giving up.

### The egress firewall

`entrypoint.sh` installs an `inet egress` table with an `accept` policy and
these rules, in order: loopback; established/related (so replies to the host's
inbound SSH work); the ICMPv6 types connectivity actually needs (neighbor and
router discovery, MLD, path-MTU); UDP/TCP port 53 to each resolver in
`/etc/resolv.conf`; then `reject` for `10/8`, `100.64/10`, `169.254/16`,
`172.16/12`, `192.168/16`, `224/4`, `240/4`, `fc00::/7`, `fe80::/10`, and
`ff00::/8`. Everything else — the public Internet — falls through to `accept`.

The DNS exemption matters because the resolver is itself on the local network
(`192.168.64.1`, the host side of the VM bridge) and would otherwise be
rejected. There is no DHCP exemption: the runtime configures the VM's address
directly, so no DHCP client runs.

Resolver addresses are sanitized before they reach `nft`, so a malformed
`/etc/resolv.conf` can neither inject syntax nor abort the fail-closed boot.

The table is enforced by the guest's own kernel, so it holds exactly as long as
nothing in the guest can become root — see [Root inside the
VM](#root-inside-the-vm) for why the account has no `sudo` by default, and what
`--sudo` gives up.

### The idle watchdog

A background loop samples `/proc/net/tcp{,6}` every 5s for established
connections to port 22. Once none have been seen for `IDLE_TIMEOUT` seconds it
`kill -TERM 1`s `sshd`, which stops the container — and, because it was created
with `--rm`, deletes it. A session only counts as "seen" after two consecutive
samples, so the launcher's momentary readiness probe can't collapse the boot
grace period down to the short idle timeout.

## State on the host

| Path | Contents |
| --- | --- |
| `~/.config/claude-sandbox/id_ed25519{,.pub}` | Dedicated SSH key for VM access |
| `~/.config/claude-sandbox/ssh_config` | Managed host blocks, `Include`d from `~/.ssh/config` |
| `~/.config/claude-sandbox/zed-trust-notice` | Marker that the Restricted Mode note has been printed |
| `~/.config/claude-sandbox/global/` | Your global overlay: a `Dockerfile` and anything it `COPY`s, layered onto every project |
| `~/.config/claude-sandbox/overlays/<project>/` | Accepted overlay: `accepted.json` (fingerprint), `accepted/` (snapshot), `build/` (generated context), `built.tag` (and `built-sudo.tag`, one per base variant) |
| `~/.config/claude-sandbox/overlays/_global/` | The same launcher-owned bits for the global overlay (`build/`, `built*.tag`) — no snapshot, since it is not gated |
| `~/.config/claude-sandbox/containers/<name>.image` | Which image a running VM was created from, for the staleness check |
| `~/.config/claude-sandbox/base-<user>.stamp` | Bumped on every base-image rebuild, so overlays rebuild on top of it. `base-<user>-sudo.stamp` is the same for the `--sudo` variant |
| `~/.claude-sandbox/` | Mounted as `~/.claude` in every VM — the persistent Claude Code login and settings |

## Releasing

`Formula/claude-sandbox.rb` is head-only — it names no tarball or checksum, just
the repository — so pushing to `main` *is* the release. Anyone on the tap picks
it up with `brew upgrade --fetch-HEAD claude-sandbox`.

Tags are optional on top of that, and only produce a changelog:

```sh
# bump `version` in Cargo.toml, then
cargo build --release          # refreshes Cargo.lock's own entry
git commit -am "Release 0.2.0"
git tag v0.2.0
git push origin main v0.2.0
```

Pushing a `v*` tag runs [`release.yml`](.github/workflows/release.yml), which
creates a GitHub release with generated notes. It does not touch the formula,
and nobody has to tag for installs to work — the version in `Cargo.toml` is what
`claude-sandbox --version` reports, so bump it when it would otherwise go stale.

## Troubleshooting

- **"timed out waiting for sshd" / "cannot reach …:22"** — almost always the
  macOS Local Network grant for your terminal app. Check with
  `nc -vz <vm-ip> 22` from the same terminal.
- **"stopped before sshd came up"** — the firewall failed to apply and the
  entrypoint bailed. Re-run with `CLAUDE_SANDBOX_DEBUG=1` to keep the container
  around, then `container logs <name>`.
- **`container` subcommands failing with "Operation not permitted"** — the
  `container` CLI does not work under a restrictive command sandbox; run it
  from a normal shell.
- **No language servers in the VM, "Restricted Mode" in Zed's title bar** —
  expected on a host Zed hasn't been told to trust; see
  [step 5](#5-lift-zeds-restricted-mode).
- **Dockerfile edits appear to do nothing** — for the *base* image
  (`Dockerfile` in this repo), rebuild the binary (`cargo build --release`);
  the image sources are compiled in. Then `rebuild --use-cache`, which is the
  seconds-long form. You shouldn't get this far silently: once the binary is
  rebuilt, launches refuse the stale image outright rather than booting it (see
  [Rebuilds](#rebuilds)). For a *project's* `.claude-sandbox/Dockerfile`, the
  launcher picks changes up on the next launch and asks you to accept them —
  check `claude-sandbox overlay <dir>` if it didn't.
- **"Refusing to start a VM from it" on a base image you never touched** — most
  likely the image predates the `claude-sandbox.source` label, which is the one
  case the check can't tell apart from a genuinely stale image. `rebuild
  --use-cache <dir>` restamps it in seconds without refetching anything.
- **A package in the image is out of date, and rebuilding doesn't move it** —
  you are getting the builder's cached layer. That is what plain `rebuild`
  (no `--use-cache`) exists for; see [Keeping images up to
  date](#keeping-images-up-to-date).
- **`git checkout` in the VM fails with "Read-only file system"** — the branch
  you are switching to has a different `.claude-sandbox/`, which is mounted
  read-only in the guest. Do that checkout on the host; see
  [Read-only inside the VM](#read-only-inside-the-vm).
- **"the overlay must be accepted before it is built"** — a non-interactive run
  hit an unaccepted overlay. Accept it deliberately with
  `claude-sandbox overlay --accept <dir>`, or pass `--no-overlay`.
- **`sudo: <user> is not in the sudoers file`, or `apt-get`/`npm install -g`
  failing with `Permission denied`** — working as intended: the VM's account
  cannot become root. Put the package in an [image
  overlay](#image-overlays), or launch with
  [`--sudo`](#root-inside-the-vm) if you want root in the VM itself and accept
  what that costs.
