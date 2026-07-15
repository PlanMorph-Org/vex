# syntax=docker/dockerfile:1.7
#
# vex-serve container image — closes the gap noted in architur's
# docs/deploy-vex-serve.md ("the vex repo ships no Dockerfile"). Bundles the
# vex-serve binary with a dedicated sshd instance and the AuthorizedKeysCommand
# integration scripts, so the whole "vexatlas-sshd + vex-serve" host described
# in that runbook can run as a single container (Azure Container Instances,
# or any other container runtime) instead of a hand-provisioned VM.
#
# Build from the repo root:
#   docker build -f docker/vex-serve/Dockerfile -t vex-serve:latest .
#
# Run (Azure Container Instances / docker):
#   -p 22:22
#   -v <azure-files-share>:/var/lib/vexatlas/repos   (the shared vex CAS)
#   -e VEX_API_BASE=https://studio.planmorph.software
#   -e VEX_INTERNAL_SECRET=<same as api Vex:InternalSecret>
#   -e VEX_REPO_ROOT=/var/lib/vexatlas/repos
#   -e VEX_FAIL_CLOSED=true

FROM rust:1.82-slim-bookworm AS build
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --release -p vex-cli -p vex-serve

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        openssh-server curl ca-certificates jq openssl gosu \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /run/sshd /etc/ssh/vexatlas /etc/vexatlas /var/lib/vexatlas/repos \
    && useradd --system --create-home --shell /usr/sbin/nologin vex \
    && useradd --system --shell /usr/sbin/nologin vexkeys

COPY --from=build /src/target/release/vex        /usr/local/bin/vex
COPY --from=build /src/target/release/vex-serve  /usr/local/bin/vex-serve
COPY docker/vex-serve/authorized-keys-command.sh /usr/local/bin/vex-authorized-keys
COPY docker/vex-serve/vex-serve-session.sh       /usr/local/bin/vex-serve-session
COPY docker/vex-serve/sshd_vexatlas.conf         /etc/ssh/sshd_vexatlas.conf
COPY docker/vex-serve/entrypoint.sh              /entrypoint.sh
RUN chmod 0755 /usr/local/bin/vex-authorized-keys /usr/local/bin/vex-serve-session /entrypoint.sh \
    && chown root:root /usr/local/bin/vex-authorized-keys \
    && chown -R vex:vex /var/lib/vexatlas/repos

EXPOSE 22
ENTRYPOINT ["/entrypoint.sh"]
