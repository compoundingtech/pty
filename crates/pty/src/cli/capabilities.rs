//! `pty capabilities --json`: what this binary can do, for a launcher that
//! must not guess from a version number.
//!
//! An older binary has no such verb: it runs a `pty-capabilities` extension
//! if one is on `PATH`, or prints `Unknown command` and exits 1. A caller
//! therefore treats anything but exit 0 with a parseable object that lists the
//! version it needs as "unsupported".

use serde_json::json;

use super::CliResult;
use pty_core::operation::{CAPABILITY, RECORD_SCHEMA, VERSIONS};

/// The one object `pty capabilities --json` prints.
pub fn capabilities() -> serde_json::Value {
    json!({
        "version": super::version::version(),
        "capabilities": {
            CAPABILITY: {
                "versions": VERSIONS,
                "recordSchemas": [RECORD_SCHEMA],
            }
        }
    })
}

pub fn run(args: &[String]) -> CliResult {
    match args {
        [] => {}
        [flag] if flag == "--json" => {}
        [flag] if flag == "-h" || flag == "--help" => {
            println!(
                "Usage: pty capabilities --json\n\nPrint this binary's capabilities as one JSON object.\n\nExample:\n  pty capabilities --json"
            );
            return Ok(0);
        }
        _ => {
            eprintln!("Usage: pty capabilities --json");
            return Ok(64);
        }
    }
    println!("{}", capabilities());
    Ok(0)
}
