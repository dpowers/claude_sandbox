# Prime handoff — maintained by /document
Updated: 2026-08-30 on branch main

## In flight
- Nothing. `vm-limits-and-clap-cli` was rebased onto main and fast-forwarded
  in (history kept linear); `main` = `eaca60c`, pushed. The branch ref still
  exists locally and on origin, fully merged — safe to delete.
- The merge changed the embedded `entrypoint.sh` (hardening commit), so a
  binary built from current main refuses pre-merge base images as stale;
  `claude-sandbox rebuild --use-cache <dir>` restamps them in seconds.
  `target/release` was rebuilt from merged main; the brew keg
  (`HEAD-eed2a76`) predates the merge — `brew upgrade --fetch-HEAD
  claude-sandbox` refreshes it.

## Dead ends
- Hunted for the exact Homebrew line that repoints HOME during `install`: not
  in `bin/brew`, superenv (`extend/ENV/super.rb`), `build.rb`,
  `utils/fork.rb`, or `sandbox.rb`. The repointing is real (a reproduced
  failure attests), so don't re-hunt; nothing depends on finding the line.

## Notes for the next session
- The formula's rustup fast path was broken until `eed2a76` (2026-08-30):
  `brew install --HEAD` on a rustup machine died with "rustup could not
  choose a version of cargo to run". Fixed by exporting RUSTUP_HOME; verified
  end to end (install + `brew test`). Rationale is in the formula's comments
  (`RUSTUP_ROOT` constant and `install`).
- Homebrew 6 env facts behind those comments (verified against 6.0.18
  source): `bin/brew` re-execs `env -i` with a fixed passthrough (HOME, PATH,
  USER, proxies, …) plus all `HOMEBREW_*`; a user's CARGO_HOME/RUSTUP_HOME
  are renamed to `HOMEBREW_CARGO_HOME`/`HOMEBREW_RUSTUP_HOME`, consumed only
  by `brew bundle` and never restored for formula builds — so
  `RUSTUP_HOME=… brew install` can never work and the formula's `ENV.fetch`
  defaults always win.
- Build-sandbox home policy (`Library/Homebrew/sandbox.rb`,
  `deny_read_home`): when `HOMEBREW_CACHE` is inside `$HOME` (the default,
  `~/Library/Caches/Homebrew`), only a credential denylist is denied
  (`~/.ssh`, `~/.gnupg`, `.cargo/credentials*`, `.claude`, …) and the rest of
  home — `~/.rustup`, `~/.cargo/bin` — stays readable, which is why exporting
  RUSTUP_HOME works. With the cache relocated outside `$HOME`, the whole home
  is denied and the rustup fast path dies earlier (the shim itself is
  unreadable); accepted edge case, loud failure.
