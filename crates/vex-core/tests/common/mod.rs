//! Deterministic synthetic IFC generator for fidelity tests.
//!
//! Produces a valid IFC4 STEP model with a configurable number of storeys,
//! extruded walls (rectangle profile + IfcExtrudedAreaSolid) and tessellated
//! meshes (IfcTriangulatedFaceSet). Generation is fully deterministic so the
//! same `ModelSpec` + mutations always yield byte-identical files.
#![allow(dead_code)]
#![allow(unreachable_pub)]

use std::fmt::Write as _;

/// Shape of the synthetic model.
#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    pub storeys: usize,
    pub walls_per_storey: usize,
    pub meshes_per_storey: usize,
}

impl Default for ModelSpec {
    fn default() -> Self {
        Self {
            storeys: 2,
            walls_per_storey: 3,
            meshes_per_storey: 1,
        }
    }
}

/// Deterministic edits applied on top of the base model — used to simulate
/// what a CAD user (or a noisy exporter) does between two versions.
#[derive(Debug, Clone, Copy)]
pub enum Mutation {
    /// Rename one wall (`Wall-s-w` → `Wall-s-w-renamed`).
    RenameWall { storey: usize, wall: usize },
    /// Translate one wall's local placement along X.
    MoveWall { storey: usize, wall: usize, dx: f64 },
    /// Perturb one mesh vertex (a real geometry edit).
    EditMesh { storey: usize, mesh: usize },
    /// Re-export every mesh with its vertex array reversed (indices fixed
    /// up accordingly). Semantically a no-op; hashing must agree.
    PermuteMeshVertices,
}

struct Emitter {
    out: String,
    next_id: u64,
    next_gid: u64,
}

impl Emitter {
    fn new() -> Self {
        Self {
            out: String::new(),
            next_id: 1,
            next_gid: 1,
        }
    }

    /// Emit one entity, returning its STEP id.
    fn emit(&mut self, body: &str) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let _ = writeln!(self.out, "#{id} = {body};");
        id
    }

    /// Deterministic 22-char GlobalId (digits are valid IFC base64 chars).
    fn gid(&mut self) -> String {
        let g = self.next_gid;
        self.next_gid += 1;
        format!("{g:022}")
    }
}

fn real(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{v:.1}")
    } else {
        format!("{v}")
    }
}

fn point3(e: &mut Emitter, x: f64, y: f64, z: f64) -> u64 {
    e.emit(&format!(
        "IFCCARTESIANPOINT(({},{},{}))",
        real(x),
        real(y),
        real(z)
    ))
}

fn axis_placement(e: &mut Emitter, x: f64, y: f64, z: f64) -> u64 {
    let p = point3(e, x, y, z);
    e.emit(&format!("IFCAXIS2PLACEMENT3D(#{p},$,$)"))
}

fn local_placement(e: &mut Emitter, x: f64, y: f64, z: f64) -> u64 {
    let ax = axis_placement(e, x, y, z);
    e.emit(&format!("IFCLOCALPLACEMENT($,#{ax})"))
}

