use io_uring::{opcode, squeue, types};

use crate::{util::map_buf_result, BufResult, UringOp};

pub struct Read {
    pub buf: Vec<u8>,
    pub fd: crate::Fd,
}

unsafe impl UringOp for Read {
    type Output = BufResult<usize>;

    fn as_entry(&mut self) -> squeue::Entry {
        opcode::Read::new(
            types::Fd(self.fd.0),
            self.buf.as_mut_ptr(),
            u32::try_from(self.buf.len()).unwrap(),
        )
        .build()
    }

    fn into_result(self, result: i32) -> Self::Output {
        // TODO: Handle results greater than 2^31 - 1
        map_buf_result(result, self.buf, |n| n as usize)
    }
}
