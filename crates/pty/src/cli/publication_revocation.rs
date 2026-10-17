//! Durable, exact-operation publication fences; never session teardown.

use pty_core::registry::revocation::{self, PublicationBinding};

use super::{CliError, CliResult};

pub fn run(args: &[String]) -> CliResult {
    let Some(verb @ ("identity" | "install" | "read")) = args.first().map(String::as_str) else {
        return Err("pty publication-revocation: expected subcommand \"identity\", \"install\", or \"read\".".into());
    };
    let fail = |message: String| CliError(format!("pty publication-revocation {verb}: {message}"));
    let mut id = None;
    let mut operation = None;
    let mut binding = None;
    let mut intent = None;
    let mut root_id = None;
    let mut json = false;
    let mut index = 1;
    while index < args.len() {
        let flag = args[index].as_str();
        if flag == "--json" {
            if json {
                return Err(fail("--json may only be provided once.".into()));
            }
            json = true;
            index += 1;
            continue;
        }
        let slot = match flag {
            "--id" if verb != "identity" => &mut id,
            "--operation" if verb != "identity" => &mut operation,
            "--binding" if verb == "install" => &mut binding,
            "--intent" if verb == "install" => &mut intent,
            "--root-id" if verb == "install" => &mut root_id,
            _ => return Err(fail(format!("unexpected argument \"{flag}\"."))),
        };
        if slot.is_some() {
            return Err(fail(format!("{flag} may only be provided once.")));
        }
        let value = args
            .get(index + 1)
            .filter(|value| !value.is_empty() && !value.starts_with('-'))
            .ok_or_else(|| fail(format!("{flag} requires a value.")))?;
        *slot = Some(value.as_str());
        index += 2;
    }
    if verb == "identity" {
        if !json {
            return Err(fail("missing required --json.".into()));
        }
        let root_id = revocation::root_id().map_err(|error| fail(error.to_string()))?;
        println!("{}", serde_json::json!({"root_id": root_id}));
        return Ok(0);
    }
    let id = id.ok_or_else(|| fail("missing required --id.".into()))?;
    let operation = operation.ok_or_else(|| fail("missing required --operation.".into()))?;
    if !json {
        return Err(fail("missing required --json.".into()));
    }
    let output = if verb == "install" {
        let binding = binding.ok_or_else(|| fail("missing required --binding.".into()))?;
        let intent = intent.ok_or_else(|| fail("missing required --intent.".into()))?;
        let root_id = root_id.ok_or_else(|| fail("missing required --root-id.".into()))?;
        let installed = revocation::install(
            id,
            &PublicationBinding {
                operation: operation.to_owned(),
                digest: binding.to_owned(),
            },
            intent,
            root_id,
        )
        .map_err(|error| fail(error.to_string()))?;
        serde_json::to_string(&installed)
    } else {
        let record = revocation::read(id, operation)
            .map_err(|error| fail(error.to_string()))?
            .ok_or_else(|| fail("publication revocation not found.".into()))?;
        serde_json::to_string(&record)
    }
    .map_err(|error| fail(error.to_string()))?;
    println!("{output}");
    Ok(0)
}
