// claude-sandbox — run a project directory inside a firewalled VM (Apple
// `container`) and open Zed on the host connected to it over SSH.
//
// The Dockerfile and entrypoint.sh are embedded at compile time, so the
// release binary is fully self-contained; changing either file requires a
// `cargo build` for image rebuilds to pick it up.

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::env;
use std::fs;
use std::io;
use std::io::{IsTerminal, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, SystemTime};

const IMAGE_REPO: &str = "claude-sandbox";
const DOCKERFILE: &str = include_str!("../Dockerfile");
const ENTRYPOINT: &str = include_str!("../entrypoint.sh");

/// Per-project image overlay: a directory inside the project holding a
/// Dockerfile (plus anything it COPYs) that is layered on top of the base
/// image. It is the build context, it is mounted read-only inside the VM, and
/// it is never built from until the host has accepted its current contents.
const OVERLAY_DIR: &str = ".claude-sandbox";

/// The same idea one level up: a directory in the state directory whose
/// Dockerfile is layered onto every project, under the per-project overlay.
/// It needs no acceptance step — it is not in any repository and is not
/// reachable from any sandbox, so it is the host's own by construction.
const GLOBAL_DIR: &str = "global";

/// Launcher-owned record for the global overlay, kept alongside the
/// per-project ones. `_` cannot start a project id (basenames are sanitized to
/// `[a-z0-9-]`), so it can never collide with one.
const GLOBAL_RECORD: &str = "_global";

/// Ceiling on the overlay build context. Everything under it is read into
/// memory to fingerprint it, and shipped to the builder on every rebuild, so a
/// directory this large is a mistake worth naming rather than tolerating.
const OVERLAY_MAX_BYTES: u64 = 64 << 20;

/// Per-file cap when printing an unaccepted overlay for review. Long enough
/// for any plausible Dockerfile; short enough that a padded one cannot scroll
/// the interesting lines off the screen.
const REVIEW_MAX_LINES: usize = 200;

// The runtime's own defaults (1 GiB / 4 cpus) are sized for a service
// container, not a dev box: Claude Code is a Node process before rustc or a
// language server starts, and a single link step can outgrow the rest of that
// gigabyte. These are per-VM ceilings and there is one VM per project, so they
// are chosen to leave the host usable with several open at once rather than to
// match the machine — vCPUs are time-sliced against the host anyway, and past
// the performance-core count the guest just schedules onto efficiency cores it
// cannot tell apart. The runtime adds one vCPU of overhead on top, so the
// guest kernel reports CPUS + 1.
const DEFAULT_MEMORY: &str = "8g";
const DEFAULT_CPUS: &str = "6";

const USAGE: &str = "\
claude-sandbox — run a project directory inside a claude-sandbox VM and open
Zed on the host connected to it over SSH.

usage:
  claude-sandbox [opts] <dir>                 start the VM (build/create as needed), open Zed
  claude-sandbox shell [opts] <dir> [cmd...]  same, but ssh in instead of opening Zed
  claude-sandbox stop <dir>                   stop the project's VM (it deletes itself on stop)
  claude-sandbox rm <dir>                     stop and delete the project's VM
  claude-sandbox overlay [action] <dir>       inspect or manage the project's image overlay
  claude-sandbox --rebuild <dir>              rebuild the image, recreate the VM, open Zed

Options (memory/cpus are read when the VM is created; a running VM keeps the
limits it was created with, so change them with `rm` first):
  -m, --memory <size>  memory ceiling, K/M/G/T/P suffix required (default: 8g)
  -c, --cpus <n>       vCPUs; the guest sees one more than this (default: 6)
      --rebuild        rebuild the image and recreate the VM
      --no-overlay     ignore both overlays for this run (boot the plain base image)
      --accept-overlay accept the project overlay's contents without prompting

Overlay actions (for `claude-sandbox overlay`):
  (none)      show status, and a diff if the overlay is unaccepted
  --accept    accept the current contents
  --revert    restore the last accepted contents into the project
  --forget    drop the acceptance record and the stored snapshot

Image overlays (base -> global -> project):
  ~/.config/claude-sandbox/global/Dockerfile, if it exists, is layered onto
  every project's image. It is yours and no sandbox can reach it, so it is
  built as-is; edit it and the next launch rebuilds.

  <dir>/.claude-sandbox/Dockerfile is then layered on top of that, with that
  directory as the build context. It comes from the project's repository and
  runs as root at build time, so nothing is built until you have seen the
  contents and accepted them; the accepted bytes are recorded under
  ~/.config/claude-sandbox and any later change re-prompts with a diff. The
  directory is mounted read-only inside the VM, so writes to it from in there
  fail.

State:
  ~/.config/claude-sandbox/   dedicated ssh key, managed ssh config, overlay records
  ~/.config/claude-sandbox/global/  Dockerfile layered onto every project
  ~/.claude-sandbox/          mounted as ~/.claude in every VM (Claude login)

A VM deletes itself ~15 seconds after its last ssh session ends (Zed window
closed, shell exited); reopening the project recreates it in seconds. A fresh
VM gets 2 minutes to receive its first connection.

Environment:
  CLAUDE_SANDBOX_IDLE=<seconds>  idle timeout, applied when the VM is created (0 = never reap)
  CLAUDE_SANDBOX_DEBUG=1         keep failed VMs around (skips --rm) so `container logs` works
  CLAUDE_SANDBOX_USER=<name>     account inside the VM (default: your host username)
  CLAUDE_SANDBOX_RESEED=1        overwrite the VM's Claude credentials from the host keychain
  CLAUDE_SANDBOX_ACCEPT_OVERLAY=1  accept overlay contents without prompting (for scripts)

Claude Code inside the VM is seeded from the host on every run: OAuth tokens
exported from the login Keychain, plus settings.json, CLAUDE.md, agents,
commands, skills, output-styles and plugins. This is a one-way copy, never a
mount of ~/.claude — transcripts stay on the host, and nothing the VM writes
can reach config the host executes.";

#[derive(PartialEq, Clone, Copy)]
enum Cmd {
    Up,
    Shell,
    Stop,
    Rm,
    Overlay,
}

/// What `claude-sandbox overlay` was asked to do.
#[derive(PartialEq, Clone, Copy)]
enum OverlayAction {
    Status,
    Accept,
    Revert,
    Forget,
}

/// Flags accepted before the directory (and, for `up`, after it too). Values
/// stay `None` when unset so `stop`/`rm` can reject flags that only mean
/// something at creation time.
#[derive(Default)]
struct Opts {
    rebuild: bool,
    memory: Option<String>,
    cpus: Option<String>,
    no_overlay: bool,
    accept_overlay: bool,
    overlay_action: Option<OverlayAction>,
}

impl Opts {
    /// Consume leading flags, stopping at the first argument that is not one —
    /// the directory, or (for `shell`) the start of the ssh command.
    fn take_from(&mut self, args: &mut VecDeque<String>) -> Result<()> {
        while let Some(head) = args.front().cloned() {
            let (name, inline) = match head.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (head.as_str(), None),
            };
            match name {
                "--rebuild" => {
                    if inline.is_some() {
                        bail!("--rebuild takes no value");
                    }
                    args.pop_front();
                    self.rebuild = true;
                }
                "--memory" | "-m" => {
                    let v = take_value(args, name, inline)?;
                    self.memory = Some(parse_memory(&v)?);
                }
                "--cpus" | "-c" => {
                    let v = take_value(args, name, inline)?;
                    self.cpus = Some(parse_cpus(&v)?);
                }
                "--no-overlay" | "--accept-overlay" => {
                    if inline.is_some() {
                        bail!("{name} takes no value");
                    }
                    args.pop_front();
                    if name == "--no-overlay" {
                        self.no_overlay = true;
                    } else {
                        self.accept_overlay = true;
                    }
                }
                "--accept" | "--revert" | "--forget" => {
                    if inline.is_some() {
                        bail!("{name} takes no value");
                    }
                    args.pop_front();
                    if self.overlay_action.is_some() {
                        bail!("pick one overlay action: --accept, --revert or --forget");
                    }
                    self.overlay_action = Some(match name {
                        "--accept" => OverlayAction::Accept,
                        "--revert" => OverlayAction::Revert,
                        _ => OverlayAction::Forget,
                    });
                }
                _ => break,
            }
        }
        Ok(())
    }
}

/// Pull a flag's value, accepting both `--flag value` and `--flag=value`.
fn take_value(args: &mut VecDeque<String>, flag: &str, inline: Option<String>) -> Result<String> {
    args.pop_front();
    match inline {
        Some(v) => Ok(v),
        None => args.pop_front().ok_or_else(|| anyhow!("{flag} needs a value")),
    }
}

/// Validate a memory ceiling and normalise it for `container -m`. The suffix
/// is required: the runtime reads a bare number as mebibytes, which turns a
/// plausible `--memory 8` into 8 MiB rather than the 8 GiB it looks like.
fn parse_memory(v: &str) -> Result<String> {
    let s = v.trim().to_lowercase();
    let digits = s.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let n: u64 = digits
        .parse()
        .map_err(|_| anyhow!("--memory: expected a size like 8g or 8192m, got: {v}"))?;
    if n == 0 {
        bail!("--memory must be greater than 0");
    }
    match &s[digits.len()..] {
        "" => bail!("--memory: {v} needs a unit suffix (K, M, G, T or P), e.g. {v}g"),
        "k" | "m" | "g" | "t" | "p" | "kb" | "mb" | "gb" | "tb" | "pb" | "kib" | "mib" | "gib"
        | "tib" | "pib" => Ok(s),
        other => bail!("--memory: unknown suffix {other:?} in {v} (use K, M, G, T or P)"),
    }
}

