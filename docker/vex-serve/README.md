# vex-serve container image

Packages the `vex-serve` binary together with a dedicated, hardened `sshd`
instance and the `AuthorizedKeysCommand` integration that resolves incoming
SSH keys against Vex Atlas (architur). This closes the gap called out in
`architur/docs/deploy-vex-serve.md`: previously the `vex` repo shipped no
Dockerfile, so `vex-serve` could only be deployed by hand-provisioning a VM.

## What's in here

| File | Role |
| --- | --- |
| `Dockerfile` | Multi-stage build: compiles `vex` + `vex-serve` (release), then a slim Debian runtime with `openssh-server`. |
| `entrypoint.sh` | PID 1. Materializes `/etc/vexatlas/vex-serve.env` from container env vars, generates the host key on first boot, execs `sshd -D`. |
| `sshd_vexatlas.conf` | Key-only, forced-command, no-shell sshd config — mirrors `architur/deploy/vex-serve/sshd_vexatlas.conf`. |
| `authorized-keys-command.sh` | Resolves a key fingerprint via architur's HMAC-signed `/api/internal/vex/authorized-keys` callback. |
| `vex-serve-session.sh` | Forced-command wrapper that execs `vex-serve` for the resolved user. |

Keep these in sync with their `architur/deploy/vex-serve/*` counterparts —
the two are duplicated (not symlinked) so each repo can be built standalone,
but they must stay behaviorally identical.

## Build

```sh
docker build -f docker/vex-serve/Dockerfile -t vex-serve:latest .
```

## Run

Requires:

- `VEX_API_BASE` — reachable URL of the Vex Atlas API.
- `VEX_INTERNAL_SECRET` — must equal the API's `Vex:InternalSecret`.
- A volume mounted at `/var/lib/vexatlas/repos` **shared with the API**
  (same filesystem the API's `Vex:RepoRoot` points at — the vex CAS is
  on-disk, not blob-backed; see `architur/docs/architecture.md`). On Azure
  this is an Azure Files share mounted by both this container and the API.
- Port 22 published/exposed for `vex push`/`vex fetch` traffic.
- (Recommended) a persistent volume at `/etc/ssh/vexatlas` so the host key
  survives restarts — otherwise clients see a one-time host-key-changed
  warning after every container recreate.

```sh
docker run -d \
  -p 22:22 \
  -e VEX_API_BASE=https://studio.planmorph.software \
  -e VEX_INTERNAL_SECRET=*** \
  -v vexatlas-repos:/var/lib/vexatlas/repos \
  -v vexatlas-hostkey:/etc/ssh/vexatlas \
  vex-serve:latest
```

See `architur/docs/MIGRATION.md` for the Azure Container Instances deployment
(Azure Files share mounted into both this container and the API's Container
App at the same path).
