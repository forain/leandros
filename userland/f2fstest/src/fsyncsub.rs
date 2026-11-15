//! `f2fstest fsync PATH...`: a real fsync(2) of each file (uutils `sync FILE`
//! is a whole-filesystem sync, so it cannot exercise the fsync path).
use leandros_libc::*;

#[cfg(target_arch = "aarch64")]
const SYS_FSYNC: usize = 82;
#[cfg(target_arch = "x86_64")]
const SYS_FSYNC: usize = 74;

unsafe fn say(b: &[u8]) { write(1, b.as_ptr(), b.len()); }

pub unsafe fn fsync_paths(argc: i32, argv: *const *const u8) -> i32 {
    for i in 2..argc as usize {
        let p = *argv.add(i);
        let fd = open(p, O_RDONLY, 0);
        if fd < 0 || leandros_libc::syscall::syscall1(SYS_FSYNC, fd as usize) != 0 {
            say(b"FSYNC_FAIL\n");
            return 1;
        }
        close(fd);
        say(b"FSYNC_OK\n");
    }
    0
}