fn parse_cpus(v: &str) -> Result<String> {
    let n: u32 =
        v.trim().parse().map_err(|_| anyhow!("--cpus: expected a whole number, got: {v}"))?;
    if n == 0 {
        bail!("--cpus must be at least 1");
    }
    Ok(n.to_string())
}

/// Byte count for a string `parse_memory` accepted, for comparing a request
/// against what a running VM was created with. Binary units, matching the
/// runtime: it records `-m 2048mb` as exactly 2 GiB.
fn memory_bytes(s: &str) -> Option<u64> {
    let digits = s.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let n: u64 = digits.parse().ok()?;
    let mult: u64 = match s[digits.len()..].chars().next() {
        Some('k') => 1 << 10,
        Some('m') => 1 << 20,
        Some('g') => 1 << 30,
        Some('t') => 1 << 40,
        Some('p') => 1 << 50,
        _ => return None,
    };
    n.checked_mul(mult)
}

fn human_bytes(b: u64) -> String {
    if b.is_multiple_of(1 << 30) {
        format!("{}g", b >> 30)
    } else {
        format!("{}m", b >> 20)
    }
}

struct Sandbox {
    user: String,
    /// The shared image every project starts from, one per guest account.
    base_image: String,
    home: PathBuf,
    state_dir: PathBuf,
    claude_dir: PathBuf,
    key: PathBuf,
    ssh_conf: PathBuf,
    abs: PathBuf,
    /// `<sanitized basename>-<6 hex of the path hash>`: the project's identity
    /// in the container name, the overlay image tag, and the state directory.
    proj_id: String,
    name: String,
    target: String,
    /// `<project>/.claude-sandbox` on the host — where the overlay is authored.
    overlay_src: PathBuf,
    /// The global overlay's build context, shared by every project.
    global_src: PathBuf,
    /// Host-side record for that overlay: the accepted snapshot, its
    /// fingerprint, and the context the image is actually built from.
    overlay_state: PathBuf,
    memory: String,
    cpus: String,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("claude-sandbox: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args: VecDeque<String> = env::args().skip(1).collect();

    let cmd = match args.front().map(String::as_str) {
        None => {
            eprintln!("{USAGE}");
            std::process::exit(1);
        }
        Some("-h" | "--help") => {
            println!("{USAGE}");
            return Ok(());
        }
        Some("shell") => {
            args.pop_front();
            Cmd::Shell
        }
        Some("stop") => {
            args.pop_front();
            Cmd::Stop
        }
        Some("rm") => {
            args.pop_front();
            Cmd::Rm
        }
        Some("overlay") => {
            args.pop_front();
            Cmd::Overlay
        }
        Some(_) => Cmd::Up,
    };

    let mut opts = Opts::default();
    opts.take_from(&mut args)?;

    let Some(dir) = args.pop_front() else {
        eprintln!("{USAGE}");
        std::process::exit(1);
    };
    // take_from stops at the first non-flag, so an unrecognised flag lands
    // here and would otherwise be reported as the directory.
    if dir.starts_with('-') {
        bail!("unknown option: {dir}");
    }

    // Only `shell` forwards trailing arguments (to ssh). Elsewhere they are
    // mistakes — except flags, which are also accepted after the dir.
    let ssh_args: Vec<String> = if cmd == Cmd::Shell {
        args.into()
    } else {
        opts.take_from(&mut args)?;
        if let Some(a) = args.pop_front() {
            bail!("unexpected argument: {a}");
        }
        Vec::new()
    };

    // These only take effect while creating something, and neither a VM that
    // is about to be deleted nor a pure host-side overlay query is that.
    // Silently ignoring them would look like they had resized an existing VM.
    if matches!(cmd, Cmd::Stop | Cmd::Rm | Cmd::Overlay) {
        let what = if cmd == Cmd::Overlay { "overlay" } else { "stop/rm" };
        if opts.rebuild {
            bail!("--rebuild has no effect on {what}");
        }
        if opts.memory.is_some() || opts.cpus.is_some() {
            bail!("--memory/--cpus have no effect on {what}");
        }
        if opts.no_overlay || opts.accept_overlay {
            bail!("--no-overlay/--accept-overlay have no effect on {what}");
        }
    }
    // The overlay actions name what `overlay` should do; anywhere else they
    // would read as a request the launcher silently ignored.
    if cmd != Cmd::Overlay && opts.overlay_action.is_some() {
        bail!("--accept/--revert/--forget are actions for `claude-sandbox overlay <dir>`");
    }

    let sb = Sandbox::new(&dir, cmd, &opts)?;
    match cmd {
        Cmd::Stop => sb.stop(),
        Cmd::Rm => sb.remove(),
        Cmd::Overlay => sb.overlay_cmd(opts.overlay_action.unwrap_or(OverlayAction::Status)),
        Cmd::Up | Cmd::Shell => sb.up(cmd, &opts, &ssh_args),
    }
}

/// Milliseconds-since-epoch expiry from a Claude credentials blob, 0 if it
/// cannot be read (which makes any real token look newer).
fn token_expiry(json: &str) -> u64 {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return 0;
    };
    let node = if v.get("claudeAiOauth").is_some() { &v["claudeAiOauth"] } else { &v };
    node["expiresAt"].as_u64().unwrap_or(0)
}

/// Recursive copy, overwriting the destination; files only, no deletions, so
/// state the VM created alongside the seeded config survives.
fn copy_tree(src: &Path, dest: &Path) -> Result<()> {
    if src.is_dir() {
        fs::create_dir_all(dest)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dest.join(entry.file_name()))?;
        }
    } else if src.is_file() {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        // fs::copy carries the source's mode across, and git stores pack files
        // and loose objects read-only — so a plugin marketplace (a clone) is
        // seeded fine the first time and then cannot be overwritten on any run
        // after it. Unlinking is enough: the mode denies writing the file, not
        // removing it from a directory that is still writable. Only done after
        // a copy actually fails, so an unwritable destination is never dropped
        // on the floor for some unrelated reason.
        let ctx = || format!("copying {} to {}", src.display(), dest.display());
        if let Err(e) = fs::copy(src, dest) {
            if e.kind() != io::ErrorKind::PermissionDenied {
                return Err(e).with_context(ctx);
            }
            let _ = fs::remove_file(dest);
            fs::copy(src, dest).with_context(ctx)?;
        }
    }
    Ok(())
}

/// What to do about an overlay the host has not accepted.
enum Decision {
    Accept,
    Revert,
    Skip,
    Quit,
}

/// One file in an overlay build context: its path relative to the context
/// root, whether it is executable, and its bytes — or, for a symlink, the
/// target it names.
#[derive(PartialEq, Eq)]
struct CtxEntry {
    path: String,
    exec: bool,
    link: bool,
    data: Vec<u8>,
}

/// Copy a build context, reproducing symlinks rather than following them.
///
/// `copy_tree` resolves them, which is right for seeding config but wrong
/// here: a snapshot taken that way would not compare equal to what
/// `scan_context` read, and every accept would fail its own verification. The
/// destination is expected not to exist (callers clear it first).
fn copy_context(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        let ty = entry.file_type()?;
        let ctx = || format!("copying {} to {}", from.display(), to.display());
        if ty.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&from)?, &to).with_context(ctx)?;
        } else if ty.is_dir() {
            copy_context(&from, &to)?;
        } else if ty.is_file() {
            fs::copy(&from, &to).with_context(ctx)?;
        }
    }
    Ok(())
}

/// Read an overlay build context in full, in a fixed order.
///
/// Everything under the directory counts, not just the Dockerfile: a script
/// the Dockerfile `COPY`s and then runs is build instruction too, and gating
/// on one file while ignoring the other would be a gate in name only.
fn scan_context(root: &Path) -> Result<Vec<CtxEntry>> {
    let mut out = Vec::new();
    let mut total: u64 = 0;
    scan_into(root, root, &mut out, &mut total)
        .with_context(|| format!("reading {}", root.display()))?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn scan_into(root: &Path, dir: &Path, out: &mut Vec<CtxEntry>, total: &mut u64) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().into_owned();
        let ty = entry.file_type()?;
        if ty.is_symlink() {
            // Recorded as the link, never followed: a link pointing out of the
            // context would otherwise make the fingerprint cover bytes the
            // builder never sees, and change without the context changing.
            let target = fs::read_link(&path)?;
            out.push(CtxEntry {
                path: rel,
                exec: false,
                link: true,
                data: target.to_string_lossy().into_owned().into_bytes(),
            });
        } else if ty.is_dir() {
            scan_into(root, &path, out, total)?;
        } else if ty.is_file() {
            let meta = entry.metadata()?;
            *total += meta.len();
            if *total > OVERLAY_MAX_BYTES {
                bail!(
                    "{} holds more than {} MiB. It is read in full to fingerprint it and \
                     shipped to the builder on every rebuild, so it is meant to hold a \
                     Dockerfile and the few files it COPYs — not the project.",
                    root.display(),
                    OVERLAY_MAX_BYTES >> 20
                );
            }
            out.push(CtxEntry {
                path: rel,
                exec: meta.permissions().mode() & 0o111 != 0,
                link: false,
                data: fs::read(&path)?,
            });
        }
    }
    Ok(())
}

