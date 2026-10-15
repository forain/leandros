import sys
p=sys.argv[1]+'/kernel/src/syscall.rs'
s=open(p).read()
anchor='''    if ((SYSCALL_TRACE_EINVAL && ret == -22) || (SYSCALL_TRACE_EACCES && ret == -13)) && current_pid() >= 3 {'''
assert s.count(anchor)==1, p
add='''    // DIAG(ffpaint): every ENOMEM, and every failing DRM ioctl ('d' type),
    // budget-limited: what made Mesa raise GL_OUT_OF_MEMORY.
    if (ret == -12 || (number == FFP_SYS_IOCTL && ret < 0 && ((a1 >> 8) & 0xff) == 0x64)) && current_pid() >= 3 {
        static FFP_BUDGET: AtomicUsize = AtomicUsize::new(400);
        if FFP_BUDGET.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1)).is_ok() {
            crate::serial_print_str("[FFPFAIL] nr=");
            crate::serial_print_hex(number);
            crate::serial_print_str(" ret=");
            crate::serial_print_hex(ret as usize);
            crate::serial_print_str(" a0=");
            crate::serial_print_hex(a0);
            crate::serial_print_str(" a1=");
            crate::serial_print_hex(a1);
            crate::serial_print_str(" a2=");
            crate::serial_print_hex(a2);
            crate::serial_print_str(" pid=");
            crate::serial_print_hex(current_pid() as usize);
            crate::serial_print_str(" tgid=");
            crate::serial_print_hex(sched::current_tgid() as usize);
            crate::serial_print_str(" t_ms=");
            crate::serial_print_hex((monotonic_ns() / 1_000_000) as usize);
            crate::serial_print_str("\\n");
        }
    }
'''
s=s.replace(anchor, add+anchor)
s+='''
#[cfg(target_arch = "aarch64")]
const FFP_SYS_IOCTL: usize = 29;
#[cfg(target_arch = "x86_64")]
const FFP_SYS_IOCTL: usize = 16;
'''
if '--enomem' in sys.argv:
    s=s.replace('const DBG_ENOMEM: bool = false;','const DBG_ENOMEM: bool = true;')
open(p,'w').write(s)
print("patched",p)
