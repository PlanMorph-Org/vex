#!/usr/bin/env bash
#
# sshd AuthorizedKeysCommand for Vex Atlas (container variant of
# architur/deploy/vex-serve/authorized-keys-command.sh — keep both in sync).
#
# sshd invokes this on every incoming SSH connection to resolve the offered
# public key to an authorized_keys line, via architur's HMAC-signed callback
#   GET /api/internal/vex/authorized-keys?fingerprint={fingerprint}
# On a hit it emits a single forced-command authorized_keys line that runs
# vex-serve for the resolved user. Exit non-zero or print nothing => sshd
# rejects the key.

set -euo pipefail

ENV_FILE="${VEX_SERVE_ENV_FILE:-/etc/vexatlas/vex-serve.env}"
# shellcheck disable=SC1090
[ -r "$ENV_FILE" ] && . "$ENV_FILE"

: "${VEX_API_BASE:?VEX_API_BASE not set}"
: "${VEX_INTERNAL_SECRET:?VEX_INTERNAL_SECRET not set}"
: "${VEX_REPO_ROOT:?VEX_REPO_ROOT not set}"
VEX_SERVE_SESSION="${VEX_SERVE_SESSION:-/usr/local/bin/vex-serve-session}"

fingerprint="${1:-}"
[ -n "$fingerprint" ] || { echo "no fingerprint" >&2; exit 1; }

# architur stores fingerprints as "SHA256:" + base64(...) with '=' trimmed;
# only '+' and '/' need escaping for the query string.
encoded="${fingerprint//+/%2B}"
encoded="${encoded//\//%2F}"

ts="$(date +%s)"
sig="$(printf '%s.' "$ts" \
  | openssl dgst -sha256 -hmac "$VEX_INTERNAL_SECRET" -hex \
  | sed 's/^.*= //')"

resp="$(curl -fsS \
  --max-time 5 \
  -H "X-Vex-Signature: t=${ts},v1=${sig}" \
  "${VEX_API_BASE%/}/api/internal/vex/authorized-keys?fingerprint=${encoded}" \
  2>/dev/null)" || exit 0

user_id="$(printf '%s' "$resp"   | jq -r '.userId // empty')"
key_type="$(printf '%s' "$resp"  | jq -r '.keyType // empty')"
public_key="$(printf '%s' "$resp" | jq -r '.publicKey // empty')"

[ -n "$user_id" ] && [ -n "$key_type" ] && [ -n "$public_key" ] || exit 0

opts='restrict,no-agent-forwarding,no-port-forwarding,no-X11-forwarding,no-pty'
cmd="${VEX_SERVE_SESSION} --repo-root ${VEX_REPO_ROOT} --user ${user_id}"
printf 'command="%s",%s %s %s\n' "$cmd" "$opts" "$key_type" "$public_key"