/// Fingerprint of a build context: every entry in a fixed order, length-framed
/// so no two listings can collide by running together.
///
/// SHA-256 rather than the md5 used to derive container names — this one is
/// recorded as "the contents you accepted", which is exactly where a crafted
/// collision would be aimed. (Acceptance itself compares the bytes, so the
/// hash is what gets recorded and what names the image, not the gate.)
fn fingerprint(entries: &[CtxEntry]) -> String {
    let mut h = Sha256::new();
    h.update(b"claude-sandbox overlay v1\0");
    for e in entries {
        h.update((e.path.len() as u64).to_le_bytes());
        h.update(e.path.as_bytes());
        h.update([e.exec as u8, e.link as u8]);
        h.update((e.data.len() as u64).to_le_bytes());
        h.update(&e.data);
    }
    format!("{:x}", h.finalize())
}

/// Check the project's fragment and return the body to splice into the
/// generated Dockerfile.
///
/// The launcher supplies the `FROM`, so the fragment must not: a second one
/// starts a fresh build stage, which would silently discard the base image and
/// produce a VM with no sshd, no firewall and no Claude Code. The one accepted
/// form is the `claude-sandbox-base` placeholder, which is dropped here — it
/// exists so people who want a file their editor and linter parse standalone
/// can have one.
fn check_fragment(text: &str) -> Result<String> {
    let mut out = String::new();
    let mut continued = false;
    for (n, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        let comment = trimmed.starts_with('#');
        let directive = !continued && !trimmed.is_empty() && !comment;
        continued = !comment && trimmed.ends_with('\\');
        if directive {
            let mut tokens = trimmed.split_whitespace();
            if tokens.next().is_some_and(|k| k.eq_ignore_ascii_case("FROM")) {
                if tokens.next() == Some("claude-sandbox-base") && tokens.next().is_none() {
                    continue;
                }
                bail!(
                    "{OVERLAY_DIR}/Dockerfile line {}: {trimmed}\n  \
                     the overlay is layered onto the base image for you, so it must not \
                     carry its own FROM.\n  \
                     drop the line, or write `FROM claude-sandbox-base` if you want a \
                     file that parses standalone.",
                    n + 1
                );
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

/// Unified diff of the accepted snapshot against the project's copy. `diff`
/// exits non-zero precisely when they differ, which is the whole point, so its
/// status is ignored; if it cannot be run at all, show the current contents
/// instead — an unreadable prompt is worse than a verbose one.
fn show_diff(accepted: &Path, current: &Path) {
    if Command::new("diff").arg("-ruN").arg(accepted).arg(current).status().is_err() {
        eprintln!("(no `diff` available — showing the current contents in full instead)");
        if let Ok(entries) = scan_context(current) {
            show_context(&entries);
        }
    }
}

/// Print a build context for review.
fn show_context(entries: &[CtxEntry]) {
    for e in entries {
        if e.link {
            println!("--- {} -> {} (symlink)", e.path, String::from_utf8_lossy(&e.data));
            continue;
        }
        let Ok(text) = std::str::from_utf8(&e.data) else {
            println!("--- {} ({} bytes, not text)", e.path, e.data.len());
            continue;
        };
        println!("--- {}{}", e.path, if e.exec { " (executable)" } else { "" });
        let mut lines = text.lines();
        for line in lines.by_ref().take(REVIEW_MAX_LINES) {
            println!("    {line}");
        }
        let rest = lines.count();
        if rest > 0 {
            println!("    … {rest} more lines");
        }
    }
}

/// Absolute path for a directory that may no longer exist: canonicalize the
/// longest still-existing ancestor (resolving symlinks — e.g. macOS
/// /tmp -> /private/tmp — exactly the way creation-time canonicalization
/// did) and re-append the missing tail components, so the derived container
/// name matches the one used at creation.
fn absolutize_missing(dir: &str) -> Result<PathBuf> {
    let p = std::path::absolute(dir).with_context(|| format!("cannot absolutize {dir}"))?;
    let mut existing = p.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => tail.push(name.to_owned()),
            None => break,
        }
        existing.pop();
    }
    let mut out = if existing.as_os_str().is_empty() {
        existing
    } else {
        fs::canonicalize(&existing)?
    };
    for name in tail.iter().rev() {
        out.push(name);
    }
    Ok(out)
}

impl Sandbox {
    fn new(dir: &str, cmd: Cmd, opts: &Opts) -> Result<Self> {
        let home = PathBuf::from(env::var("HOME").context("HOME is not set")?);
        let user = guest_user()?;
        let state_dir = env::var("CLAUDE_SANDBOX_STATE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".config/claude-sandbox"));

        let abs = if Path::new(dir).is_dir() {
            fs::canonicalize(dir)?
        } else if matches!(cmd, Cmd::Stop | Cmd::Rm | Cmd::Overlay) {
            // stop/rm must work after the directory is gone, and so must
            // `overlay --forget`, which exists to clean up after exactly that.
            absolutize_missing(dir)?
        } else {
            bail!("not a directory: {dir}");
        };
        if abs.to_string_lossy().contains(':') {
            bail!(
                "project path contains ':', which the container -v mount syntax cannot express: {}",
                abs.display()
            );
        }

        let base: String = abs
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        // Container names double as DNS labels, which cap at 63 bytes:
        // "claude-sandbox-" + base + "-" + 6 hash chars must fit.
        let mut base = base.trim_matches('-').to_string();
        base.truncate(40);
        let base = base.trim_end_matches('-').to_string();
        if base.is_empty() {
            bail!("cannot derive a name from: {}", abs.display());
        }
        let hash = format!("{:x}", md5::compute(abs.to_string_lossy().as_bytes()));
        let proj_id = format!("{base}-{}", &hash[..6]);
        let overlay_state = state_dir.join("overlays").join(&proj_id);
        let overlay_src = abs.join(OVERLAY_DIR);
        let global_src = state_dir.join(GLOBAL_DIR);

        Ok(Sandbox {
            key: state_dir.join("id_ed25519"),
            ssh_conf: state_dir.join("ssh_config"),
            claude_dir: home.join(".claude-sandbox"),
            name: format!("claude-sandbox-{proj_id}"),
            target: format!("/home/{user}/Projects/{base}"),
            // The account is baked into the image, so it belongs in the tag:
            // a different CLAUDE_SANDBOX_USER then builds its own image rather
            // than silently booting one whose only account it cannot log in as.
            base_image: format!("{IMAGE_REPO}:{user}"),
            memory: opts.memory.clone().unwrap_or_else(|| DEFAULT_MEMORY.to_string()),
            cpus: opts.cpus.clone().unwrap_or_else(|| DEFAULT_CPUS.to_string()),
            proj_id,
            overlay_src,
            overlay_state,
            global_src,
            user,
            home,
            state_dir,
            abs,
        })
    }

    fn stop(&self) -> Result<()> {
        require_cli()?;
        if self.state()?.is_none() {
            println!("{} does not exist (idle VMs delete themselves)", self.name);
            return Ok(());
        }
        capture("container", &["stop", &self.name])?;
        println!("stopped {} (it deletes itself; reopen the project to recreate)", self.name);
        Ok(())
    }

    fn remove(&self) -> Result<()> {
        require_cli()?;
        let existed = self.state()?.is_some();
        self.destroy();
        self.strip_ssh_block();
        // The acceptance record survives (it is authored config, not derived
        // state — `overlay --forget` drops it); only the note of which image
        // the now-deleted container was created from goes.
        let _ = fs::remove_file(self.image_record());
        if existed {
            println!("deleted {}", self.name);
        } else {
            println!("nothing to delete: {} does not exist", self.name);
        }
        Ok(())
    }

    /// Make the container not exist. Containers run with --rm delete
    /// themselves on stop, so either step may find nothing to do.
    fn destroy(&self) {
        let _ = capture("container", &["stop", &self.name]);
        let _ = capture("container", &["rm", &self.name]);
    }

    /// Drop this container's address block from the managed ssh config.
    fn strip_ssh_block(&self) {
        let Ok(existing) = fs::read_to_string(&self.ssh_conf) else {
            return;
        };
        let mut kept = String::new();
        let mut skipping = false;
        for line in existing.lines() {
            if let Some(tag) = line.strip_prefix("# BEGIN ") {
                skipping = tag == self.name;
            }
            if !skipping {
                kept.push_str(line);
                kept.push('\n');
            }
            if let Some(tag) = line.strip_prefix("# END ") {
                if tag == self.name {
                    skipping = false;
                }
            }
        }
        let _ = fs::write(&self.ssh_conf, kept);
    }

    fn up(&self, cmd: Cmd, opts: &Opts, ssh_args: &[String]) -> Result<()> {
        let domain = preflight()?;
        self.ensure_key()?;
        self.seed_claude_config()?;

        // Ask about the overlay before building anything: the answer decides
        // which image is wanted, and "skip" should not arrive after a build.
        let overlay = self.resolve_overlay(opts)?;
        let global = self.resolve_global(opts)?;

        if opts.rebuild || !self.have_image(&self.base_image)? {
            self.build_image()?;
            // A base rebuild keeps the same tag, so the image-change check in
            // ensure_container cannot see it; say so explicitly.
            if opts.rebuild && self.state()?.is_some() {
                self.destroy();
                println!("recreating {} with the new image", self.name);
            }
        }
        // base -> global -> project, each layer built on whatever is beneath it.
        let mut image = self.base_image.clone();
        if let Some(fp) = &global {
            image = self.ensure_global_image(fp)?;
        }
        if let Some(fp) = &overlay {
            image = self.ensure_overlay_image(fp, &image)?;
        }

        self.ensure_container(&image)?;
        // Connect by IP, not by name: the runtime's DNS record appears some
        // seconds after the container does, and a lookup made in that window
        // gets an NXDOMAIN that macOS then caches — poisoning every retry (and
        // Zed's own connect) for the life of the negative TTL. `inspect` is
        // authoritative, so the address that actually accepted a connection is
        // pinned into the ssh config as HostName and the DNS name is kept only
        // as a stable alias.
        let host = format!("{}.{domain}", self.name);
        let ip = self.wait_for_sshd(&host)?;
        self.ensure_ssh_config(&domain, &ip)?;

        if cmd == Cmd::Shell {
            let mut ssh = Command::new("ssh");
            ssh.arg(&host).args(ssh_args);
            return Err(ssh.exec()).context("failed to exec ssh");
        }
        println!("opening zed on {host} at {}", self.target);
        self.zed_trust_notice();
        let url = format!("ssh://{}@{host}{}", self.user, self.target);
        Err(Command::new("zed").arg(url).exec()).context("failed to exec zed")
    }

    /// Zed opens a worktree it has not been told to trust in Restricted Mode:
    /// no language servers, no MCP servers, and `.zed/settings.json` ignored.
    /// Trust is recorded against the ssh host name in Zed's own database on
    /// the host, so the launcher cannot grant it — only Zed can, from the
    /// title bar or from the one global setting. Say so once and then stay
    /// out of the way.
    fn zed_trust_notice(&self) {
        let notice = self.state_dir.join("zed-trust-notice");
        if notice.exists() {
            return;
        }
        // Any mention of the setting — `true` or `false` — means the question
        // has already been answered deliberately; only silence earns the note.
        let settings = self.home.join(".config/zed/settings.json");
        let decided = fs::read_to_string(settings).is_ok_and(|s| s.contains("trust_all_worktrees"));
        if !decided {
            eprintln!(
                "claude-sandbox: note: Zed opens each sandbox in Restricted Mode - no language\n  \
                 servers, no MCP servers, no .zed/settings.json. Lift it from the Restricted\n  \
                 Mode indicator in Zed's title bar (once per project: the choice is keyed to\n  \
                 the VM's ssh name, which every future VM for this directory reuses), or for\n  \
                 every project at once by adding to ~/.config/zed/settings.json:\n\n      \
                 \"session\": {{ \"trust_all_worktrees\": true }}\n\n  \
                 That one is global - projects you open locally get trusted too.\n  \
                 Shown once; delete {} to see it again.",
                notice.display()
            );
        }
        let _ = fs::write(&notice, "");
    }

    /// Seed the VM's ~/.claude (the shared ~/.claude-sandbox mount) from the
    /// host so Claude Code inside the VM starts authenticated.
    ///
    /// Deliberately a one-way copy of a short allowlist rather than a bind
    /// mount of the real ~/.claude. Mounting that directory would hand the
    /// sandbox read access to ~/.claude/projects — every transcript from
    /// every project, hundreds of MB — and, worse, write access to
    /// settings.json/hooks/skills that the *host's* Claude Code later
    /// executes, which turns a compromised sandbox into host code execution.
    /// Copying in means the VM sees only what is listed here, and nothing it
    /// writes can reach anything the host runs.
    fn seed_claude_config(&self) -> Result<()> {
        fs::create_dir_all(&self.claude_dir)?;
        self.seed_credentials()?;

        // Config worth having inside the VM. Everything else in ~/.claude —
        // transcripts, history, caches, telemetry, machine-local settings —
        // is intentionally left behind.
        const COPY: &[&str] = &[
            "settings.json", "CLAUDE.md", "agents", "commands",
            "skills", "output-styles", "plugins",
        ];
        let src_root = self.home.join(".claude");
        for name in COPY {
            let src = src_root.join(name);
            if src.exists() {
                copy_tree(&src, &self.claude_dir.join(name))
                    .with_context(|| format!("copying {name} into the sandbox config"))?;
            }
        }
        Ok(())
    }

    /// Claude Code on macOS keeps its OAuth tokens in the login Keychain, so
    /// there is no file to copy — export them into the credentials file the
    /// Linux build reads.
    fn seed_credentials(&self) -> Result<()> {
        let dest = self.claude_dir.join(".credentials.json");
        let host = match capture(
            "security",
            &["find-generic-password", "-s", "Claude Code-credentials", "-w"],
        ) {
            Ok(s) => s,
            Err(_) => {
                if !dest.exists() {
                    eprintln!(
                        "claude-sandbox: warning: no Claude Code credentials in the login \
                         keychain; run `claude login` inside the VM (or on the host first)"
                    );
                }
                return Ok(());
            }
        };
        // Both sides refresh these tokens independently, and overwriting a
        // fresher token with a staler one can invalidate the newer session —
        // so only replace the VM's copy when the host's lives longer.
        let force = env::var_os("CLAUDE_SANDBOX_RESEED").is_some();
        if !force && dest.exists() {
            let theirs = fs::read_to_string(&dest).unwrap_or_default();
            if token_expiry(&theirs) >= token_expiry(&host) {
                return Ok(());
            }
        }
        fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&dest)?;
        fs::write(&dest, host.trim_end())?;
        println!("seeded Claude credentials into {}", dest.display());
        Ok(())
    }

    fn ensure_key(&self) -> Result<()> {
        if self.key.exists() {
            return Ok(());
        }
        fs::create_dir_all(&self.state_dir)?;
        capture(
            "ssh-keygen",
            &["-q", "-t", "ed25519", "-N", "", "-C", "claude-sandbox",
              "-f", &self.key.to_string_lossy()],
        )?;
        println!("generated ssh key {}", self.key.display());
        Ok(())
    }

    fn build_image(&self) -> Result<()> {
        println!("building {} ...", self.base_image);

        // Authorize the dedicated key plus any personal keys, so manual ssh
        // works too. Key files are split per line (one file may hold several
        // keys) and joined with a literal \n expanded by printf in the
        // Dockerfile, because a raw newline anywhere in a build-arg crashes
        // the builder.
        let mut sources = vec![fs::read_to_string(self.key.with_extension("pub"))?];
        if let Ok(entries) = fs::read_dir(self.home.join(".ssh")) {
            for entry in entries.flatten() {
                let fname = entry.file_name().to_string_lossy().into_owned();
                if fname.starts_with("id_") && fname.ends_with(".pub") {
                    if let Ok(k) = fs::read_to_string(entry.path()) {
                        sources.push(k);
                    }
                }
            }
        }
        let keys: Vec<&str> = sources
            .iter()
            .flat_map(|s| s.lines())
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let pubkeys = keys.join("\\n");

        // Build from a temp context holding the embedded Dockerfile/entrypoint.
        let ctx = env::temp_dir().join(format!("claude-sandbox-ctx-{}", std::process::id()));
        fs::create_dir_all(&ctx)?;
        fs::write(ctx.join("Dockerfile"), DOCKERFILE)?;
        fs::write(ctx.join("entrypoint.sh"), ENTRYPOINT)?;
        let built = passthrough(
            "container",
            &["build", "-t", self.base_image.as_str(),
              "--build-arg", &format!("SSH_PUBKEY={pubkeys}"),
              "--build-arg", &format!("USERNAME={}", self.user),
              &ctx.to_string_lossy()],
        );
        let _ = fs::remove_dir_all(&ctx);
        built?;
        self.bump_base_stamp()
    }

    fn have_image(&self, tag: &str) -> Result<bool> {
        Ok(capture("container", &["image", "inspect", tag]).is_ok())
    }

    /// A value that changes every time the base image is actually rebuilt.
    ///
    /// Overlay images sit on top of the base, so a base rebuild leaves every
    /// project's overlay stale — but the base keeps its tag across rebuilds,
    /// so the tag cannot say so. Folding this stamp into each overlay's tag
    /// makes any base rebuild, from any project, invalidate them all.
    fn base_stamp_path(&self) -> PathBuf {
        self.state_dir.join(format!("base-{}.stamp", self.user))
    }

    fn base_stamp(&self) -> Result<String> {
        if let Ok(s) = fs::read_to_string(self.base_stamp_path()) {
            let s = s.trim().to_string();
            if !s.is_empty() {
                return Ok(s);
            }
        }
        // No stamp yet: an image built before this existed, or a fresh state
        // directory. Adopt a fixed value rather than a fresh one, so merely
        // reading it can never look like a rebuild.
        fs::create_dir_all(&self.state_dir)?;
        fs::write(self.base_stamp_path(), "0\n")?;
        Ok("0".to_string())
    }

    fn bump_base_stamp(&self) -> Result<()> {
        let n = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        fs::create_dir_all(&self.state_dir)?;
        fs::write(self.base_stamp_path(), format!("{n}\n"))?;
        Ok(())
    }

    // ---- Per-project image overlay ---------------------------------------

    /// The snapshot of the overlay contents the host has accepted. It is both
    /// the thing a change is diffed against and the context builds run from.
    fn overlay_snapshot(&self) -> PathBuf {
        self.overlay_state.join("accepted")
    }

    fn overlay_record(&self) -> PathBuf {
        self.overlay_state.join("accepted.json")
    }

    /// Decide whether this run has an overlay to build, prompting when the
    /// project's overlay directory is new or has changed since it was last
    /// accepted. Returns the accepted contents' fingerprint.
    fn resolve_overlay(&self, opts: &Opts) -> Result<Option<String>> {
        if !self.overlay_src.is_dir() {
            return Ok(None);
        }
        if opts.no_overlay {
            println!("--no-overlay: ignoring {}", self.overlay_src.display());
            return Ok(None);
        }
        let current = scan_context(&self.overlay_src)?;
        if !current.iter().any(|e| e.path == "Dockerfile") {
            println!(
                "note: {} has no Dockerfile, so no image overlay is applied\n  \
                 (the directory is still mounted read-only inside the VM)",
                self.overlay_src.display()
            );
            return Ok(None);
        }

        let snap = self.overlay_snapshot();
        let accepted = if snap.is_dir() { Some(scan_context(&snap)?) } else { None };
        // Compared byte for byte rather than by fingerprint: the hash is what
        // gets recorded and what names the image, but the gate itself owes
        // nothing to the hash function holding up.
        if accepted.as_deref() == Some(&current[..]) {
            return Ok(Some(fingerprint(&current)));
        }

        if opts.accept_overlay || env::var_os("CLAUDE_SANDBOX_ACCEPT_OVERLAY").is_some() {
            return Ok(Some(self.accept_overlay(&current)?));
        }
        match self.prompt_overlay(&current, accepted.is_some())? {
            Decision::Accept => Ok(Some(self.accept_overlay(&current)?)),
            Decision::Revert => {
                self.revert_overlay()?;
                Ok(Some(fingerprint(&scan_context(&self.overlay_src)?)))
            }
            Decision::Skip => {
                println!("skipping the overlay: this VM boots the base image");
                Ok(None)
            }
            Decision::Quit => bail!("aborted"),
        }
    }

    /// Record the overlay's current contents as accepted: snapshot the bytes
    /// (so later changes can be diffed and reverted, and so builds read a copy
    /// the VM cannot reach) and write the fingerprint alongside them.
    fn accept_overlay(&self, current: &[CtxEntry]) -> Result<String> {
        let snap = self.overlay_snapshot();
        fs::create_dir_all(&self.overlay_state)?;
        if snap.exists() {
            fs::remove_dir_all(&snap)?;
        }
        copy_context(&self.overlay_src, &snap)
            .with_context(|| format!("snapshotting {}", self.overlay_src.display()))?;
        // Close the gap between "these are the bytes you were shown" and
        // "these are the bytes that were stored": a write landing in between
        // would otherwise be accepted without ever having been displayed.
        if scan_context(&snap)? != current {
            fs::remove_dir_all(&snap)?;
            bail!(
                "{} changed while it was being accepted; nothing was recorded",
                self.overlay_src.display()
            );
        }

        let fp = fingerprint(current);
        let at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let record = serde_json::json!({
            "fingerprint": fp,
            "project": self.abs.to_string_lossy(),
            "accepted_at": at,
        });
        fs::write(self.overlay_record(), format!("{}\n", serde_json::to_string_pretty(&record)?))?;
        println!("accepted the image overlay for {} ({}…)", self.abs.display(), &fp[..12]);
        Ok(fp)
    }

    /// Put the accepted contents back, dropping anything the working copy
    /// added. The snapshot is the source of truth, so the destination is
    /// replaced wholesale rather than merged — and it stays intact throughout,
    /// so a failure part-way through loses nothing that cannot be retried.
    fn revert_overlay(&self) -> Result<()> {
        let snap = self.overlay_snapshot();
        if !snap.is_dir() {
            bail!("nothing to revert to: no accepted overlay recorded for {}", self.abs.display());
        }
        // Restoring into a project that is no longer there would conjure the
        // directory back into existence holding nothing but the overlay.
        if !self.abs.is_dir() {
            bail!("cannot revert: {} does not exist", self.abs.display());
        }
        if self.overlay_src.exists() {
            fs::remove_dir_all(&self.overlay_src)?;
        }
        copy_context(&snap, &self.overlay_src)?;
        println!("reverted {} to the accepted contents", self.overlay_src.display());
        Ok(())
    }

    /// Show what is being asked about and read a decision.
    fn prompt_overlay(&self, current: &[CtxEntry], have_accepted: bool) -> Result<Decision> {
        if have_accepted {
            println!("\nThe image overlay for this project has changed since you accepted it:\n");
            show_diff(&self.overlay_snapshot(), &self.overlay_src);
        } else {
            println!("\nThis project carries an image overlay that has not been accepted:\n");
            show_context(current);
        }
        println!(
            "\n  {}\n\n  \
             These instructions run as root while building this project's VM image.\n  \
             The directory is mounted read-only inside the VM, so a change here is\n  \
             expected to have come from the host.\n",
            self.overlay_src.display()
        );

        // Never block on a prompt nobody can answer — but only after printing
        // the above, so a log from a non-interactive run still shows why.
        if !io::stdin().is_terminal() {
            bail!(
                "the overlay must be accepted before it is built, and there is no \
                 terminal to ask on.\n  \
                 review and accept it with:  claude-sandbox overlay --accept {}\n  \
                 or ignore it with:          --no-overlay",
                self.abs.display()
            );
        }
        loop {
            if have_accepted {
                print!("[a] accept and build  [r] revert to accepted  [s] skip  [q] quit > ");
            } else {
                print!("[a] accept and build  [s] skip  [q] quit > ");
            }
            io::stdout().flush()?;
            let mut line = String::new();
            // EOF mid-prompt is not consent.
            if io::stdin().read_line(&mut line)? == 0 {
                println!();
                return Ok(Decision::Quit);
            }
            match line.trim().to_ascii_lowercase().as_str() {
                "a" | "accept" => return Ok(Decision::Accept),
                "r" | "revert" if have_accepted => return Ok(Decision::Revert),
                // Bare Enter takes the option that changes nothing.
                "" | "s" | "skip" => return Ok(Decision::Skip),
                "q" | "quit" => return Ok(Decision::Quit),
                other => println!("  not one of the options: {other:?}"),
            }
        }
    }

    /// Is the global overlay in play this run, and what are its contents?
    ///
    /// No acceptance step: unlike a project overlay this is not something a
    /// repository brought with it, and no sandbox can reach it — so there is
    /// nobody to gate against, and a prompt here would only train the reflex
    /// that answers the one that matters without reading.
    fn resolve_global(&self, opts: &Opts) -> Result<Option<String>> {
        if !self.global_src.is_dir() {
            return Ok(None);
        }
        if opts.no_overlay {
            println!("--no-overlay: ignoring {}", self.global_src.display());
            return Ok(None);
        }
        // The claim above — that no sandbox can reach it — holds because the
        // state directory is not one of the two mounts. Sandboxing a directory
        // that contains it (`~`, a dotfiles repo) breaks that, and a global
        // overlay the guest can rewrite is exactly what the per-project gate
        // exists to prevent. Note that this is not the worst of it: that mount
        // also hands the guest the ssh key every other sandbox is reached with.
        let state = fs::canonicalize(&self.state_dir).unwrap_or_else(|_| self.state_dir.clone());
        if state.starts_with(&self.abs) {
            eprintln!(
                "claude-sandbox: warning: {} is inside the project being mounted, so the \
                 VM could rewrite it.\n  Skipping the global overlay for this project.{}",
                state.display(),
                if self.key.starts_with(&self.abs) {
                    "\n  warning: this mount also exposes the ssh key in that directory \
                     to the guest, which is the key every other sandbox is reached with."
                } else {
                    ""
                }
            );
            return Ok(None);
        }
        let entries = scan_context(&self.global_src)?;
        if !entries.iter().any(|e| e.path == "Dockerfile") {
            println!(
                "note: {} has no Dockerfile, so no global overlay is applied",
                self.global_src.display()
            );
            return Ok(None);
        }
        Ok(Some(fingerprint(&entries)))
    }

    fn ensure_global_image(&self, fp: &str) -> Result<String> {
        self.ensure_layer(
            &format!("glb-{}", self.user),
            &self.state_dir.join("overlays").join(GLOBAL_RECORD),
            &self.global_src,
            fp,
            &self.global_src,
            &self.base_image,
        )
    }

    /// The project layer is built from the accepted snapshot rather than from
    /// the project, so the bytes handed to the builder are exactly the ones
    /// that were shown and accepted, with no window in between to change them.
    fn ensure_overlay_image(&self, fp: &str, parent: &str) -> Result<String> {
        self.ensure_layer(
            &format!("ovl-{}", self.proj_id),
            &self.overlay_state,
            &self.overlay_snapshot(),
            fp,
            &self.overlay_src,
            parent,
        )
    }

    /// Build (or reuse) one image layer on top of `parent`.
    ///
    /// The tag carries a fingerprint of the layer's contents, of the image it
    /// sits on, and of the base image's build stamp — so "is this image
    /// current?" is answered by whether the tag exists. Editing a layer and
    /// editing it back costs nothing; changing one invalidates everything
    /// stacked above it. No bookkeeping to go stale, and it heals itself if
    /// images are pruned by hand.
    ///
    /// `context` is the directory built; `source` is the path to name in
    /// messages, which differs when the two are not the same (the project
    /// layer builds a snapshot of a directory the user authors elsewhere).
    fn ensure_layer(
        &self,
        stem: &str,
        record: &Path,
        context: &Path,
        fp: &str,
        source: &Path,
        parent: &str,
    ) -> Result<String> {
        let mut h = Sha256::new();
        h.update(fp.as_bytes());
        h.update([0]);
        h.update(parent.as_bytes());
        h.update([0]);
        h.update(self.base_stamp()?.as_bytes());
        let tag = format!("{IMAGE_REPO}:{stem}-{}", &format!("{:x}", h.finalize())[..8]);
        if self.have_image(&tag)? {
            return Ok(tag);
        }

        let fragment = fs::read_to_string(context.join("Dockerfile"))
            .with_context(|| format!("reading {}", context.join("Dockerfile").display()))?;
        let body = check_fragment(&fragment)?;

        let build = record.join("build");
        if build.exists() {
            fs::remove_dir_all(&build)?;
        }
        copy_context(context, &build)?;
        // Overwrites the copy of the authored Dockerfile: the build reads the
        // generated one, and the original stays where it was written.
        fs::write(build.join("Dockerfile"), self.generated_dockerfile(&body, source, parent))?;

        println!("building {tag} from {} ...", source.display());
        passthrough(
            "container",
            &["build", "-t", &tag,
              "--build-arg", &format!("USERNAME={}", self.user),
              &build.to_string_lossy()],
        )
        .with_context(|| format!("building the image layer from {}", source.display()))?;
        self.prune_layer_images(record, &tag);
        Ok(tag)
    }

    /// The Dockerfile actually built: the authored fragment with a `FROM` in
    /// front of it and the directives that must survive it behind.
    fn generated_dockerfile(&self, body: &str, source: &Path, parent: &str) -> String {
        format!(
            "# Generated by claude-sandbox from {src}/Dockerfile.\n\
             # Edit that file, not this one — this copy is rewritten every build.\n\
             FROM {parent}\n\
             # ARG does not survive FROM, so it is re-declared for the fragment.\n\
             ARG USERNAME={user}\n\
             USER root\n\
             # ---- begin overlay ----\n\
             {body}\
             # ---- end overlay ----\n\
             # Re-asserted after the fragment (last one wins) so a stray directive in it\n\
             # cannot leave the VM without the entrypoint that installs the egress\n\
             # firewall. This is a guard against accident, not against a hostile overlay:\n\
             # the fragment runs as root and can rewrite anything beneath it, which is why\n\
             # a project's is not built until the host has accepted it.\n\
             USER root\n\
             EXPOSE 22\n\
             ENTRYPOINT [\"/usr/local/bin/entrypoint.sh\"]\n",
            src = source.display(),
            user = self.user,
        )
    }

    /// Drop the tag this layer's previous build produced. Layer tags are
    /// content-addressed, so an old one is never reused and they would
    /// otherwise accumulate one per edit; only the layers unique to them are
    /// freed, since everything underneath is shared.
    fn prune_layer_images(&self, record: &Path, keep: &str) {
        let marker = record.join("built.tag");
        if let Ok(prev) = fs::read_to_string(&marker) {
            let prev = prev.trim();
            // Two spellings because the runtime's is the one that matters and
            // this is best-effort cleanup either way.
            if !prev.is_empty()
                && prev != keep
                && capture("container", &["image", "rm", prev]).is_err()
            {
                let _ = capture("container", &["image", "delete", prev]);
            }
        }
        let _ = fs::write(&marker, format!("{keep}\n"));
    }

    /// `claude-sandbox overlay [action] <dir>` — all host-side, so it needs
    /// neither the runtime nor a VM.
    fn overlay_cmd(&self, action: OverlayAction) -> Result<()> {
        match action {
            OverlayAction::Revert => self.revert_overlay(),
            OverlayAction::Forget => {
                if self.overlay_state.exists() {
                    fs::remove_dir_all(&self.overlay_state)?;
                    println!("forgot the overlay record for {}", self.abs.display());
                } else {
                    println!("no overlay record for {}", self.abs.display());
                }
                Ok(())
            }
            OverlayAction::Accept => {
                if !self.overlay_src.is_dir() {
                    bail!("nothing to accept: {} does not exist", self.overlay_src.display());
                }
                self.accept_overlay(&scan_context(&self.overlay_src)?)?;
                Ok(())
            }
            OverlayAction::Status => self.overlay_status(),
        }
    }

    fn overlay_status(&self) -> Result<()> {
        // The whole stack, so it is clear from one command what a VM for this
        // project is actually built from.
        println!(
            "global:   {}{}",
            self.global_src.display(),
            if self.global_src.is_dir() {
                "  (applied to every project)"
            } else {
                "  (none — create it to extend every project's image)"
            }
        );
        println!("overlay:  {}", self.overlay_src.display());
        println!("record:   {}", self.overlay_state.display());
        if !self.overlay_src.is_dir() {
            println!("status:   absent — this project adds nothing of its own");
            return Ok(());
        }
        let current = scan_context(&self.overlay_src)?;
        let snap = self.overlay_snapshot();
        let accepted = if snap.is_dir() { Some(scan_context(&snap)?) } else { None };
        match accepted {
            Some(ref a) if a == &current => {
                println!("status:   accepted ({}…)", &fingerprint(&current)[..12]);
            }
            Some(_) => {
                println!("status:   CHANGED since it was accepted\n");
                show_diff(&snap, &self.overlay_src);
                println!(
                    "\naccept with:  claude-sandbox overlay --accept {}\
                     \nrevert with:  claude-sandbox overlay --revert {}",
                    self.abs.display(),
                    self.abs.display()
                );
            }
            None => {
                println!("status:   NEW — never accepted\n");
                show_context(&current);
                println!("\naccept with:  claude-sandbox overlay --accept {}", self.abs.display());
            }
        }
        if !current.iter().any(|e| e.path == "Dockerfile") {
            println!("\nnote: there is no Dockerfile here, so no image overlay is built.");
        }
        Ok(())
    }

    /// None = the container does not exist. Other inspect failures (daemon
    /// down, transient errors) propagate instead of masquerading as missing.
    fn state(&self) -> Result<Option<String>> {
        let out = Command::new("container")
            .args(["inspect", &self.name])
            .stdin(Stdio::null())
            .output()
            .context("failed to run container inspect")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if err.contains("not found") {
                return Ok(None);
            }
            bail!("container inspect {} failed: {}", self.name, err.trim());
        }
        let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout))?;
        Ok(v[0]["status"]["state"].as_str().map(String::from))
    }

    /// (cpus, memory bytes) the VM was actually created with.
    fn resources(&self) -> Option<(u64, u64)> {
        let out = capture("container", &["inspect", &self.name]).ok()?;
        let v: serde_json::Value = serde_json::from_str(&out).ok()?;
        let r = &v[0]["configuration"]["resources"];
        Some((r["cpus"].as_u64()?, r["memoryInBytes"].as_u64()?))
    }

    /// Limits are fixed when the VM is created, and a running one is reused as
    /// is, so `-m`/`-c` on an already-open project would otherwise do nothing
    /// at all — quietly, which reads as though the resize took.
    fn warn_if_limits_differ(&self) {
        let Some((cpus, mem)) = self.resources() else {
            return;
        };
        let want_cpus: u64 = self.cpus.parse().unwrap_or(cpus);
        let want_mem = memory_bytes(&self.memory).unwrap_or(mem);
        if (cpus, mem) == (want_cpus, want_mem) {
            return;
        }
        println!(
            "note: {} is already running with {cpus} cpus / {} — limits are set at \
             creation.\n  to run it with {} cpus / {}:  claude-sandbox rm {}",
            self.name,
            human_bytes(mem),
            self.cpus,
            self.memory,
            self.abs.display()
        );
    }

    /// Where the tag a container was created from is noted, for the staleness
    /// check in `ensure_container`.
    fn image_record(&self) -> PathBuf {
        self.state_dir.join("containers").join(format!("{}.image", self.name))
    }

    /// The image a running container was created from, or None if it cannot be
    /// established. `inspect` is authoritative when it carries the reference;
    /// the note written at creation covers the case where it does not.
    fn container_image(&self) -> Option<String> {
        if let Ok(out) = capture("container", &["inspect", &self.name]) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&out) {
                let cfg = &v[0]["configuration"];
                for node in [&cfg["image"]["reference"], &cfg["image"], &v[0]["image"]] {
                    if let Some(s) = node.as_str() {
                        return Some(s.to_string());
                    }
                }
            }
        }
        fs::read_to_string(self.image_record())
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn ensure_container(&self, image: &str) -> Result<()> {
        // Only a running container is reused. Anything else is torn down and
        // recreated (~2s): these VMs are ephemeral by design — all state lives
        // in the two mounts — and restarting one churns its address, which the
        // runtime reports stale for a window afterwards.
        let mut state = self.state()?;
        if !matches!(state.as_deref(), Some("running") | None) {
            self.destroy();
            state = None;
        }
        // A running VM was built from whatever image was current when it
        // started, so an overlay edited since then has not taken effect.
        // Recreating costs seconds and is what "I changed my Dockerfile" asks
        // for; an unknown previous image is left alone rather than churning
        // every launch, and resolves itself once one has been recorded.
        if state.as_deref() == Some("running") {
            if let Some(prev) = self.container_image() {
                if prev != image {
                    println!("image changed ({prev} -> {image}) — recreating {}", self.name);
                    self.destroy();
                    state = None;
                }
            }
        }
        match state.as_deref() {
            Some("running") => {
                self.warn_if_limits_differ();
                Ok(())
            }
            None => {
                let idle = match env::var("CLAUDE_SANDBOX_IDLE") {
                    Ok(v) => {
                        v.parse::<u64>().map_err(|_| {
                            anyhow!("CLAUDE_SANDBOX_IDLE must be a whole number of seconds, got: {v}")
                        })?;
                        Some(format!("IDLE_TIMEOUT={v}"))
                    }
                    Err(_) => None,
                };
                let debug = env::var_os("CLAUDE_SANDBOX_DEBUG").is_some();
                let mount_project = format!("{}:{}", self.abs.display(), self.target);
                let mount_claude =
                    format!("{}:/home/{}/.claude", self.claude_dir.display(), self.user);
                // Read-only in the guest whenever the directory exists, which
                // is independent of whether this run builds from it: --rebuild
                // and --no-overlay change what runs, not what is protected.
                let overlay_env = format!("OVERLAY_DIR={}/{OVERLAY_DIR}", self.target);

                let mut args: Vec<&str> = vec![
                    "run",
                    "-d",
                    "--name",
                    &self.name,
                    "--cap-add",
                    "CAP_NET_ADMIN",
                    "-m",
                    &self.memory,
                    "-c",
                    &self.cpus,
                ];
                if !debug {
                    args.push("--rm");
                }
                if let Some(e) = idle.as_deref() {
                    args.extend(["-e", e]);
                }
                if self.overlay_src.is_dir() {
                    args.extend(["-e", &overlay_env]);
                }
                args.extend(["-v", &mount_project, "-v", &mount_claude, image]);
                capture("container", &args)?;
                let record = self.image_record();
                if let Some(parent) = record.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&record, format!("{image}\n"))?;
                println!(
                    "created {} ({} cpus, {} memory) ({} -> {})",
                    self.name,
                    self.cpus,
                    self.memory,
                    self.abs.display(),
                    self.target
                );
                Ok(())
            }
            Some(other) => bail!("container {} is in unexpected state: {other}", self.name),
        }
    }

    /// Is sshd listening inside the VM? Asked over vsock via `container exec`,
    /// which needs no local-network access, so this works even when this
    /// process is barred from the VM's subnet.
    fn sshd_listening_via_exec(&self) -> bool {
        capture(
            "container",
            &["exec", &self.name, "sh", "-c",
              "awk 'FNR > 1 { split($2, a, \":\"); if (a[2] == \"0016\" && $4 == \"0A\") f = 1 } \
               END { exit !f }' /proc/net/tcp /proc/net/tcp6"],
        )
        .is_ok()
    }

    /// The address reported by `inspect`, if the runtime has assigned one.
    fn current_ip(&self) -> Result<Option<String>> {
        let out = capture("container", &["inspect", &self.name])?;
        let v: serde_json::Value = serde_json::from_str(&out)?;
        Ok(v[0]["status"]["networks"][0]["ipv4Address"]
            .as_str()
            .and_then(|cidr| cidr.split('/').next())
            .filter(|ip| !ip.is_empty())
            .map(String::from))
    }

    /// Wait for sshd, re-reading the address every poll: the runtime assigns
    /// it a moment after the container appears and can report a previous
    /// incarnation's address in between, so a single snapshot may be stale or
    /// go stale mid-wait. Returns the address that actually accepted a
    /// connection, which is what gets pinned into the ssh config.
    fn wait_for_sshd(&self, host: &str) -> Result<String> {
        let mut last = String::new();
        let mut last_err: Option<String> = None;
        let mut blocked = 0;
        for i in 0..60 {
            // A firewall failure makes the entrypoint exit instead of starting
            // sshd, and --rm then deletes the container.
            if self.state()?.as_deref() != Some("running") {
                self.dump_logs_or_hint();
                bail!("{} stopped before sshd came up", self.name);
            }
            if let Some(ip) = self.current_ip()? {
                if ip != last {
                    if !last.is_empty() {
                        println!("address changed: {last} -> {ip}");
                    }
                    last = ip.clone();
                }
                if let Ok(addr) = format!("{ip}:22").parse::<std::net::SocketAddr>() {
                    match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
                        Ok(_) => return Ok(ip),
                        Err(e) => {
                            // macOS gates connections to local-network addresses
                            // per app and reports a denial as "No route to host"
                            // (EHOSTUNREACH), not a permission error. The VM is
                            // confirmed running above, so after a few seconds of
                            // this there is no route because we are not allowed
                            // one — say so instead of spinning for a minute.
                            // A freshly booted VM is briefly unreachable this
                            // way too (the host has not learned its MAC yet),
                            // so only a sustained run of these means the route
                            // is barred rather than still settling.
                            blocked = match e.kind() {
                                io::ErrorKind::HostUnreachable
                                | io::ErrorKind::NetworkUnreachable => blocked + 1,
                                _ => 0,
                            };
                            if blocked >= 15 || e.kind() == io::ErrorKind::PermissionDenied {
                                // This process is barred from the VM's subnet,
                                // but `container exec` reaches the guest over
                                // vsock, so sshd can still be verified — and
                                // Zed.app carries its own grant, so it can
                                // connect even when this terminal cannot.
                                if self.sshd_listening_via_exec() {
                                    eprintln!(
                                        "claude-sandbox: warning: still cannot reach {ip}:22 after {blocked}s ({e}), \
                                         but sshd is confirmed listening inside the VM.{}",
                                        local_network_hint()
                                    );
                                    return Ok(ip);
                                }
                                bail!("cannot reach {ip}:22: {e}{}", local_network_hint());
                            }
                            last_err = Some(e.to_string());
                        }
                    }
                }
            }
            // Belt and braces: if the address from `inspect` is unusable, the
            // runtime's DNS may still know where the VM is (the two have
            // failed independently). Whatever answers wins.
            if i >= 5 {
                if let Ok(addrs) = (host, 22u16).to_socket_addrs() {
                    for addr in addrs.filter(|a| a.is_ipv4()) {
                        if TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok() {
                            println!("reached {host} at {} via DNS", addr.ip());
                            return Ok(addr.ip().to_string());
                        }
                    }
                }
            }
            if i > 0 && i % 5 == 0 {
                let where_ = if last.is_empty() { self.name.clone() } else { format!("{last}:22") };
                match &last_err {
                    Some(e) => println!("waiting for sshd on {where_} ({i} attempts; last error: {e}) ..."),
                    None => println!("waiting for sshd on {where_} ({i} attempts) ..."),
                }
            }
            sleep(Duration::from_secs(1));
        }
        // The VM is up (state checked above) but unreachable from this
        // process, so logs are unlikely to help; the usual cause is the
        // per-app local-network grant.
        bail!(
            "timed out waiting for sshd on {}{}{}",
            if last.is_empty() { self.name.clone() } else { format!("{last}:22") },
            last_err.map(|e| format!(" (last error: {e})")).unwrap_or_default(),
            local_network_hint()
        )
    }

    /// Best-effort diagnostics: --rm containers delete themselves on exit,
    /// taking their logs with them, so explain that when logs are gone.
    fn dump_logs_or_hint(&self) {
        if passthrough("container", &["logs", &self.name]).is_err() {
            eprintln!(
                "(no logs: the container already deleted itself (--rm); re-run with \
                 CLAUDE_SANDBOX_DEBUG=1 to keep failed VMs for `container logs`)"
            );
        }
    }

    /// The managed ssh config holds one block per container pinning its
    /// current address (so ssh and Zed never call getaddrinfo for these
    /// names), plus a shared defaults block for identity and host-key policy.
    /// Patterns are scoped to claude-sandbox-* so unrelated containers under
    /// the same DNS domain keep the user's own ssh settings. Blocks for other
    /// projects are preserved; this container's is rewritten every run, so a
    /// recycled address can never go stale.
    fn ensure_ssh_config(&self, domain: &str, ip: &str) -> Result<()> {
        fs::create_dir_all(&self.state_dir)?;
        let existing = fs::read_to_string(&self.ssh_conf).unwrap_or_default();

        // Keep only other projects' marked blocks: this container's block and
        // the defaults are rewritten below, and anything unmarked is cruft
        // from an older layout that would otherwise shadow the new defaults
        // (ssh keeps the first value it reads for a keyword).
        let mut kept = String::new();
        let mut emit = false;
        for line in existing.lines() {
            if let Some(tag) = line.strip_prefix("# BEGIN ") {
                emit = tag != self.name && tag != "defaults";
            }
            if emit {
                kept.push_str(line);
                kept.push('\n');
            }
            if line.starts_with("# END ") {
                emit = false;
            }
        }

        let desired = format!(
            "{kept}\
             # BEGIN {name}\n\
             Host {name}.{domain} {name}\n\
             \x20 HostName {ip}\n\
             # END {name}\n\
             # BEGIN defaults\n\
             Host claude-sandbox-*.{domain} claude-sandbox-*\n\
             \x20 User {user}\n\
             \x20 IdentityFile {key}\n\
             \x20 IdentitiesOnly yes\n\
             \x20 StrictHostKeyChecking no\n\
             \x20 UserKnownHostsFile /dev/null\n\
             \x20 LogLevel ERROR\n\
             # END defaults\n",
            name = self.name,
            user = self.user,
            key = self.key.display(),
        );
        if existing != desired {
            fs::write(&self.ssh_conf, &desired)?;
        }
        self.ensure_include()
    }

    /// Make sure ~/.ssh/config pulls in the managed file.
    fn ensure_include(&self) -> Result<()> {
        let ssh_dir = self.home.join(".ssh");
        if !ssh_dir.is_dir() {
            fs::DirBuilder::new().mode(0o700).create(&ssh_dir)?;
        }
        let cfg_path = ssh_dir.join("config");
        let existing = if cfg_path.exists() {
            fs::read_to_string(&cfg_path)?
        } else {
            fs::OpenOptions::new()
                .create(true)
                .write(true)
                .mode(0o600)
                .open(&cfg_path)?;
            String::new()
        };
        // Tolerate indentation and keyword case so an existing Include is
        // never duplicated.
        let target = self.ssh_conf.display().to_string();
        let present = existing.lines().any(|l| {
            l.trim()
                .split_once(char::is_whitespace)
                .is_some_and(|(k, v)| k.eq_ignore_ascii_case("include") && v.trim() == target)
        });
        if present {
            return Ok(());
        }
        fs::write(&cfg_path, format!("Include {target}\n{existing}"))?;
        Ok(())
    }
}

