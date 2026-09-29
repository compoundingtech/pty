#![cfg(target_os = "linux")]

use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Duration;

use pty_client::{ClientIo, attach::{AttachParams, attach}};

#[test]
fn an_attached_client_leaves_when_its_terminal_hangs_up() {
    let mut master = 0;
    let mut slave = 0;
    // SAFETY: openpty fills both descriptors.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        },
        0
    );
    let (client, daemon) = UnixStream::pair().unwrap();
    let (done, finished) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let io = ClientIo {
            stdin: slave,
            stdout: slave,
            stderr: slave,
        };
        let result = attach(AttachParams::new("hangup", client), &io);
        // SAFETY: the worker owns this openpty descriptor.
        unsafe { libc::close(slave) };
        done.send(result).unwrap();
    });
    std::thread::sleep(Duration::from_millis(100));
    // SAFETY: the test owns the master side; closing it simulates a lost tty.
    unsafe { libc::close(master) };

    let exited_on_hangup = finished.recv_timeout(Duration::from_millis(600)).is_ok();
    drop(daemon); // Release an old attach loop so the test never hangs.
    worker.join().unwrap();
    assert!(exited_on_hangup, "the client stayed attached after its tty hung up");
}
