//! `pty rm <ref>` / `pty remove <ref>`: remove a session that is not
//! running from the registry — socket, pid, metadata, events, recovery
//! revision — under the creation lock with a generation check. The removal
//! itself is `pty_client::remove`.
//!
//! node: src/cli.ts:1604-1613, 3036-3087 (`cmdRm`)

use super::{CliError, CliResult, require_ref};

/// `cmdRm`.
pub fn run(args: &[String]) -> CliResult {
    let name = require_ref(args, "Usage: pty rm <name>")?;
    pty_client::remove(&name).map_err(|error| CliError(error.to_string()))?;
    println!("Session \"{name}\" removed.");
    Ok(0)
}