/// Build the STEP file for `spec` with `mutations` applied.
pub fn generate(spec: ModelSpec, mutations: &[Mutation]) -> String {
    let mut e = Emitter::new();
    let permute = mutations
        .iter()
        .any(|m| matches!(m, Mutation::PermuteMeshVertices));

    // ── Project scaffolding ────────────────────────────────────────────
    let proj_gid = e.gid();
    let origin = point3(&mut e, 0.0, 0.0, 0.0);
    let wcs = e.emit(&format!("IFCAXIS2PLACEMENT3D(#{origin},$,$)"));
    let ctx = e.emit(&format!(
        "IFCGEOMETRICREPRESENTATIONCONTEXT($,'Model',3,1.0E-5,#{wcs},$)"
    ));
    let u1 = e.emit("IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.)");
    let u2 = e.emit("IFCSIUNIT(*,.PLANEANGLEUNIT.,$,.RADIAN.)");
    let units = e.emit(&format!("IFCUNITASSIGNMENT((#{u1},#{u2}))"));
    e.emit(&format!(
        "IFCPROJECT('{proj_gid}',$,'SyntheticProject',$,$,$,$,(#{ctx}),#{units})"
    ));

    // ── Storeys with walls + meshes ────────────────────────────────────
    for s in 0..spec.storeys {
        let elev = s as f64 * 3.0;
        let storey_gid = e.gid();
        let slp = local_placement(&mut e, 0.0, 0.0, elev);
        e.emit(&format!(
            "IFCBUILDINGSTOREY('{storey_gid}',$,'Storey-{s}',$,$,#{slp},$,$,.ELEMENT.,{})",
            real(elev)
        ));

        for w in 0..spec.walls_per_storey {
            let mut x = w as f64 * 5.0;
            let mut name = format!("Wall-{s}-{w}");
            for m in mutations {
                match *m {
                    Mutation::RenameWall { storey, wall } if storey == s && wall == w => {
                        name.push_str("-renamed");
                    }
                    Mutation::MoveWall { storey, wall, dx } if storey == s && wall == w => {
                        x += dx;
                    }
                    _ => {}
                }
            }
            let wall_gid = e.gid();
            let lp = local_placement(&mut e, x, 0.0, elev);
            let prof_ax = axis_placement(&mut e, 0.0, 0.0, 0.0);
            let prof = e.emit(&format!(
                "IFCRECTANGLEPROFILEDEF(.AREA.,$,#{prof_ax},4.0,0.3)"
            ));
            let dir = e.emit("IFCDIRECTION((0.,0.,1.))");
            let solid_ax = axis_placement(&mut e, 0.0, 0.0, 0.0);
            let solid = e.emit(&format!(
                "IFCEXTRUDEDAREASOLID(#{prof},#{solid_ax},#{dir},3.0)"
            ));
            let sr = e.emit(&format!(
                "IFCSHAPEREPRESENTATION(#{ctx},'Body','SweptSolid',(#{solid}))"
            ));
            let pds = e.emit(&format!("IFCPRODUCTDEFINITIONSHAPE($,$,(#{sr}))"));
            e.emit(&format!(
                "IFCWALL('{wall_gid}',$,'{name}',$,$,#{lp},#{pds},$,.STANDARD.)"
            ));
        }

        for m in 0..spec.meshes_per_storey {
            let mut verts: Vec<[f64; 3]> = vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0, 1.0],
            ];
            for mu in mutations {
                if let Mutation::EditMesh { storey, mesh } = *mu {
                    if storey == s && mesh == m {
                        verts[3] = [0.0, 0.0, 1.5];
                    }
                }
            }
            // 1-based CoordIndex triangles over the canonical order.
            let mut tris: Vec<[usize; 3]> = vec![[1, 2, 3], [1, 2, 4], [1, 3, 4], [2, 3, 4]];
            if permute {
                // Reverse the vertex table; index i becomes n + 1 - i.
                let n = verts.len();
                verts.reverse();
                for t in &mut tris {
                    for ix in t.iter_mut() {
                        *ix = n + 1 - *ix;
                    }
                }
            }
            let coords = verts
                .iter()
                .map(|v| format!("({},{},{})", real(v[0]), real(v[1]), real(v[2])))
                .collect::<Vec<_>>()
                .join(",");
            let index = tris
                .iter()
                .map(|t| format!("({},{},{})", t[0], t[1], t[2]))
                .collect::<Vec<_>>()
                .join(",");
            let mesh_gid = e.gid();
            let lp = local_placement(&mut e, 20.0 + m as f64 * 3.0, 5.0, elev);
            let plist = e.emit(&format!("IFCCARTESIANPOINTLIST3D(({coords}))"));
            let faceset = e.emit(&format!(
                "IFCTRIANGULATEDFACESET(#{plist},$,.T.,({index}),$)"
            ));
            let sr = e.emit(&format!(
                "IFCSHAPEREPRESENTATION(#{ctx},'Body','Tessellation',(#{faceset}))"
            ));
            let pds = e.emit(&format!("IFCPRODUCTDEFINITIONSHAPE($,$,(#{sr}))"));
            e.emit(&format!(
                "IFCBUILDINGELEMENTPROXY('{mesh_gid}',$,'Mesh-{s}-{m}',$,$,#{lp},#{pds},$,$)"
            ));
        }
    }

    format!(
        "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION(('ViewDefinition [CoordinationView_V2.0]'),'2;1');\nFILE_NAME('synthetic.ifc','2024-01-01T00:00:00',('Vex'),('Vex'),'vex-test','vex-test','');\nFILE_SCHEMA(('IFC4'));\nENDSEC;\nDATA;\n{}ENDSEC;\nEND-ISO-10303-21;\n",
        e.out
    )
}
