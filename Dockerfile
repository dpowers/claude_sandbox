FROM ubuntu:24.04

# Account created inside the VM. The launcher passes the host username; this
# default only applies to a hand-run `container build`.
ARG USERNAME=claude

# Base tooling, sshd, and Node.js 22 (required by Claude Code). The NodeSource
# script is downloaded to a file first so a failed download fails the build
# instead of silently leaving the stock Ubuntu Node packages in play.
#
# build-essential pulls in the C/C++ toolchain (gcc, g++, cpp, make, libc6-dev)
# and binutils, which supplies the linker. It is in the base rather than left to
# a per-project overlay because nearly every language runtime that gets used in
# here shells out to cc/ld somewhere — native npm modules via node-gyp, cgo, Rust
# crates with build scripts, Python wheels built from source. pkg-config comes
# along for the same reason: those builds locate system libraries through it.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git sudo openssh-server vim less procps unzip nftables \
        build-essential pkg-config \
    && curl -fsSL https://deb.nodesource.com/setup_22.x -o /tmp/nodesource.sh \
    && bash /tmp/nodesource.sh \
    && rm /tmp/nodesource.sh \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*

# Claude Code CLI (drop the npm download cache in the same layer)
RUN npm install -g @anthropic-ai/claude-code && rm -rf /root/.npm

# Drop the stock 'ubuntu' user so $USERNAME gets UID 1000 (plays nicer with volume mounts)
RUN userdel -r ubuntu || true

# User to ssh in as. No password is ever set: login is by public key only
# (see SSH_PUBKEY below), root login over ssh is off, and root's own password
# is locked — so whether anything in this VM can become root is decided here.
#
# SUDO=0 (the default) leaves the account out of the sudo group and writes no
# sudoers entry. That is what makes the in-guest controls real rather than
# advisory: the egress firewall is enforced by this VM's own kernel and `nft`
# needs root to change it, so an agent that decides to reach the LAN has
# nowhere to go. Same for the read-only overlay mount the entrypoint sets up.
# The `sudo` binary is deliberately left installed — attempting it then fails
# with "is not in the sudoers file" rather than "command not found", which
# says which of the two situations you are in.
#
# SUDO=1 (`claude-sandbox --sudo`) restores passwordless root. It buys
# `apt-get install`, `npm install -g` and anything else that writes outside
# $HOME from inside the VM, and it costs every in-guest control: root holds
# CAP_NET_ADMIN here, so `nft delete table inet egress` drops the firewall in
# one command. Only the host-side controls — the VM boundary, the two mounts,
# and the overlay acceptance gate — still hold.
ARG SUDO=0
RUN useradd -m -u 1000 -s /bin/bash "$USERNAME" \
    && if [ "$SUDO" = 1 ]; then \
         usermod -aG sudo "$USERNAME" \
         && echo "$USERNAME ALL=(ALL) NOPASSWD:ALL" > "/etc/sudoers.d/$USERNAME" \
         && chmod 440 "/etc/sudoers.d/$USERNAME"; \
       fi

# Parent for the launcher's per-project bind mount, which lands at
# /home/$USERNAME/Projects/<project> at run time.
RUN mkdir -p "/home/$USERNAME/Projects" \
    && chown -R "$USERNAME:$USERNAME" "/home/$USERNAME/Projects"

# Claude Code subscription login: consolidate all Claude Code state (OAuth
# credentials, settings) under one directory so the launcher's runtime mount
# of ~/.claude-sandbox persists the login across containers.
# /etc/environment is read by PAM, so SSH login shells see the variable.
RUN mkdir -p "/home/$USERNAME/.claude" \
    && chown "$USERNAME:$USERNAME" "/home/$USERNAME/.claude" \
    && echo "CLAUDE_CONFIG_DIR=/home/$USERNAME/.claude" >> /etc/environment

# sshd setup. Hardening lives in a 00- drop-in: Ubuntu's sshd_config begins
# with `Include /etc/ssh/sshd_config.d/*.conf` and sshd keeps the FIRST value
# it reads for each keyword, so this file wins over the main config and over
# any package-installed drop-in sorted after it.
RUN mkdir -p /run/sshd /etc/ssh/sshd_config.d \
    && printf 'PermitRootLogin no\nPasswordAuthentication no\n' \
        > /etc/ssh/sshd_config.d/00-claude-sandbox.conf

# Public-key auth (required — password login is disabled above and the account
# has no password). Multiple keys may be passed separated by a literal \n
# two-character escape (a raw newline inside a build-arg crashes Apple
# container's builder); printf '%b' expands them.
ARG SSH_PUBKEY=""
RUN mkdir -p "/home/$USERNAME/.ssh" && chmod 700 "/home/$USERNAME/.ssh" \
    && if [ -n "$SSH_PUBKEY" ]; then \
         printf '%b\n' "$SSH_PUBKEY" > "/home/$USERNAME/.ssh/authorized_keys" \
         && chmod 600 "/home/$USERNAME/.ssh/authorized_keys"; \
       fi \
    && chown -R "$USERNAME:$USERNAME" "/home/$USERNAME/.ssh"

# Egress firewall: rejects new outbound connections to the local network
# (RFC1918, link-local, CGNAT, multicast) while allowing the Internet, DNS,
# and replies to inbound SSH. Applied at startup by the entrypoint, which
# also runs the idle watchdog that stops the VM when its last ssh session
# ends. Needs NET_ADMIN — Apple's `container` grants it (VM-per-container);
# with Docker add: --cap-add=NET_ADMIN
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod 755 /usr/local/bin/entrypoint.sh

EXPOSE 22
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]

# A digest of this file and entrypoint.sh, passed in by the launcher, recorded
# so that the image can be asked what it was built from. The base image keeps
# its tag across rebuilds, so the tag cannot answer that and a stale image is
# otherwise indistinguishable from a current one; the launcher reads this label
# back and refuses to start a VM from an image whose sources have moved on.
#
# Last in the file, and after ENTRYPOINT, deliberately: a new digest then
# invalidates nothing but this trailing empty layer, so restamping an image
# whose real instructions have not changed is a `rebuild --use-cache` away.
ARG SOURCE_DIGEST=unknown
LABEL claude-sandbox.source=$SOURCE_DIGEST
