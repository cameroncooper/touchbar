use std::{
    ffi::{CStr, CString, c_char},
    io,
    os::{fd::AsRawFd, unix::net::UnixStream},
    ptr,
};

use anyhow::{Context, Result, bail};

#[link(name = "systemd")]
unsafe extern "C" {
    fn sd_seat_get_active(
        seat: *const c_char,
        session: *mut *mut c_char,
        uid: *mut libc::uid_t,
    ) -> libc::c_int;
}

/// Resolve the active local user through libsystemd's supported sd-login API.
pub fn active_seat_uid(seat: &str) -> Result<u32> {
    let seat = CString::new(seat).context("seat name contains NUL")?;
    let mut session = ptr::null_mut();
    let mut uid = 0;
    let result = unsafe { sd_seat_get_active(seat.as_ptr(), &mut session, &mut uid) };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(-result)).context("query active seat session");
    }
    if session.is_null() {
        bail!("seat has no active session");
    }
    let session_name = unsafe { CStr::from_ptr(session) }
        .to_string_lossy()
        .into_owned();
    unsafe { libc::free(session.cast()) };
    if session_name.is_empty() || uid == 0 {
        bail!("active seat returned an invalid session identity");
    }
    Ok(uid)
}

pub fn peer_uid(stream: &UnixStream) -> Result<u32> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error()).context("read session peer credentials");
    }
    if length as usize != std::mem::size_of::<libc::ucred>() {
        bail!("kernel returned malformed session peer credentials");
    }
    Ok(credentials.uid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_peer_uid_is_kernel_authenticated() {
        let (left, right) = UnixStream::pair().unwrap();
        let expected = unsafe { libc::geteuid() };
        assert_eq!(peer_uid(&left).unwrap(), expected);
        assert_eq!(peer_uid(&right).unwrap(), expected);
    }
}
