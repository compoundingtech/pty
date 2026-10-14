//! The identity of the host's current boot.
//!
//! A process identity is only meaningful within one boot: after a reboot the
//! same pid and even the same start time can name a different process. Two
//! records carry this value today: the startup lease (`pty-lifecycle`) and the
//! launch-operation record ([`crate::operation`]).

/// `linux:<boot_id>` from `/proc/sys/kernel/random/boot_id`, or
/// `darwin:{ sec = S, usec = U }` from `kern.boottime`. `None` when the host
/// does not say.
pub fn read_boot_identity() -> Option<String> {
    // Debug builds: a test can make the boot identity unreadable, to prove
    // that nothing is signalled without it.
    #[cfg(debug_assertions)]
    if std::env::var("PTY_TEST_BOOT_ID").as_deref() == Ok("unavailable") {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let value = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let value = value.trim();
        return (!value.is_empty()).then(|| format!("linux:{value}"));
    }
    #[cfg(target_os = "macos")]
    {
        let name = c"kern.boottime";
        let mut value: libc::timeval = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::timeval>();
        // SAFETY: the name is NUL-terminated; `value` and `len` describe a
        // writable `timeval`, and no replacement value is supplied.
        if unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&mut value as *mut libc::timeval).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        } != 0
            || len != std::mem::size_of::<libc::timeval>()
        {
            return None;
        }
        return Some(format!(
            "darwin:{{ sec = {}, usec = {} }}",
            value.tv_sec, value.tv_usec
        ));
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}
