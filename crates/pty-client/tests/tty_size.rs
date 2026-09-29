use pty_client::tty::size_or_default;

#[test]
fn a_zero_sized_terminal_uses_a_usable_default() {
    let mut master = 0;
    let mut slave = 0;
    let size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty fills the two descriptors and reads the supplied size.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &size,
            )
        },
        0
    );
    let actual = size_or_default(slave);
    // SAFETY: both descriptors were returned by openpty above.
    unsafe {
        libc::close(master);
        libc::close(slave);
    }
    assert_eq!(actual, (24, 80));
}
