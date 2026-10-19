//! `pty root [--json]`: report root selection without accessing the registry.

use std::path::Path;

use pty_core::registry;
use serde::Serialize;

use super::{CliError, CliResult};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RootReport<'a> {
    effective: EffectiveRoot<'a>,
    native_default: RootPath<'a>,
}

#[derive(Serialize)]
struct EffectiveRoot<'a> {
    path: &'a Path,
    source: &'static str,
}

#[derive(Serialize)]
struct RootPath<'a> {
    path: &'a Path,
}

/// Print the effective root and the native default, preserving selected paths.
pub fn run(rest: &[String], root_flag: bool) -> CliResult {
    let json = match rest {
        [] => false,
        [argument] if argument == "--json" => true,
        _ => return Err(CliError("Usage: pty root [--json]".to_string())),
    };

    let (effective, resolved_source) = registry::resolve_session_dir();
    let source = if root_flag {
        "flag"
    } else {
        resolved_source.as_str()
    };
    let native_default = registry::default_session_dir();

    if json {
        let report = RootReport {
            effective: EffectiveRoot {
                path: &effective,
                source,
            },
            native_default: RootPath {
                path: &native_default,
            },
        };
        let output = serde_json::to_string(&report)
            .map_err(|error| CliError(format!("pty root: {error}")))?;
        println!("{output}");
    } else {
        println!("Effective: {} ({source})", effective.display());
        println!("Native default: {}", native_default.display());
    }
    Ok(0)
}
