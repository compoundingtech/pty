use pty_client::TERMINAL_SANITIZE;
use pty_terminal::TerminalActor;

#[test]
fn detach_restores_the_hosts_default_colors() {
    let mut host = TerminalActor::new(24, 80, 100);
    let original = {
        let t = host.terminal();
        (
            t.fg_color().unwrap(),
            t.bg_color().unwrap(),
            t.cursor_color().unwrap(),
            t.color_palette().unwrap().0[1],
        )
    };
    host.write(b"\x1b]10;#ff0000\x07\x1b]11;#00ff00\x07\x1b]12;#0000ff\x07\x1b]4;1;#123456\x07");
    let changed = host.terminal().color_palette().unwrap().0[1];
    assert_ne!(changed, original.3);
    host.write(TERMINAL_SANITIZE.as_bytes());
    let t = host.terminal();
    assert_eq!(t.fg_color().unwrap(), original.0);
    assert_eq!(t.bg_color().unwrap(), original.1);
    assert_eq!(t.cursor_color().unwrap(), original.2);
    assert_eq!(t.color_palette().unwrap().0[1], original.3);
}
