//! The identity of the host's current boot.
//!
//! A process identity is only meaningful within one boot: after a reboot the
//! same pid and even the same start time can name a different process. Two
//! records carry this value today: the startup lease (`pty-lifecycle`) and the
//! launch-operation record ([`crate::operation`]).
//!
//! The value must not change while the host stays up, or a live process reads
//! as one from another boot. On macOS that rules out `kern.boottime`: the
//! kernel keeps boot time plus uptime equal to the calendar time, so every
//! clock step (`settimeofday`) moves it by the step.

/// `linux:<boot_id>` from `/proc/sys/kernel/random/boot_id`, or
/// `darwin:<uuid>` from `kern.bootsessionuuid`, which the kernel generates
/// once per boot. `None` when the host does not say.
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
        let name = c"kern.bootsessionuuid";
        // A `uuid_string_t`: 36 characters and a NUL.
        let mut value = [0u8; 37];
        let mut len = value.len();
        // SAFETY: the name is NUL-terminated; `value` and `len` describe a
        // writable buffer, and no replacement value is supplied.
        if unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        let value = std::str::from_utf8(&value[..len.min(value.len())]).ok()?;
        let value = value.trim_end_matches('\0');
        return is_uuid(value).then(|| format!("darwin:{value}"));
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// `8-4-4-4-12` hex digits: the only shape `kern.bootsessionuuid` has. Before
/// the kernel sets it the string is empty.
#[cfg(any(target_os = "macos", test))]
fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_boot_session_uuid_has_one_shape() {
        assert!(is_uuid("38DDC5AC-4C5B-46E4-8D50-B011C58AC697"));
        assert!(!is_uuid(""));
        assert!(!is_uuid("{ sec = 1791126940, usec = 986480 }"));
        assert!(!is_uuid("38DDC5AC-4C5B-46E4-8D50-B011C58AC69"));
        assert!(!is_uuid("38DDC5AC4C5B-46E4-8D50-B011C58AC6970"));
    }

    /// The identity names the boot, not the clock: it is the kernel's boot
    /// session UUID, and two reads agree.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_boot_identity_is_the_boot_session_uuid() {
        if std::env::var("PTY_TEST_BOOT_ID").is_ok() {
            return;
        }
        let first = read_boot_identity().expect("kern.bootsessionuuid is readable");
        let uuid = first.strip_prefix("darwin:").expect("darwin: prefix");
        assert!(is_uuid(uuid), "{first}");
        assert_eq!(read_boot_identity().as_deref(), Some(first.as_str()));
        let sysctl = std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "kern.bootsessionuuid"])
            .output()
            .expect("run sysctl");
        assert_eq!(String::from_utf8_lossy(&sysctl.stdout).trim(), uuid);
    }
}
