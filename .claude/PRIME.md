# Prime handoff — maintained by /document
Updated: 2026-08-30 on branch main

## In flight
- Nothing pending. `main` = `3854ed1` (--dns for VPN-broken guest DNS),
  pushed; `target/release` is built from it.
- One verification gap in `3854ed1`: `check_guest_dns` (the post-launch
  in-guest DNS probe) has never run against a live VM — the `container` CLI
  does not work under a sandboxed shell. Needs one healthy launch (probe
  should stay silent) and ideally one behind Mullvad (should print the
  `--dns` hint). Flag/env validation and per-mode rejection were tested.
- `3854ed1` touched entrypoint.sh (comment only), which moves the source
  digest: binaries from current main refuse older base images until
  `claude-sandbox rebuild --use-cache <dir>` restamps them (seconds).
- The brew keg on this machine (`HEAD-eed2a76`) is several commits behind —
  `brew upgrade --fetch-HEAD claude-sandbox` when it matters.
- Housekeeping: `vm-limits-and-clap-cli` and `image-overlay` are both fully
  merged into main (verified 2026-08-30); their local and origin refs are
  safe to delete.

## Dead ends
- Hunted for the exact Homebrew line that repoints HOME during `install`: not
  in `bin/brew`, superenv (`extend/ENV/super.rb`), `build.rb`,
  `utils/fork.rb`, or `sandbox.rb`. The repointing is real (a reproduced
  failure attests), so don't re-hunt; nothing depends on finding the line.

## Notes for the next session
- The Mullvad/VPN diagnosis behind `--dns` rests on symptoms (guest → bridge
  resolver:53 dead while direct public DNS through the tunnel works, per a
  user report), not on Mullvad's actual firewall rules — README
  "Troubleshooting" and the `check_guest_dns` comment word the mechanism
  loosely on purpose; don't firm it up without testing on a real Mullvad host.
- `container inspect` is deliberately not asked what DNS a VM was created
  with — its JSON shape for that field is unverified; the launcher keeps its
  own note instead (see `dns_record`).
- The Homebrew/rustup fast-path rationale lives in the formula's comments
  (`RUSTUP_ROOT` constant and `install`). One edge not spelled out there:
  with HOMEBREW_CACHE relocated outside `$HOME`, the build sandbox denies the
  whole home and the fast path dies loudly on the unreadable rustup shim —
  accepted edge case.
