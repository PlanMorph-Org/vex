#!/usr/bin/env bash
# Fetch the real-world IFC fidelity corpus into crates/vex-core/tests/fixtures/corpus/.
#
# Sources are pinned to an exact commit of buildingSMART/Sample-Test-Files and
# verified by SHA-256, so the corpus is reproducible and tamper-evident. The
# corpus directory is gitignored; fidelity tests skip gracefully when it is
# absent (e.g. offline machines) and run the full battery when present.
#
# Usage: tools/fetch-fixtures.sh
set -euo pipefail

REPO_PIN="cecf656112a54a0d8cdd8b06b9398bfea5163886" # buildingSMART/Sample-Test-Files @ main, 2024
BASE="https://raw.githubusercontent.com/buildingSMART/Sample-Test-Files/${REPO_PIN}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${ROOT}/crates/vex-core/tests/fixtures/corpus"

# name|repo path (unencoded)|sha256
MANIFEST=$(cat <<'EOF'
wall-with-opening-and-window.ifc|IFC 4.0.2.1 (IFC 4)/ISO Spec - ReferenceView_V1.2/wall-with-opening-and-window.ifc|73b0e45d931d5dc13bfee5fdc7bd80f796526445458b2de74c4168d209097832
basin-tessellation.ifc|IFC 4.0.2.1 (IFC 4)/ISO Spec - ReferenceView_V1.2/basin-tessellation.ifc|7278769ec5ef35d388819d2f197519061c7f0f68dd43a733f333ddeb74578121
column-straight-rectangle-tessellation.ifc|IFC 4.0.2.1 (IFC 4)/ISO Spec - ReferenceView_V1.2/column-straight-rectangle-tessellation.ifc|58bb9b2cae96edf2c368de95f526650f3f282e4e7fc37c0065c501bdbeed00a1
tessellated-item.ifc|IFC 4.0.2.1 (IFC 4)/ISO Spec - ReferenceView_V1.2/tessellated-item.ifc|758974f7d558f11b8d8121816954179def32594ed7eafcd1faae70ee0a8e5946
Building-Architecture.ifc|IFC 4.0.2.1 (IFC 4)/PCERT-Sample-Scene/Building-Architecture.ifc|3ff9b10bd00c7b96dded51e7ca5a6b69efbea38b049adcdd05fcd247de7e70d5
Building-Structural.ifc|IFC 4.0.2.1 (IFC 4)/PCERT-Sample-Scene/Building-Structural.ifc|68be722391e7aaa53bb9278645a02aa4b6382f13cc07548a1612e9b1dc3def67
EOF
)

mkdir -p "${DEST}"
fail=0
while IFS='|' read -r name path sha; do
    [ -z "${name}" ] && continue
    out="${DEST}/${name}"
    if [ -f "${out}" ] && echo "${sha}  ${out}" | sha256sum --check --status 2>/dev/null; then
        echo "ok       ${name} (cached)"
        continue
    fi
    url="${BASE}/$(printf '%s' "${path}" | sed 's/ /%20/g')"
    echo "fetching ${name}"
    if ! curl -fsSL --max-time 300 -o "${out}.tmp" "${url}"; then
        echo "error    ${name}: download failed" >&2
        rm -f "${out}.tmp"
        fail=1
        continue
    fi
    if ! echo "${sha}  ${out}.tmp" | sha256sum --check --status; then
        echo "error    ${name}: checksum mismatch — refusing to install" >&2
        rm -f "${out}.tmp"
        fail=1
        continue
    fi
    mv "${out}.tmp" "${out}"
    echo "ok       ${name}"
done <<< "${MANIFEST}"

if [ "${fail}" -ne 0 ]; then
    echo "corpus incomplete — see errors above" >&2
    exit 1
fi
echo "corpus ready: ${DEST}"
