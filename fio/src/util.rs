use std::{
    ffi::{CString, OsStr},
    io,
};

use crate::BufResult;

/// Panics if the provided string contains a NUL-byte.
#[cfg(unix)]
pub fn into_c_string(s: &OsStr) -> CString {
    #[cfg(not(unix))]
    compile_error!("not supported");

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        CString::new(s.as_bytes()).expect("must not contain NUL-byte")
    }
}

pub fn map_result<T>(res: i32, ok: impl FnOnce(i32) -> T) -> io::Result<T> {
    if res < 0 {
        let err = -res;
        Err(io::Error::from_raw_os_error(err))
    } else {
        Ok(ok(res))
    }
}

pub fn map_buf_result<T, B>(res: i32, buf: B, ok: impl FnOnce(i32) -> T) -> BufResult<T, B> {
    if res < 0 {
        let err = -res;
        (buf, Err(io::Error::from_raw_os_error(err)))
    } else {
        (buf, Ok(ok(res)))
    }
}
