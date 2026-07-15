#!/usr/bin/env bash
#
# Container entrypoint: materializes /etc/vexatlas/vex-serve.env from the
# process environment (ACI / docker -e flags), generates a stable host key on
# first boot (persisted on the same volume as the repo root so it survives
# container restarts if you mount a dedicated path — see README), then execs
# the dedicated sshd in the foreground as PID 1.

set -euo pipefail

: "${VEX_API_BASE:?VEX_API_BASE not set}"
: "${VEX_INTERNAL_SECRET:?VEX_INTERNAL_SECRET not set}"
: "${VEX_REPO_ROOT:=/var/lib/vexatlas/repos}"
: "${VEX_FAIL_CLOSED:=true}"

umask 077
cat > /etc/vexatlas/vex-serve.env <<EOF
VEX_API_BASE=${VEX_API_BASE}
VEX_INTERNAL_SECRET=${VEX_INTERNAL_SECRET}
VEX_REPO_ROOT=${VEX_REPO_ROOT}
VEX_FAIL_CLOSED=${VEX_FAIL_CLOSED}
VEX_SERVE_SESSION=/usr/local/bin/vex-serve-session
EOF
chown root:vexkeys /etc/vexatlas/vex-serve.env
chmod 0640 /etc/vexatlas/vex-serve.env

mkdir -p "$VEX_REPO_ROOT"
chown -R vex:vex "$VEX_REPO_ROOT"

# Host key: generate once. If /etc/ssh/vexatlas is backed by a persistent
# volume it survives restarts (recommended — clients pin this key); otherwise
# a new one is generated each restart and clients will see a host-key-changed
# warning once.
if [ ! -f /etc/ssh/vexatlas/ssh_host_ed25519_key ]; then
  ssh-keygen -t ed25519 -f /etc/ssh/vexatlas/ssh_host_ed25519_key -N '' -q
fi

exec /usr/sbin/sshd -D -e -f /etc/ssh/sshd_vexatlas.conf