/// The account created inside the VM, and the name ssh logs in as. Defaults
/// to whoever is running this, so the home directory in the VM matches the one
/// on the host; CLAUDE_SANDBOX_USER overrides. Linux account names are more
/// restricted than macOS ones, so the value is lowercased and anything outside
/// [a-z0-9_] becomes '-' (useradd's own rule, minus the trailing '$' form).
fn guest_user() -> Result<String> {
    let (raw, source) = match env::var("CLAUDE_SANDBOX_USER") {
        Ok(v) => (v, "CLAUDE_SANDBOX_USER"),
        Err(_) => (host_user()?, "your username"),
    };
    let mut name: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '-' })
        .collect();
    name.truncate(32); // useradd's limit
    let name = name.trim_end_matches('-').to_string();
    // useradd rejects a leading digit or '-', and root already exists (with a
    // uid the image does not use), so those cases need an explicit override
    // rather than a name invented here.
    if name.is_empty() || name == "root" || !name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_') {
        bail!(
            "cannot use {source} (\"{}\") as the account name inside the VM: it must \
             start with a letter or underscore, and cannot be root.\n  \
             pick one explicitly:  CLAUDE_SANDBOX_USER=<name> claude-sandbox <dir>",
            raw.trim()
        );
    }
    Ok(name)
}

