#!/bin/sh
# Egress policy: allow the broad Internet, reject the local network — then run
# the idle watchdog and hand off to sshd.
#
# How this VM's networking is set up (Apple `container`, verified 2026-08-14):
#   * Address, routes and /etc/resolv.conf are written statically by the
#     runtime's init agent (ghcr.io/apple/containerization/vminit) before this
#     script runs; the host reserves the address itself. No DHCP client exists
#     in the guest, so there is nothing to exempt on ports 67/68.
#   * The default gateway and the DNS resolver are the same host address on
#     the container bridge (192.168.64.1), which falls inside the private
#     ranges rejected below — hence the explicit port 53 exemption. Internet
#     traffic is unaffected: nftables matches the destination address, not the
#     next hop, so packets merely routed via the gateway still pass.
#   * IPv6 is on-link only (a ULA /64 plus link-local, no default route), so
#     every IPv6 destination is local by definition and is rejected wholesale.
#
# Known trade-off: port 53 to the host resolver is the one deliberate hole in
# "no local network", and it doubles as a DNS-tunnelling channel. Closing it
# means pointing the guest at a public resolver instead (`container run
# --dns 1.1.1.1 ...`), after which the exemption below can go.
#
# Requires NET_ADMIN. Apple's `container` runs each container in its own VM
# where root keeps that capability; with Docker, run with --cap-add=NET_ADMIN.
# Fails closed: if the rules can't be applied the container exits instead of
# starting sshd unfiltered.
set -eu

# Exempt port 53 to whatever resolvers the runtime wrote into resolv.conf
# (normally just the host bridge address; `container run --dns` can override).
# %zone suffixes are stripped and tokens are restricted to address characters,
# so a malformed resolv.conf line can neither inject nft syntax nor abort the
# fail-closed boot.
dns_v4="" dns_v6=""
for ns in $(awk '/^nameserver/ { print $2 }' /etc/resolv.conf 2>/dev/null); do
    ns=${ns%%\%*}
    case $ns in '' | *[!0-9a-fA-F:.]*) continue ;; esac
    case $ns in
        *:*) dns_v6="$dns_v6
        ip6 daddr $ns udp dport 53 accept
        ip6 daddr $ns tcp dport 53 accept" ;;
        *)   dns_v4="$dns_v4
        ip daddr $ns udp dport 53 accept
        ip daddr $ns tcp dport 53 accept" ;;
    esac
done

if nft list table inet egress >/dev/null 2>&1; then
    nft delete table inet egress
fi

nft -f - <<EOF
table inet egress {
    chain output {
        type filter hook output priority filter; policy accept;

        oifname "lo" accept

        # Replies to inbound connections (SSH from the host) and anything
        # conntrack already knows about.
        ct state established,related accept

        # Neighbour discovery, multicast-listener reports and path-MTU
        # messages keep the link itself working, so they are allowed ahead of
        # the blanket IPv6 reject. Everything else in ICMPv6 is not.
        icmpv6 type { nd-router-solicit, nd-router-advert, nd-neighbor-solicit, nd-neighbor-advert, mld-listener-query, mld-listener-report, mld-listener-reduction, mld2-listener-report, packet-too-big } accept
${dns_v4}${dns_v6}

        # IPv4: new connections to private / CGNAT / link-local / multicast /
        # reserved space are refused; public destinations fall through to the
        # accept policy.
        ip daddr { 10.0.0.0/8, 100.64.0.0/10, 169.254.0.0/16, 172.16.0.0/12, 192.168.0.0/16, 224.0.0.0/4, 240.0.0.0/4 } counter reject

        # IPv6: no route leaves this link, so anything still here is local —
        # reject the whole family rather than enumerate ranges. Inbound ssh
        # over IPv6 is unaffected (its replies match ct established above),
        # and this covers a global-unicast LAN if one ever appears.
        meta nfproto ipv6 counter reject
    }
}
EOF

# Idle watchdog: once the last ssh session (Zed remote or shell) has been
# gone for IDLE_TIMEOUT seconds, kill PID 1 (sshd) so the container stops —
# and, because the launcher creates containers with --rm, is deleted. A fresh
# VM gets BOOT_GRACE seconds to receive its first connection, and a session
# only counts as "seen" after two consecutive samples (~5s apart), so the
# launcher's momentary TCP readiness probe cannot collapse the boot grace
# down to the short idle timeout. Set IDLE_TIMEOUT=0 to disable. Non-numeric
# values fall back to the defaults instead of silently disabling the reaper.
IDLE_TIMEOUT=${IDLE_TIMEOUT:-15}
BOOT_GRACE=${BOOT_GRACE:-120}
case $IDLE_TIMEOUT in '' | *[!0-9]*) IDLE_TIMEOUT=15 ;; esac
case $BOOT_GRACE in '' | *[!0-9]*) BOOT_GRACE=120 ;; esac
if [ "$IDLE_TIMEOUT" -gt 0 ]; then
    (
        tcpfiles=
        for f in /proc/net/tcp /proc/net/tcp6; do
            if [ -r "$f" ]; then tcpfiles="$tcpfiles $f"; fi
        done
        seen=0 prev=0 idle=0
        while sleep 5; do
            established=0
            if [ -n "$tcpfiles" ]; then
                # FNR (not NR) so every file's header row is skipped, and
                # `|| echo 0` so an awk failure can't kill this subshell
                # via the inherited set -e.
                established=$(awk 'FNR > 1 { split($2, a, ":");
                    if (a[2] == "0016" && $4 == "01") n++ } END { print n + 0 }' \
                    $tcpfiles 2>/dev/null || echo 0)
            fi
            if [ "$established" -gt 0 ]; then
                if [ "$prev" = 1 ]; then seen=1; fi
                prev=1
                idle=0
            else
                prev=0
                idle=$((idle + 5))
                if [ "$seen" = 1 ]; then limit=$IDLE_TIMEOUT; else limit=$BOOT_GRACE; fi
                if [ "$idle" -ge "$limit" ]; then
                    echo "no ssh sessions for ${idle}s - stopping" >&2
                    kill -TERM 1
                    exit 0
                fi
            fi
        done
    ) &
fi

exec /usr/sbin/sshd -D -e
