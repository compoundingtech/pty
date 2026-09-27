//! Bounded terminal-evidence reads and generation-fenced removal.
//!
//! These operations intentionally acquire the same creation/event locks as a
//! replacement launch. A caller can therefore consume one retained generation
//! without racing a new daemon into the stable id.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;

use serde::Serialize;
use serde_json::Value;

use super::list::{pid_alive, read_session_pid, socket_reachable};
use super::lock::{acquire_event_lock, acquire_lock};
use super::metadata::SESSION_EXIT_LAST_LINES_LIMIT;
use super::root::{
    events_path, metadata_path, output_activity_path, pid_path, recovery_revision_path, socket_path,
};
use super::names::validate_name;

const METADATA_MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "_tag", rename_all = "kebab-case")]
pub enum SessionExitEvidenceResult {
    Snapshot { snapshot: SessionExitEvidence },
    Unavailable { reason: EvidenceUnavailableReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceUnavailableReason {
    Missing,
    Running,
    Busy,
    GenerationUnavailable,
    InvalidMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionExitEvidence {
    pub name: String,
    pub generation: String,
    pub status: SessionExitStatus,
    pub exit_code: Option<i32>,
    pub stream: EvidenceStream,
    pub tail: SessionExitEvidenceTail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionExitStatus {
    Exited,
    Vanished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EvidenceStream {
    Combined,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "_tag", rename_all = "lowercase")]
pub enum SessionExitEvidenceTail {
    Present {
        #[serde(rename = "lastLines")]
        last_lines: Vec<String>,
    },
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "_tag", rename_all = "kebab-case")]
pub enum RemoveSessionGenerationResult {
    Removed,
    Missing,
    GenerationMismatch,
    NotTerminal,
    InvalidMetadata,
    Busy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EvidenceMetadata {
    generation: String,
    daemon_pid: Option<i32>,
    daemon_start_token: Option<String>,
    exited_at: Option<String>,
    exit_code: Option<i32>,
    last_lines: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EvidenceMetadataRead {
    Valid(EvidenceMetadata),
    Missing,
    GenerationUnavailable,
    Invalid,
}

fn read_evidence_metadata(name: &str) -> EvidenceMetadataRead {
    let path = metadata_path(name);
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return EvidenceMetadataRead::Missing;
        }
        Err(_) => return EvidenceMetadataRead::Invalid,
    };
    let Ok(stat) = file.metadata() else {
        return EvidenceMetadataRead::Invalid;
    };
    if !stat.is_file() || stat.len() > METADATA_MAX_BYTES {
        return EvidenceMetadataRead::Invalid;
    }
    let mut bytes = Vec::with_capacity(stat.len() as usize);
    if file
        .by_ref()
        .take(METADATA_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > METADATA_MAX_BYTES
    {
        return EvidenceMetadataRead::Invalid;
    }
    let Ok(Value::Object(record)) = serde_json::from_slice::<Value>(&bytes) else {
        return EvidenceMetadataRead::Invalid;
    };
    let Some(generation_value) = record.get("generation") else {
        return EvidenceMetadataRead::GenerationUnavailable;
    };
    let Some(generation) = generation_value.as_str().filter(|value| !value.is_empty()) else {
        return EvidenceMetadataRead::Invalid;
    };
    let daemon_pid = match record.get("daemonPid") {
        None => None,
        Some(value) => match value.as_i64().and_then(|pid| i32::try_from(pid).ok()) {
            Some(pid) if pid > 0 => Some(pid),
            _ => return EvidenceMetadataRead::Invalid,
        },
    };
    let daemon_start_token = match record.get("daemonStartToken") {
        Some(Value::String(token)) if !token.is_empty() => Some(token.to_string()),
        Some(_) => return EvidenceMetadataRead::Invalid,
        None => record
            .get("recovery")
            .and_then(Value::as_object)
            .and_then(|recovery| recovery.get("processStartToken"))
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_string),
    };
    let exited_at = record.get("exitedAt");
    let exit_code = record.get("exitCode");
    if exited_at.is_some() != exit_code.is_some() {
        return EvidenceMetadataRead::Invalid;
    }
    let (exited_at, exit_code) = match (exited_at, exit_code) {
        (Some(at), Some(code)) => {
            let Some(at) = at.as_str().filter(|value| !value.is_empty()) else {
                return EvidenceMetadataRead::Invalid;
            };
            let Some(code) = code.as_i64().and_then(|code| i32::try_from(code).ok()) else {
                return EvidenceMetadataRead::Invalid;
            };
            (Some(at.to_string()), Some(code))
        }
        (None, None) => (None, None),
        _ => unreachable!(),
    };
    let last_lines = match record.get("lastLines") {
        None => None,
        Some(Value::Array(lines)) if lines.len() <= SESSION_EXIT_LAST_LINES_LIMIT => {
            let Some(lines) = lines
                .iter()
                .map(|line| line.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
            else {
                return EvidenceMetadataRead::Invalid;
            };
            Some(lines)
        }
        Some(_) => return EvidenceMetadataRead::Invalid,
    };
    EvidenceMetadataRead::Valid(EvidenceMetadata {
        generation: generation.to_string(),
        daemon_pid,
        daemon_start_token,
        exited_at,
        exit_code,
        last_lines,
    })
}

fn generation_is_alive(name: &str, metadata: &EvidenceMetadata) -> bool {
    let daemon_pid_is_alive = |pid| {
        metadata.daemon_start_token.as_deref().is_some_and(|token| {
            pid_alive(pid)
                && super::list::read_process_start_token(pid).as_deref() == Some(token)
        })
    };
    read_session_pid(name).is_some_and(daemon_pid_is_alive)
        || metadata.daemon_pid.is_some_and(daemon_pid_is_alive)
        || (socket_path(name).exists() && socket_reachable(&socket_path(name)))
}

pub fn get_session_exit_evidence(name: &str) -> SessionExitEvidenceResult {
    if validate_name(name).is_err() {
        return SessionExitEvidenceResult::Unavailable {
            reason: EvidenceUnavailableReason::InvalidMetadata,
        };
    }
    let Some(_lock) = acquire_lock(name) else {
        return SessionExitEvidenceResult::Unavailable {
            reason: EvidenceUnavailableReason::Busy,
        };
    };
    let metadata = match read_evidence_metadata(name) {
        EvidenceMetadataRead::Valid(metadata) => metadata,
        EvidenceMetadataRead::Missing => {
            return SessionExitEvidenceResult::Unavailable {
                reason: EvidenceUnavailableReason::Missing,
            };
        }
        EvidenceMetadataRead::GenerationUnavailable => {
            return SessionExitEvidenceResult::Unavailable {
                reason: EvidenceUnavailableReason::GenerationUnavailable,
            };
        }
        EvidenceMetadataRead::Invalid => {
            return SessionExitEvidenceResult::Unavailable {
                reason: EvidenceUnavailableReason::InvalidMetadata,
            };
        }
    };
    if generation_is_alive(name, &metadata) {
        return SessionExitEvidenceResult::Unavailable {
            reason: EvidenceUnavailableReason::Running,
        };
    }
    let exited = metadata.exited_at.is_some();
    SessionExitEvidenceResult::Snapshot {
        snapshot: SessionExitEvidence {
            name: name.to_string(),
            generation: metadata.generation,
            status: if exited {
                SessionExitStatus::Exited
            } else {
                SessionExitStatus::Vanished
            },
            exit_code: metadata.exit_code,
            stream: EvidenceStream::Combined,
            tail: match metadata.last_lines {
                Some(last_lines) => SessionExitEvidenceTail::Present { last_lines },
                None => SessionExitEvidenceTail::Unavailable,
            },
        },
    }
}

fn unlink_if_present(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn remove_session_generation(
    name: &str,
    expected_generation: &str,
) -> std::io::Result<RemoveSessionGenerationResult> {
    if validate_name(name).is_err() {
        return Ok(RemoveSessionGenerationResult::InvalidMetadata);
    }
    let Some(_event_lock) = acquire_event_lock(name) else {
        return Ok(RemoveSessionGenerationResult::Busy);
    };
    let Some(_creation_lock) = acquire_lock(name) else {
        return Ok(RemoveSessionGenerationResult::Busy);
    };
    let first = match read_evidence_metadata(name) {
        EvidenceMetadataRead::Missing => return Ok(RemoveSessionGenerationResult::Missing),
        EvidenceMetadataRead::GenerationUnavailable => {
            return Ok(RemoveSessionGenerationResult::GenerationMismatch);
        }
        EvidenceMetadataRead::Invalid => {
            return Ok(RemoveSessionGenerationResult::InvalidMetadata);
        }
        EvidenceMetadataRead::Valid(metadata) => metadata,
    };
    if first.generation != expected_generation {
        return Ok(RemoveSessionGenerationResult::GenerationMismatch);
    }
    if generation_is_alive(name, &first) {
        return Ok(RemoveSessionGenerationResult::NotTerminal);
    }
    let second = match read_evidence_metadata(name) {
        EvidenceMetadataRead::Missing => return Ok(RemoveSessionGenerationResult::Missing),
        EvidenceMetadataRead::GenerationUnavailable => {
            return Ok(RemoveSessionGenerationResult::GenerationMismatch);
        }
        EvidenceMetadataRead::Invalid => {
            return Ok(RemoveSessionGenerationResult::InvalidMetadata);
        }
        EvidenceMetadataRead::Valid(metadata) => metadata,
    };
    if second.generation != expected_generation {
        return Ok(RemoveSessionGenerationResult::GenerationMismatch);
    }
    // Evidence metadata is removed last. Any preceding I/O failure preserves
    // the generation for a safe retry.
    unlink_if_present(&socket_path(name))?;
    unlink_if_present(&pid_path(name))?;
    unlink_if_present(&events_path(name))?;
    unlink_if_present(&recovery_revision_path(name))?;
    unlink_if_present(&output_activity_path(name))?;
    unlink_if_present(&metadata_path(name))?;
    Ok(RemoveSessionGenerationResult::Removed)
}