/// The account running this process. $USER is set by every interactive shell,
/// but not by launchd or cron, so fall back to asking the system.
fn host_user() -> Result<String> {
    for key in ["USER", "LOGNAME"] {
        if let Ok(v) = env::var(key) {
            if !v.trim().is_empty() {
                return Ok(v);
            }
        }
    }
    let out = capture("id", &["-un"]).context(
        "cannot tell who you are: neither $USER nor $LOGNAME is set and `id -un` failed.\n  \
         set the VM account name explicitly with CLAUDE_SANDBOX_USER",
    )?;
    Ok(out.trim().to_string())
}

/// macOS gates connections to local-network addresses (the VMs live on
/// 192.168.64.0/24) per application, and a command-line tool inherits the
/// grant of whatever app launched it — so the same binary can reach a VM from
/// one terminal and silently fail from another.
fn local_network_hint() -> String {
    let app = env::var("TERM_PROGRAM").unwrap_or_else(|_| "your terminal app".into());
    format!(
        "\n  The VM is running but unreachable from this process. macOS requires \
         Local Network permission per app, and a terminal inherits it to \
         everything it launches:\n    \
         System Settings -> Privacy & Security -> Local Network -> enable {app}\n  \
         A prompt that was dismissed or denied makes these connections fail \
         silently, exactly like this.\n  \
         Quick check from the same terminal:  nc -vz <vm-ip> 22"
    )
}

