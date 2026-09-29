#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use pty_testkit::{Session, SpawnOptions};

#[test]
fn a_running_picker_can_create_a_session_after_its_binary_is_replaced() {
    let root = std::env::temp_dir().join(format!(
        "pu-{}",
        pty_testkit::server::random_id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let installed = root.join("pty");
    std::fs::copy(env!("CARGO_BIN_EXE_pty"), &installed).unwrap();
    let mut picker = Session::spawn(
        installed.to_str().unwrap(),
        &["--preselect-new"],
        SpawnOptions {
            rows: Some(24),
            cols: Some(100),
            env: vec![
                ("PTY_ROOT".into(), root.to_string_lossy().into_owned()),
                ("HOME".into(), root.to_string_lossy().into_owned()),
                ("SHELL".into(), "/bin/sh".into()),
                ("PTY_CREATION_LOCK_OWNER_PID".into(), String::new()),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    picker.wait_for_text("+ Create new session...", 8000).unwrap();

    let staged = root.join("pty.new");
    std::fs::copy(env!("CARGO_BIN_EXE_pty"), &staged).unwrap();
    std::fs::rename(staged, &installed).unwrap();
    picker.type_str("\r");

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut created = false;
    while Instant::now() < deadline {
        created = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .any(|entry| entry.path().extension().is_some_and(|ext| ext == "sock"));
        if created {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    picker.type_str("\x1c");
    drop(picker);
    for entry in std::fs::read_dir(&root).unwrap().flatten() {
        let path: PathBuf = entry.path();
        if path.extension().is_some_and(|ext| ext == "sock") {
            let name = path.file_stem().unwrap().to_string_lossy();
            for command in ["kill", "rm"] {
                let _ = Command::new(&installed)
                    .args([command, &name])
                    .env("PTY_ROOT", &root)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
    }
    let _ = std::fs::remove_dir_all(root);
    assert!(created, "the picker could not create a session after replacement");
}
