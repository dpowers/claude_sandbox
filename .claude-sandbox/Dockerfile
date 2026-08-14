# Image overlay for this project: a Rust toolchain.
#
# claude-sandbox is itself a Cargo crate, so a sandbox for it can't build,
# test, or lint anything without one. Layered on top of the base image (and the
# global overlay, if there is one) — no FROM line, the launcher supplies it.
#
# rustup rather than Ubuntu's rustc/cargo packages: 24.04 pins 1.75, and a
# toolchain that can't be moved off the distro's version is the wrong shape for
# a disposable dev VM — `rustup update`, `+nightly`, and extra targets all stop
# working. The C toolchain that linking and build scripts need is already in
# the base image (build-essential + pkg-config).
#
# Installed as $USERNAME, into that account's ~/.rustup and ~/.cargo, rather
# than system-wide: the sandbox account has no sudo by default, and CARGO_HOME
# is written to at *run* time — the registry cache, git checkouts of
# dependencies, and `cargo install` all land there — so a root-owned one would
# be read-only exactly when cargo needs it.
#
# The init script is downloaded to a file before being run rather than piped
# into sh, so a failed download fails the build instead of half-running; same
# reason the base image does it for the NodeSource script. It runs as root with
# its three home-directory variables pointed at the account and the result
# chowned in the same layer, rather than under `su`, which would pull PAM into
# the build to no benefit; the chown costs nothing here because it rewrites the
# layer that created those files rather than adding one on top.
RUN curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o /tmp/rustup-init.sh \
    && env HOME="/home/$USERNAME" \
           RUSTUP_HOME="/home/$USERNAME/.rustup" \
           CARGO_HOME="/home/$USERNAME/.cargo" \
        sh /tmp/rustup-init.sh -y --no-modify-path --profile minimal \
           -c rustfmt -c clippy -c rust-analyzer \
    && rm /tmp/rustup-init.sh \
    && chown -R "$USERNAME:$USERNAME" \
        "/home/$USERNAME/.rustup" "/home/$USERNAME/.cargo"

# PATH, in two places, because they cover different kinds of session and
# neither covers both:
#
#   /etc/profile.d/   login shells — an interactive `ssh <vm>`, and Zed's
#                     terminal. It cannot be left to /etc/environment alone:
#                     Debian's /etc/profile *assigns* PATH rather than
#                     appending to it, so whatever pam_env exported is gone by
#                     the time the prompt appears.
#   /etc/environment  everything else — `ssh <vm> cargo build`, and any
#                     non-login shell. Read by pam_env for the session itself,
#                     which is how the base image gets CLAUDE_CONFIG_DIR in.
#                     Last assignment wins, so this line beats the stock one.
#
# --no-modify-path above keeps rustup from also editing the account's .bashrc
# and .profile, so PATH is set in these two files and nowhere else.
RUN printf 'export PATH="$HOME/.cargo/bin:$PATH"\n' > /etc/profile.d/rust-cargo.sh \
    && chmod 644 /etc/profile.d/rust-cargo.sh \
    && printf 'PATH="/home/%s/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"\n' \
        "$USERNAME" >> /etc/environment
