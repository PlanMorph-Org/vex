// One-off generator for the `vex spatial` test fixtures. Emits valid IFC4
// STEP files with a full spatial hierarchy so GlobalIds are guaranteed to be
// exactly 22 chars (the IFC identity length the graph builder recognizes).
const fs = require("fs");
const path = require("path");

// Deterministic 22-char GlobalId from a short label (digits/letters are all
// valid IFC base64 chars).
const gid = (label) => label.padEnd(22, "0");

const HEADER = [
  "ISO-10303-21;",
  "HEADER;",
  "FILE_DESCRIPTION(('ViewDefinition [CoordinationView_V2.0]'),'2;1');",
  "FILE_NAME('spatial.ifc','2024-01-01T00:00:00',('Vex'),('Vex'),'vex-test','vex-test','');",
  "FILE_SCHEMA(('IFC4'));",
  "ENDSEC;",
  "DATA;",
].join("\n");

const FOOTER = ["ENDSEC;", "END-ISO-10303-21;", ""].join("\n");

function write(name, lines) {
  const body = lines.map((l) => l + "\n").join("");
  const text = HEADER + "\n" + body + FOOTER;
  const out = path.join(__dirname, "..", "crates", "vex-cli", "tests", "fixtures", name);
  fs.writeFileSync(out, text);
  console.log("wrote", out, "(" + text.length + " bytes)");
}

// ── Fixture A: full nested hierarchy + assigned + unassigned + ambiguous ──
write("spatial-hierarchy.min.ifc", [
  `#1 = IFCPROJECT('${gid("PROJ")}',$,'MainProject',$,$,$,$,$,$);`,
  `#2 = IFCSITE('${gid("SITE")}',$,'MainSite',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);`,
  `#3 = IFCBUILDING('${gid("BLDG")}',$,'MainBuilding',$,$,$,$,$,.ELEMENT.,$,$,$);`,
  `#4 = IFCBUILDINGSTOREY('${gid("LVL1")}',$,'Level 1',$,$,$,$,$,.ELEMENT.,0.);`,
  `#5 = IFCBUILDINGSTOREY('${gid("LVL2")}',$,'Level 2',$,$,$,$,$,.ELEMENT.,3.);`,
  `#6 = IFCWALL('${gid("WALL1")}',$,'Wall-1',$,$,$,$,$,.STANDARD.);`,
  `#7 = IFCSLAB('${gid("SLAB1")}',$,'Slab-1',$,$,$,$,$,.FLOOR.);`,
  `#8 = IFCCOLUMN('${gid("COL2")}',$,'Column-2',$,$,$,$,$,.COLUMN.);`,
  `#9 = IFCWALL('${gid("WALLU")}',$,'Wall-Unassigned',$,$,$,$,$,.STANDARD.);`,
  `#10 = IFCBUILDINGELEMENTPROXY('${gid("PROXA")}',$,'Proxy-Ambiguous',$,$,$,$,$,$);`,
  `#11 = IFCRELAGGREGATES('${gid("RELA1")}',$,$,$,#1,(#2));`,
  `#12 = IFCRELAGGREGATES('${gid("RELA2")}',$,$,$,#2,(#3));`,
  `#13 = IFCRELAGGREGATES('${gid("RELA3")}',$,$,$,#3,(#4,#5));`,
  `#14 = IFCRELCONTAINEDINSPATIALSTRUCTURE('${gid("RELC1")}',$,$,$,(#6,#7,#10),#4);`,
  `#15 = IFCRELCONTAINEDINSPATIALSTRUCTURE('${gid("RELC2")}',$,$,$,(#8,#10),#5);`,
]);

// ── Fixture B: malformed relationships (null endpoints) for resilience ──
// Contains rel with a null RelatingStructure and an aggregation with a null
// RelatingObject. Neither should panic; the wall must fall out as unassigned.
write("spatial-malformed.min.ifc", [
  `#1 = IFCPROJECT('${gid("PROJ")}',$,'MalformedProject',$,$,$,$,$,$);`,
  `#2 = IFCSITE('${gid("SITE")}',$,'Site',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);`,
  `#3 = IFCBUILDING('${gid("BLDG")}',$,'Building',$,$,$,$,$,.ELEMENT.,$,$,$);`,
  `#4 = IFCBUILDINGSTOREY('${gid("LVL1")}',$,'Level 1',$,$,$,$,$,.ELEMENT.,0.);`,
  `#5 = IFCWALL('${gid("WALL1")}',$,'Wall-Orphan',$,$,$,$,$,.STANDARD.);`,
  `#6 = IFCRELAGGREGATES('${gid("RELA1")}',$,$,$,#1,(#2));`,
  `#7 = IFCRELAGGREGATES('${gid("RELA2")}',$,$,$,#2,(#3));`,
  // Missing RelatingObject (parent) — storey ends up parentless, must not panic.
  `#8 = IFCRELAGGREGATES('${gid("RELA3")}',$,$,$,$,(#4));`,
  // Missing RelatingStructure — element cannot be assigned, stays unassigned.
  `#9 = IFCRELCONTAINEDINSPATIALSTRUCTURE('${gid("RELC1")}',$,$,$,(#5),$);`,
]);
