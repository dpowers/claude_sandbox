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
- **the Internet still works** — public destinations are allowed, plus DNS to
  the configured resolvers (which usually *are* on the LAN), so `npm install`,
  `git push`, and the Claude API all work normally;
- **the blast radius is one directory** — the only host path mounted is the
  project you named;
- **it fails closed** — if the firewall rules can't be applied, the entrypoint
  exits and `sshd` never starts.

Your Claude Code login is shared across VMs (via `~/.claude-sandbox`), so you
authenticate once rather than per project.

## Requirements

- **macOS on Apple silicon** with Apple's [`container`](https://github.com/apple/container)
  CLI (developed against v1.2.2): `brew install container`. Each container gets
  its own VM, which is what makes the in-guest firewall trustworthy and lets the
  guest hold `CAP_NET_ADMIN` without weakening the host.
- **Rust** (stable, edition 2021) to build the launcher.
- **[Zed](https://zed.dev)** on the host, with the `zed` CLI on `PATH` — only
  for the default `up` command; `claude-sandbox shell` needs just `ssh`.
- **Local DNS for container names** (optional but recommended):
  `sudo container system dns create container`. Connections are made by pinned
  IP, so a missing resolver only produces a warning.
- **macOS Local Network permission** for the terminal app you launch from:
  System Settings → Privacy & Security → Local Network. VMs live on
  `192.168.64.0/24`; without the grant, connections fail as "No route to host"
  rather than as a permission error. `claude-sandbox` detects this case and
  says so. Zed.app carries its own grant, so the VM may be reachable from Zed
  even when your terminal can't reach it.

## Getting started

```sh
cargo build --release
ln -s "$PWD/target/release/claude-sandbox" /usr/local/bin/claude-sandbox

claude-sandbox ~/Projects/my-app     # first run builds the image (a few minutes)
```

The VM gets an account named after whoever runs the launcher — your host
username, lowercased and sanitized for Linux — so the project lands at
`~/Projects/<name>` inside the VM just as it does on the host. Run `claude`
there. The first run will prompt you to log in to Claude Code; that login
persists to `~/.claude-sandbox` on the host and is reused by every later VM.

### Commands

| Command | What it does |
| --- | --- |
| `claude-sandbox <dir>` | Start the VM (building/creating as needed) and open Zed |
| `claude-sandbox shell <dir> [cmd…]` | Same, but `ssh` in instead of opening Zed |
| `claude-sandbox stop <dir>` | Stop the project's VM (it deletes itself on stop) |
| `claude-sandbox rm <dir>` | Stop and delete the VM, and drop its ssh-config block |
| `claude-sandbox --rebuild <dir>` | Rebuild the image, recreate the VM, open Zed |

### Environment

| Variable | Effect |
| --- | --- |
| `CLAUDE_SANDBOX_IDLE` | Idle timeout in seconds, applied when the VM is created (`0` = never reap) |
| `CLAUDE_SANDBOX_DEBUG` | Keep failed VMs around (skips `--rm`) so `container logs <name>` works |
| `CLAUDE_SANDBOX_USER` | Account name inside the VM (default: your host username) |
| `CLAUDE_SANDBOX_STATE` | Override the state directory (default `~/.config/claude-sandbox`) |

### Lifecycle

A VM deletes itself about **15 seconds** after its last SSH session ends (Zed
window closed, shell exited). A freshly booted VM gets **2 minutes** to receive
its first connection. Reopening a project recreates it in a couple of seconds —
these VMs are meant to be disposable, and all durable state lives in the two
mounts.

## Architecture

Three files, one binary:

| File | Role |
| --- | --- |
| `src/main.rs` | The `claude-sandbox` launcher: name derivation, image build, container lifecycle, readiness polling, ssh-config management |
| `Dockerfile` | The guest image: Ubuntu 24.04 + Node 22 + Claude Code CLI + `sshd`, key-only login, passwordless sudo |
| `entrypoint.sh` | Runs in the guest at boot: applies the egress firewall, starts the idle watchdog, execs `sshd` |

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
4. **Image** — build `claude-sandbox:<user>` if absent. The account name is
   baked into the image, so it goes in the tag: changing `CLAUDE_SANDBOX_USER`
   builds a second image rather than booting one whose only account you can't
   log in as. Authorized keys are the dedicated key plus any `~/.ssh/id_*.pub`,
   joined with a literal `\n` escape that the Dockerfile expands with
   `printf '%b'` (a raw newline in a `--build-arg` value crashes Apple's
   builder).
5. **Container** — reuse only a *running* container; anything else is torn down
   and recreated. Created with `--rm`, `--cap-add CAP_NET_ADMIN`, and two bind
   mounts: the project → `~/Projects/<name>`, and `~/.claude-sandbox` →
   `~/.claude`.
6. **Readiness** — poll for up to 60s, re-reading the IP from
   `container inspect` on every attempt (the runtime assigns it shortly after
   the container appears and can briefly report a previous incarnation's
   address), and TCP-connect to port 22. If the container stops during the
   wait, dump its logs. After ~15 consecutive `EHOSTUNREACH` failures, verify
   `sshd` from inside via `container exec` (which travels over vsock and needs
   no local-network access) and print the Local Network permission hint.
7. **SSH config** — write a per-container block into
   `~/.config/claude-sandbox/ssh_config` pinning `HostName` to the address that
   actually answered, plus a shared `claude-sandbox-*` defaults block (identity,
   `IdentitiesOnly`, no host-key checking — the host key is new every boot).
   `~/.ssh/config` gets an `Include` line prepended if it doesn't already have
   one. Other projects' blocks are preserved.
8. **Hand off** — `exec` into `zed ssh://user@host/path`, or into `ssh` for the
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
| `~/.claude-sandbox/` | Mounted as `~/.claude` in every VM — the persistent Claude Code login and settings |

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
- **Dockerfile edits appear to do nothing** — rebuild the binary
  (`cargo build --release`); the image sources are compiled in.