/// Verify the `container` runtime is installed, running, and acting as the
/// host's DNS provider for container names. Returns the domain it serves.
fn preflight() -> Result<String> {
    ensure_services()?;
    let domain = dns_domain();
    // Connections are made by pinned IP, so missing DNS is a degradation
    // (names won't resolve outside the managed ssh config), not a blocker.
    if let Err(e) = check_dns(&domain) {
        eprintln!("claude-sandbox: warning: {e:#}");
    }
    Ok(domain)
}

fn require_cli() -> Result<()> {
    match Command::new("container")
        .arg("--version")
        .stdin(Stdio::null())
        .output()
    {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => bail!(
            "`container --version` failed: {}\n  \
             the Apple `container` CLI looks broken; reinstall with: brew install container",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => bail!(
            "the Apple `container` CLI is not installed (or not on PATH).\n  \
             install it with:  brew install container\n  \
             or grab a release from https://github.com/apple/container"
        ),
        Err(e) => Err(e).context("failed to run `container`"),
    }
}

/// One status call proves both that the CLI exists and that services run.
fn ensure_services() -> Result<()> {
    match Command::new("container")
        .args(["system", "status"])
        .stdin(Stdio::null())
        .output()
    {
        Err(e) if e.kind() == io::ErrorKind::NotFound => bail!(
            "the Apple `container` CLI is not installed (or not on PATH).\n  \
             install it with:  brew install container\n  \
             or grab a release from https://github.com/apple/container"
        ),
        Err(e) => Err(e).context("failed to run `container`"),
        Ok(out) if out.status.success() => Ok(()),
        Ok(_) => {
            println!("container services are not running - starting them ...");
            if passthrough("container", &["system", "start"]).is_err()
                || capture("container", &["system", "status"]).is_err()
            {
                bail!(
                    "container services are not running and could not be started.\n  \
                     try:    container system start\n  \
                     then:   container system logs"
                );
            }
            Ok(())
        }
    }
}

/// The domain the runtime appends to container hostnames, read from
/// `container system property ls` ([dns] domain = "..."). Falls back to the
/// runtime's own default; ensure_dns() reports precisely if it isn't served.
fn dns_domain() -> String {
    let props = capture("container", &["system", "property", "ls"]).unwrap_or_default();
    let mut in_dns = false;
    for line in props.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_dns = line == "[dns]";
        } else if in_dns {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim() == "domain" {
                    let value = value.trim().trim_matches('"');
                    if !value.is_empty() {
                        return value.to_string();
                    }
                }
            }
        }
    }
    "container".to_string()
}

