use std::io;

use io_uring::{opcode, squeue, types::Timespec};

use crate::UringOp;

pub struct Sleep {
    ts: Box<Timespec>,
}

impl Sleep {
    pub fn from_millis(millis: u64) -> Self {
        let sec = millis / 1000;
        let nsec = (millis % 1000) * 1000 * 1000;
        let ts = Timespec::new().sec(sec).nsec(nsec as u32);
        Sleep { ts: Box::new(ts) }
    }
}

unsafe impl UringOp for Sleep {
    type Output = io::Result<()>;

    fn as_entry(&mut self) -> squeue::Entry {
        opcode::Timeout::new(&*self.ts as *const _).count(0).build()
    }

    fn into_result(self, result: i32) -> Self::Output {
        if result == -libc::ETIME {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(-result))
        }
    }
}
