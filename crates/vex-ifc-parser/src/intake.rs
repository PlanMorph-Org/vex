use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use serde::{Deserialize, Serialize};
use vex_utils::{VexError, VexResult};

use crate::{ParseLimits, Parser, Value};

const MAX_INTAKE_SCAN_ENTITIES: u64 = 8_192;
const INTAKE_READER_CAPACITY: usize = 128 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct IfcIntakeMetadata {
    pub description: Option<String>,
    pub project_guid: Option<String>,
    pub project_name: Option<String>,
    pub author: Option<String>,
    pub originating_system: Option<String>,
    pub approximate_entity_count: u64,
}

pub fn parse_intake_metadata(path: &Path) -> VexResult<IfcIntakeMetadata> {
    let file = File::open(path).map_err(|source| VexError::io_at(path, source))?;
    let reader = BufReader::with_capacity(INTAKE_READER_CAPACITY, file);
    let mut parser = Parser::new(reader, ParseLimits::default());
    let header = parser.parse_preamble()?;

    let mut out = IfcIntakeMetadata {
        description: header.description.into_iter().next(),
        author: header.author.into_iter().next(),
        originating_system: header.originating_system,
        ..IfcIntakeMetadata::default()
    };

    for _ in 0..MAX_INTAKE_SCAN_ENTITIES {
        let Some(entity) = parser.next_entity()? else {
            return Ok(out);
        };
        out.approximate_entity_count = out.approximate_entity_count.saturating_add(1);
        if entity.type_name == "IFCPROJECT" {
            out.project_guid = string_arg(&entity.args, 0);
            out.project_name = string_arg(&entity.args, 2);
            return Ok(out);
        }
    }

    Ok(out)
}

fn string_arg(args: &[Value], index: usize) -> Option<String> {
    match args.get(index) {
        Some(Value::Str(value)) if !value.is_empty() => Some(value.clone()),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn parses_header_and_project_metadata() {
        let path = temp_ifc(
            "ISO-10303-21;\n\
             HEADER;\n\
             FILE_DESCRIPTION(('Coordination view'),'2;1');\n\
             FILE_NAME('tower.ifc','2026-05-24T12:34:00',('Lawrence'),('Planmorph'),'vex','Revit 2026','');\n\
             FILE_SCHEMA(('IFC4'));\n\
             ENDSEC;\n\
             DATA;\n\
             #1 = IFCPROJECT('2HnQxDrSH5sBbC4NkVOGR8', $, 'Commercial Tower', $, $, $, $, $, $);\n\
             ENDSEC;\n\
             END-ISO-10303-21;\n",
        );

        let metadata = parse_intake_metadata(&path).unwrap();
        assert_eq!(metadata.description.as_deref(), Some("Coordination view"));
        assert_eq!(metadata.author.as_deref(), Some("Lawrence"));
        assert_eq!(metadata.originating_system.as_deref(), Some("Revit 2026"));
        assert_eq!(
            metadata.project_guid.as_deref(),
            Some("2HnQxDrSH5sBbC4NkVOGR8")
        );
        assert_eq!(metadata.project_name.as_deref(), Some("Commercial Tower"));
        assert_eq!(metadata.approximate_entity_count, 1);
        let _ = fs::remove_file(path);
    }

    fn temp_ifc(body: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("vex-intake-{nonce}.ifc"));
        fs::write(&path, body).expect("write temp ifc");
        path
    }
}
