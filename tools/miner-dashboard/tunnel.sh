#!/bin/sh
# Keep a local tunnel to the miner's Mujina API open.
#
# The miner's firewall is INPUT policy DROP with an explicit allowlist
# that does not include 7785, so the API is not reachable across the
# LAN even though it binds 0.0.0.0. Tunnelling avoids opening an
# unauthenticated API to the network.
#
# Local 127.0.0.1:7785 -> miner 127.0.0.1:7785
#
# Usage: ./tunnel.sh root@<miner-ip>
#        MINER_SSH=root@<miner-ip> ./tunnel.sh
set -u

MINER_SSH=${1:-${MINER_SSH:-}}
if [ -z "$MINER_SSH" ]; then
    echo "usage: $0 <ssh-destination>   (e.g. $0 root@10.0.0.5)" >&2
    echo "   or: MINER_SSH=root@10.0.0.5 $0" >&2
    exit 1
fi

PORT=${MINER_API_PORT:-7785}

while true; do
    ssh -N \
        -o ExitOnForwardFailure=yes \
        -o ServerAliveInterval=15 \
        -o ServerAliveCountMax=3 \
        -o ConnectTimeout=10 \
        -L "127.0.0.1:$PORT:127.0.0.1:$PORT" \
        "$MINER_SSH"
    echo "$(date -Is) tunnel dropped, retrying in 5s" >&2
    sleep 5
done
