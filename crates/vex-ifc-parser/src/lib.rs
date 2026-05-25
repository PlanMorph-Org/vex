//! Streaming STEP Part 21 / IFC parser.
//!
//! # Example
//!
//! ```no_run
//! use std::fs::File;
//! use std::io::BufReader;
//! use vex_ifc_parser::{Parser, ParseLimits};
//!
//! let file = File::open("model.ifc").unwrap();
//! let mut p = Parser::new(BufReader::new(file), ParseLimits::default());
//! let header = p.parse_preamble().unwrap();
//! println!("schema: {:?}", header.schemas);
//! while let Some(e) = p.next_entity().unwrap() {
//!     println!("#{} = {}", e.id, e.type_name);
//! }
//! ```

pub mod guid;
pub mod header;
pub mod intake;
pub mod lexer;
pub mod limits;
pub mod parser;

pub use guid::{decode_global_id, render_uuid};
pub use header::IfcHeader;
pub use intake::{parse_intake_metadata, IfcIntakeMetadata};
pub use lexer::{Lexer, Token};
pub use limits::ParseLimits;
pub use parser::{Parser, RawEntity, Value};
