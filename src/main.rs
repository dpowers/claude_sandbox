// claude-sandbox — run a project directory inside a firewalled VM (Apple
// `container`) and open Zed on the host connected to it over SSH.
//
// The Dockerfile and entrypoint.sh are embedded at compile time, so the
// release binary is fully self-contained; changing either file requires a
// `cargo build` for image rebuilds to pick it up.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::VecDeque;
use std::env;
use std::fs;
use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

const IMAGE_REPO: &str = "claude-sandbox";
const DOCKERFILE: &str = include_str!("../Dockerfile");
const ENTRYPOINT: &str = include_str!("../entrypoint.sh");

const USAGE: &str = "\
claude-sandbox — run a project directory inside a claude-sandbox VM and open
Zed on the host connected to it over SSH.

usage:
  claude-sandbox <dir>                 start the VM (build/create as needed), open Zed
  claude-sandbox shell <dir> [cmd...]  same, but ssh in instead of opening Zed
  claude-sandbox stop <dir>            stop the project's VM (it deletes itself on stop)
  claude-sandbox rm <dir>              stop and delete the project's VM
  claude-sandbox --rebuild <dir>       rebuild the image, recreate the VM, open Zed

State:
  ~/.config/claude-sandbox/   dedicated ssh key + managed ssh config
  ~/.claude-sandbox/          mounted as ~/.claude in every VM (Claude login)

A VM deletes itself ~15 seconds after its last ssh session ends (Zed window
closed, shell exited); reopening the project recreates it in seconds. A fresh
VM gets 2 minutes to receive its first connection.

Environment:
  CLAUDE_SANDBOX_IDLE=<seconds>  idle timeout, applied when the VM is created (0 = never reap)
  CLAUDE_SANDBOX_DEBUG=1         keep failed VMs around (skips --rm) so `container logs` works
  CLAUDE_SANDBOX_USER=<name>     account inside the VM (default: your host username)
  CLAUDE_SANDBOX_RESEED=1        overwrite the VM's Claude credentials from the host keychain

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
}

struct Sandbox {
    user: String,
    image: String,
    home: PathBuf,
    state_dir: PathBuf,
    claude_dir: PathBuf,
    key: PathBuf,
    ssh_conf: PathBuf,
    abs: PathBuf,
    name: String,
    target: String,
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
        Some(_) => Cmd::Up,
    };

    let mut rebuild = args.front().is_some_and(|a| a == "--rebuild");
    if rebuild {
        args.pop_front();
    }

    let Some(dir) = args.pop_front() else {
        eprintln!("{USAGE}");
        std::process::exit(1);
    };
    let mut ssh_args: Vec<String> = args.into();

    // Only `shell` forwards trailing arguments (to ssh). Elsewhere they are
    // mistakes — except --rebuild, which is also accepted after the dir.
    if cmd != Cmd::Shell {
        for a in ssh_args.drain(..) {
            if a == "--rebuild" && cmd == Cmd::Up {
                rebuild = true;
            } else {
                bail!("unexpected argument: {a}");
            }
        }
    }

    let sb = Sandbox::new(&dir, cmd)?;
    match cmd {
        Cmd::Stop => sb.stop(),
        Cmd::Rm => sb.remove(),
        Cmd::Up | Cmd::Shell => sb.up(cmd, rebuild, &ssh_args),
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
        fs::copy(src, dest)?;
    }
    Ok(())
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
    fn new(dir: &str, cmd: Cmd) -> Result<Self> {
        let home = PathBuf::from(env::var("HOME").context("HOME is not set")?);
        let user = guest_user()?;
        let state_dir = env::var("CLAUDE_SANDBOX_STATE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".config/claude-sandbox"));

        let abs = if Path::new(dir).is_dir() {
            fs::canonicalize(dir)?
        } else if matches!(cmd, Cmd::Stop | Cmd::Rm) {
            // stop/rm must work after the directory is gone.
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

        Ok(Sandbox {
            key: state_dir.join("id_ed25519"),
            ssh_conf: state_dir.join("ssh_config"),
            claude_dir: home.join(".claude-sandbox"),
            name: format!("claude-sandbox-{base}-{}", &hash[..6]),
            target: format!("/home/{user}/Projects/{base}"),
            // The account is baked into the image, so it belongs in the tag:
            // a different CLAUDE_SANDBOX_USER then builds its own image rather
            // than silently booting one whose only account it cannot log in as.
            image: format!("{IMAGE_REPO}:{user}"),
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

    fn up(&self, cmd: Cmd, rebuild: bool, ssh_args: &[String]) -> Result<()> {
        let domain = preflight()?;
        self.ensure_key()?;
        self.seed_claude_config()?;
        if rebuild {
            self.build_image()?;
            if self.state()?.is_some() {
                self.destroy();
                println!("recreating {} with the new image", self.name);
            }
        }
        self.ensure_container()?;
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
        let url = format!("ssh://{}@{host}{}", self.user, self.target);
        Err(Command::new("zed").arg(url).exec()).context("failed to exec zed")
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
        println!("building {} ...", self.image);

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
            &["build", "-t", self.image.as_str(),
              "--build-arg", &format!("SSH_PUBKEY={pubkeys}"),
              "--build-arg", &format!("USERNAME={}", self.user),
              &ctx.to_string_lossy()],
        );
        let _ = fs::remove_dir_all(&ctx);
        built
    }

    fn have_image(&self) -> Result<bool> {
        Ok(capture("container", &["image", "inspect", self.image.as_str()]).is_ok())
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

    fn ensure_container(&self) -> Result<()> {
        // Only a running container is reused. Anything else is torn down and
        // recreated (~2s): these VMs are ephemeral by design — all state lives
        // in the two mounts — and restarting one churns its address, which the
        // runtime reports stale for a window afterwards.
        let mut state = self.state()?;
        if !matches!(state.as_deref(), Some("running") | None) {
            self.destroy();
            state = None;
        }
        match state.as_deref() {
            Some("running") => Ok(()),
            None => {
                if !self.have_image()? {
                    self.build_image()?;
                }

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

                let mut args: Vec<&str> =
                    vec!["run", "-d", "--name", &self.name, "--cap-add", "CAP_NET_ADMIN"];
                if !debug {
                    args.push("--rm");
                }
                if let Some(e) = idle.as_deref() {
                    args.extend(["-e", e]);
                }
                args.extend(["-v", &mount_project, "-v", &mount_claude, self.image.as_str()]);
                capture("container", &args)?;
                println!("created {} ({} -> {})", self.name, self.abs.display(), self.target);
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
