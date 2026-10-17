//! Capability admission pins the executable image, not its replaceable path.

mod cli_common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use cli_common::{Rig, pty_bin, wait_until};
use pty_lifecycle::{SpawnParams, prepare_daemon};
use serde_json::{Value, json};

const OPERATION: &str = "pinned-publisher-start";
const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CASE_ENV: &str = "PTY_ADMISSION_TEST_CASE";

// Each library scenario runs in its own process so PTY_ROOT can be supplied
// without mutating the multi-threaded test runner's environment. The outer Rig
// owns cleanup of every daemon and file, including on child assertion failure.
fn scenario(case: &str) {
    let rig = Rig::new();
    std::fs::set_permissions(&rig.root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let identity = rig.ok(&["publication-revocation", "identity", "--json"]).json();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "publisher_admission_child", "--nocapture"])
        .env(CASE_ENV, case)
        .env("PTY_ROOT", &rig.root)
        .env("PTY_ADMISSION_ROOT_ID", identity["root_id"].as_str().unwrap())
        .env_remove("PTY_SESSION")
        .env_remove("PTY_SESSION_DIR")
        .env_remove("PTY_SPAWNER_PID")
        .env_remove("PTY_CREATION_LOCK_OWNER_PID")
        .current_dir(&rig.scratch)
        .stdin(Stdio::null());
    if case == "external-dispatch" {
        command.env("PATH", &rig.scratch);
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "scenario {case}:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}

fn params(root_id: &str, marker: &Path) -> SpawnParams {
    let mut params = SpawnParams::new("publisher", "/bin/sh", &[
        "-c".into(), "printf ran > \"$1\"; exec cat".into(), "sh".into(),
        marker.to_string_lossy().into_owned(),
    ]);
    params.publication_binding = Some(pty_core::registry::revocation::PublicationBinding {
        operation: OPERATION.into(), digest: DIGEST.into(),
    });
    params.publication_root_id = Some(root_id.into());
    params.tags.insert("st3.start-operation".into(), OPERATION.into());
    params.tags.insert("keep".into(), "true".into());
    params
}

#[test]
fn managed_preparation_rejects_unsupported_native_publisher() {
    scenario("unsupported");
}

#[test]
fn managed_preparation_rejects_script_wrapper() {
    scenario("wrapper");
}

#[test]
fn prepared_native_image_survives_atomic_path_downgrade() {
    scenario("replace");
}

#[test]
fn prepared_native_image_survives_unlink() {
    scenario("unlink");
}

#[test]
fn legacy_unbound_preparation_does_not_probe_selected_executable() {
    scenario("legacy");
}

#[test]
fn managed_native_dispatcher_cannot_borrow_capability_from_path_extension() {
    scenario("external-dispatch");
}

#[test]
fn publisher_admission_child() {
    let Ok(case) = std::env::var(CASE_ENV) else { return };
    let root = PathBuf::from(std::env::var_os("PTY_ROOT").unwrap());
    let root_id = std::env::var("PTY_ADMISSION_ROOT_ID").unwrap();
    let scratch = std::env::current_dir().unwrap();
    let marker = scratch.join("child-ran");
    let params = params(&root_id, &marker);
    let executable = scratch.join("selected-publisher");
    match case.as_str() {
        "unsupported" => {
            assert!(prepare_daemon(&std::env::current_exe().unwrap(), &params).is_err());
        }
        "external-dispatch" => {
            // /usr/bin/env is a real native external-command dispatcher, not
            // an old PTY binary. It models the old CLI's PATH forwarding hazard.
            let capable = Command::new(pty_bin()).args(["capabilities", "--json"])
                .output().unwrap();
            assert!(capable.status.success());
            let expected: Value = serde_json::from_slice(&capable.stdout).unwrap();
            let encoded = String::from_utf8(capable.stdout).unwrap().trim().replace('\'', "'\\''");
            for extension in ["capabilities", "pty-capabilities"] {
                let path = scratch.join(extension);
                std::fs::write(&path, format!(
                    "#!/bin/sh\nprintf ran > \"$0.marker\"\nprintf '%s\\n' '{encoded}'\n"
                )).unwrap();
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            // Establish that this native dispatcher would advertise the forged
            // capability if admission inherited the fixture's PATH.
            let dispatched = Command::new("/usr/bin/env")
                .args(["capabilities", "--json"]).output().unwrap();
            assert!(dispatched.status.success());
            assert_eq!(serde_json::from_slice::<Value>(&dispatched.stdout).unwrap(), expected);
            let extension_marker = scratch.join("capabilities.marker");
            assert!(extension_marker.exists());
            std::fs::remove_file(&extension_marker).unwrap();
            assert!(prepare_daemon(Path::new("/usr/bin/env"), &params).is_err());
            assert!(!extension_marker.exists(), "admission executed PATH capabilities extension");
            assert!(!scratch.join("pty-capabilities.marker").exists());
        }
        "wrapper" => {
            std::fs::write(&executable, format!("#!/bin/sh\nexec '{}' \"$@\"\n", pty_bin())).unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(prepare_daemon(&executable, &params).is_err());
        }
        "legacy" => {
            let mut unbound = params.clone();
            unbound.publication_binding = None;
            unbound.publication_root_id = None;
            unbound.tags.remove("st3.start-operation");
            assert!(prepare_daemon(Path::new("/bin/false"), &unbound).is_ok());
            std::fs::write(&executable, "#!/bin/sh\nexit 1\n").unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(prepare_daemon(&executable, &unbound).is_ok());
        }
        "replace" | "unlink" => {
            std::fs::copy(pty_bin(), &executable).unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
            let prepared = prepare_daemon(&executable, &params)
                .unwrap_or_else(|error| panic!("prepare capable publisher: {error}"));
            if case == "replace" {
                let replacement = scratch.join("downgraded-publisher");
                // A native test image without PTY's built-in capability is a
                // portable downgrade fixture (no /bin/false assumption).
                std::fs::copy(std::env::current_exe().unwrap(), &replacement).unwrap();
                std::fs::rename(replacement, &executable).unwrap();
                assert!(!Command::new(&executable).args(["capabilities", "--json"]).status().unwrap().success());
            } else {
                std::fs::remove_file(&executable).unwrap();
                assert!(!executable.exists());
            }
            prepared.spawn(params)
                .unwrap_or_else(|error| panic!("spawn pinned admitted image: {error}"));
            wait_until("pinned publisher child", || marker.exists());
            let meta: Value = serde_json::from_slice(&std::fs::read(root.join("publisher.json")).unwrap()).unwrap();
            assert_eq!(meta["publicationBinding"], json!({"operation": OPERATION, "digest": DIGEST}));
            assert_eq!(meta["publicationRootId"], root_id);
            assert_eq!(meta["publicationCapability"], 1);
            assert_eq!(meta["tags"]["st3.start-operation"], OPERATION);
            assert!(meta["generation"].as_str().is_some_and(|value| !value.is_empty()));
            assert!(std::os::unix::net::UnixStream::connect(root.join("publisher.sock")).is_ok());
            return;
        }
        unknown => panic!("unknown admission scenario: {unknown}"),
    }
    assert!(!marker.exists());
    for suffix in ["json", "pid", "sock", "events.jsonl"] {
        assert!(!root.join(format!("publisher.{suffix}")).exists());
    }
}
