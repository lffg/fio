use std::{ffi::CString, path::Path};

use io_uring::{opcode, squeue, types};

use crate::{
    util::{into_c_string, map_result},
    UringOp,
};

#[derive(Copy, Clone)]
pub struct Fd(pub(crate) i32);

pub(crate) struct OpenAt {
    path: CString,
}

impl OpenAt {
    pub fn at_cwd(path: &Path) -> Self {
        OpenAt {
            path: into_c_string(path.as_os_str()),
        }
    }
}

unsafe impl UringOp for OpenAt {
    type Output = std::io::Result<crate::Fd>;

    fn as_entry(&mut self) -> squeue::Entry {
        opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), self.path.as_ptr()).build()
    }

    fn into_result(self, result: i32) -> Self::Output {
        map_result(result, crate::Fd)
    }
}
