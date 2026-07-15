#!/usr/bin/env bash
#
# vex-serve session wrapper (container variant of
# architur/deploy/vex-serve/vex-serve-session.sh — keep both in sync).
#
# Named in the forced `command=` of each authorized_keys line. sshd runs
# forced commands with a minimal environment, so this wrapper sources the
# env file materialized by /entrypoint.sh and execs the real vex-serve
# binary against the SSH session's stdin/stdout. All arguments are forwarded
# verbatim (e.g. `--repo-root <dir> --user <id>`).

set -euo pipefail

ENV_FILE="${VEX_SERVE_ENV_FILE:-/etc/vexatlas/vex-serve.env}"
# shellcheck disable=SC1090
[ -r "$ENV_FILE" ] && . "$ENV_FILE"

export VEX_API_BASE VEX_INTERNAL_SECRET VEX_REPO_ROOT
export VEX_FAIL_CLOSED="${VEX_FAIL_CLOSED:-true}"

exec /usr/local/bin/vex-serve "$@"
