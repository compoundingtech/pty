mod cli_common;

use std::os::unix::fs::PermissionsExt;

use cli_common::Rig;

#[test]
fn impossible_starts_report_the_cause_before_creating_a_session() {
    let rig = Rig::new();
    let missing_cwd = rig.scratch.join("missing-cwd");
    let missing_cwd = missing_cwd.to_str().unwrap();
    let out = rig.run(&[
        "run", "-d", "--id", "nocwd", "--cwd", missing_cwd,
        "--", "/bin/sh",
    ]);
    assert_ne!(out.code, 0, "{out:?}");
    assert!(
        out.stderr.contains(&format!("Working directory does not exist: {missing_cwd}")),
        "{out:?}"
    );
    assert!(!rig.path("nocwd.json").exists());

    let no_exec = rig.scratch.join("no-exec");
    std::fs::write(&no_exec, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&no_exec, std::fs::Permissions::from_mode(0o644)).unwrap();
    let no_exec = no_exec.to_str().unwrap();
    let out = rig.run(&["run", "-d", "--id", "noexec", "--", no_exec]);
    assert_ne!(out.code, 0, "{out:?}");
    assert!(out.stderr.contains(no_exec), "{out:?}");
    assert!(!rig.path("noexec.json").exists());

    let no_interpreter = rig.scratch.join("no-interpreter");
    std::fs::write(&no_interpreter, "#!/no/such/interpreter\nexit 0\n").unwrap();
    std::fs::set_permissions(&no_interpreter, std::fs::Permissions::from_mode(0o755)).unwrap();
    let no_interpreter = no_interpreter.to_str().unwrap();
    let out = rig.run(&["run", "-d", "--id", "nointerp", "--", no_interpreter]);
    assert_ne!(out.code, 0, "{out:?}");
    assert!(out.stderr.contains(no_interpreter), "{out:?}");
    assert!(!rig.path("nointerp.json").exists());
}
