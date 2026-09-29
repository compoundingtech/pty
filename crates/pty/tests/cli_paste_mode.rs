mod cli_common;

use cli_common::{Rig, wait_until};

#[test]
fn paste_markers_are_not_sent_to_a_child_without_bracketed_paste() {
    let rig = Rig::new();
    let output = rig.scratch.join("paste.bin");
    let text = "echo one\necho two";
    let script = format!(
        "stty raw -echo; printf '\\033[?2004lREADY\\n'; \
         dd bs=1 count={} of='{}' 2>/dev/null; exec sleep 30",
        text.len(),
        output.display()
    );
    rig.ok(&["run", "-d", "--id", "paste-off", "--", "/bin/sh", "-c", &script]);
    wait_until("child ready", || {
        rig.run(&["peek", "--plain", "--full", "paste-off"])
            .stdout
            .contains("READY")
    });
    let sent = rig.run(&["send", "paste-off", "--paste", text]);
    assert_eq!(sent.code, 0, "{sent:?}");
    wait_until("paste bytes", || {
        std::fs::metadata(&output).is_ok_and(|meta| meta.len() == text.len() as u64)
    });
    assert_eq!(std::fs::read(output).unwrap(), text.as_bytes());
}

#[test]
fn paste_markers_are_sent_to_a_child_with_bracketed_paste() {
    let rig = Rig::new();
    let output = rig.scratch.join("paste-on.bin");
    let text = "one\ntwo";
    let wanted = format!("\x1b[200~{text}\x1b[201~");
    let script = format!(
        "stty raw -echo; printf '\\033[?2004hREADY\\n'; \
         dd bs=1 count={} of='{}' 2>/dev/null; exec sleep 30",
        wanted.len(),
        output.display()
    );
    rig.ok(&["run", "-d", "--id", "paste-on", "--", "/bin/sh", "-c", &script]);
    wait_until("child ready", || {
        rig.run(&["peek", "--plain", "--full", "paste-on"])
            .stdout
            .contains("READY")
    });
    let sent = rig.run(&["send", "paste-on", "--paste", text]);
    assert_eq!(sent.code, 0, "{sent:?}");
    wait_until("paste bytes", || {
        std::fs::metadata(&output).is_ok_and(|meta| meta.len() == wanted.len() as u64)
    });
    assert_eq!(std::fs::read(output).unwrap(), wanted.as_bytes());
}