fn check_dns(domain: &str) -> Result<()> {
    // Runtime side: the domain must be one the runtime serves.
    let served = capture("container", &["system", "dns", "list"])
        .unwrap_or_default()
        .lines()
        .any(|l| l.split_whitespace().next() == Some(domain));
    if !served {
        bail!(
            "`container` is not serving the local DNS domain `{domain}`, which \
             claude-sandbox needs to reach VMs by name.\n  \
             fix (needs admin):  sudo container system dns create {domain}\n  \
             then verify:        container system dns list"
        );
    }
    // Host side: macOS must route that domain to the runtime's resolver.
    if !os_resolver_has(domain) {
        bail!(
            "macOS is not set up to resolve `.{domain}` names: no matching entry \
             in /etc/resolver.\n  \
             `container` writes that file when the domain is created, so re-create \
             it (needs admin):\n    \
             sudo container system dns delete {domain}\n    \
             sudo container system dns create {domain}"
        );
    }
    Ok(())
}

/// macOS applies /etc/resolver/<file> to the domain named by the file itself
/// or by a `domain`/`search` line inside it (the runtime uses the latter).
fn os_resolver_has(domain: &str) -> bool {
    let Ok(entries) = fs::read_dir("/etc/resolver") else {
        return false;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy() == domain {
            return true;
        }
        let Ok(body) = fs::read_to_string(entry.path()) else {
            continue;
        };
        for line in body.lines() {
            let mut tokens = line.split_whitespace();
            if matches!(tokens.next(), Some("domain" | "search")) && tokens.any(|t| t == domain) {
                return true;
            }
        }
    }
    false
}

/// Run a command, capturing stdout; on failure the error carries stderr.
fn capture(prog: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to run {prog}"))?;
    if !out.status.success() {
        bail!(
            "{prog} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a command with inherited stdio (for build progress, logs).
fn passthrough(prog: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(prog)
        .args(args)
        .status()
        .with_context(|| format!("failed to run {prog}"))?;
    if !status.success() {
        bail!("{prog} {} failed", args.join(" "));
    }
    Ok(())
}
