#!/usr/bin/env bash
# End-to-end semantic smoke test for the vex CLI.
# Validates the three headline classifications through the real binary:
#   1. a placement move surfaces as kind=moved with a placement hint
#   2. a mesh edit surfaces with layer=shape and a geometry hint
#   3. internal helper churn is folded into counts.internal
set -euo pipefail

VEX="${VEX:-$HOME/projects/vex/target/debug/vex}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

base() {
    local loc="$1" apex="$2"
    cat <<EOF
ISO-10303-21;
HEADER;
FILE_DESCRIPTION((''),'2;1');
FILE_NAME('','',(''),(''),'','','');
FILE_SCHEMA(('IFC4'));
ENDSEC;
DATA;
#1 = IFCPROJECT('0000000000000000000001',\$,'P',\$,\$,\$,\$,(#2),#3);
#2 = IFCGEOMETRICREPRESENTATIONCONTEXT(\$,'Model',3,1.0E-5,#4,\$);
#3 = IFCUNITASSIGNMENT((#5));
#4 = IFCAXIS2PLACEMENT3D(#6,\$,\$);
#5 = IFCSIUNIT(*,.LENGTHUNIT.,\$,.METRE.);
#6 = IFCCARTESIANPOINT((0.,0.,0.));
#10 = IFCCARTESIANPOINT((${loc}));
#11 = IFCAXIS2PLACEMENT3D(#10,\$,\$);
#12 = IFCLOCALPLACEMENT(\$,#11);
#13 = IFCRECTANGLEPROFILEDEF(.AREA.,\$,\$,4.0,0.3);
#14 = IFCDIRECTION((0.,0.,1.));
#15 = IFCEXTRUDEDAREASOLID(#13,\$,#14,3.0);
#16 = IFCSHAPEREPRESENTATION(#2,'Body','SweptSolid',(#15));
#17 = IFCPRODUCTDEFINITIONSHAPE(\$,\$,(#16));
#18 = IFCWALL('2O2Fr\$t4X7Zf8NOew3FNr2',\$,'Wall-A',\$,\$,#12,#17,\$,.STANDARD.);
#20 = IFCCARTESIANPOINTLIST3D(((0.,0.,0.),(1.,0.,0.),(0.,1.,0.),(${apex})));
#21 = IFCTRIANGULATEDFACESET(#20,\$,.T.,((1,2,3),(1,2,4),(1,3,4),(2,3,4)),\$);
#22 = IFCSHAPEREPRESENTATION(#2,'Body','Tessellation',(#21));
#23 = IFCPRODUCTDEFINITIONSHAPE(\$,\$,(#22));
#24 = IFCLOCALPLACEMENT(\$,#4);
#25 = IFCBUILDINGELEMENTPROXY('3O2Fr\$t4X7Zf8NOew3FNr3',\$,'Mesh-A',\$,\$,#24,#23,\$,\$);
ENDSEC;
END-ISO-10303-21;
EOF
}

base "0.,0.,0." "0.,0.,1." > "$WORK/v1.ifc"
base "2.5,0.,0." "0.,0.,1." > "$WORK/v2-moved.ifc"
base "0.,0.,0." "0.,0.,1.5" > "$WORK/v2-meshedit.ifc"

run_compare() {
    local variant="$1" repo="$WORK/repo-$1"
    "$VEX" --repo "$repo" init >/dev/null
    "$VEX" --repo "$repo" import "$WORK/v1.ifc" >/dev/null
    "$VEX" --repo "$repo" commit -m v1 >/dev/null
    "$VEX" --repo "$repo" import "$WORK/v2-$variant.ifc" >/dev/null
    "$VEX" --repo "$repo" commit -m v2 >/dev/null
    "$VEX" --repo "$repo" --json changes
}

echo "── moved variant ──"
run_compare moved > "$WORK/moved.json"
python3 - "$WORK/moved.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
wall = next(e for e in d["elements"] if e["type_name"] == "IFCWALL")
assert wall["kind"] == "moved", f"expected moved, got {wall}"
assert "Placement:" in (wall.get("hint") or ""), f"missing placement hint: {wall}"
assert d["counts"]["moved"] == 1, d["counts"]
print("ok  kind=moved with hint:", wall["hint"])
print("ok  counts:", d["counts"])
PY

echo "── mesh-edit variant ──"
run_compare meshedit > "$WORK/meshedit.json"
python3 - "$WORK/meshedit.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
proxy = next(e for e in d["elements"] if e["type_name"] == "IFCBUILDINGELEMENTPROXY")
assert proxy["layer"] == "shape", f"expected shape layer, got {proxy}"
assert proxy["kind"] == "modified", proxy
assert "Geometry changed" in (proxy.get("hint") or ""), proxy
assert d["counts"]["internal"] >= 1, f"mesh carrier churn should fold into internal: {d['counts']}"
print("ok  layer=shape with hint:", proxy["hint"])
print("ok  counts:", d["counts"])
PY

echo "smoke: all assertions passed"
