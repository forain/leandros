# LeandrOS Missing Functionality Implementation Plan

## Overview
This document outlines all missing functionality in the LeandrOS codebase that needs to be implemented. The system follows a phased development approach with clear markers for what functionality is still stubbed or incomplete.

## Priority 1: Critical System Components (Immediate Implementation)

### 1. Complete x86-64 Signal Handling (Phase 2) — DONE 2026-06-30
**Why Critical**: The kernel has signal infrastructure but only AArch64 is fully implemented. x86-64 signal delivery is incomplete.
- Implement full x86-64 signal frame restoration
- Complete signal delivery timing for x86-64
- Add proper signal stack handling
- Ensure both architectures are fully compatible

**Actual root cause found**: `check_and_deliver_signals()` had no caller anywhere in
either architecture's trap-return path — signal delivery was dead code on AArch64
too, not just x86-64 as originally described here. Fixed by wiring it into the EL0
return paths in `exception_asm.s` (syscall, fault, and IRQ, but not the EL1 IRQ
path, which must not deliver user signals) and into the x86-64 `syscall_entry`
return path in `arch/x86_64/src/syscall.rs`. Added a full x86-64 `rt_sigframe`
implementation (`mod x86_64` in `sched/src/signal.rs`) matching the SysV
`ucontext`/`mcontext` layout used by relibc's Linux-ABI signal trampoline
(`__restore_rt`).
A real bug was caught and fixed during QEMU testing: the inserted x86-64 call
clobbered `rax` (the live syscall return value) because the existing return path
restores `rax` from the register, not from the stack — fixed by stashing it in a
callee-saved register across the call. `sigaltstack` remains a stub on both
architectures (still open).

**Security follow-up, fixed 2026-06-30**: AArch64's `rt_sigreturn` path restored
`spsr_el1` verbatim from the user-writable signal-stack frame, with no masking —
a malicious program could forge the `M[3:0]` mode field to request a return to
EL1 on the next `eret`, a privilege-escalation primitive. Fixed in
`sched/src/signal.rs` (`aarch64::restore`) by masking the restored value to the
N/Z/C/V condition flags only (`SPSR_NZCV_MASK`), forcing everything else —
exception level, AArch64/32 state, DAIF — back to the same `spsr_el1 == 0`
baseline a freshly created thread starts with. Mirrors the x86-64 path, which
never restores `cs`/`ss` from user memory and masks `rflags` the same way.

### 2. Complete Thread Management (Phase 4)
**Why Critical**: Threads are fundamental to multitasking and application execution.
- Implement thread-local storage
- Add mutex and condition variable support
- Implement thread attributes and cleanup handlers
- Add thread-specific data support

### 3. Complete Memory Management (Phase 6)
**Why Critical**: Memory management is core to system operation.
- Implement heap management with malloc/free
- Add memory pools and advanced allocation strategies
- Complete memory mapping with proper VMA management
- Implement memory protection and sharing

## Priority 2: Core System Integration (Phase 7)

### 4. Complete VFS Server Implementation (Phase 3/7)
**Why Critical**: VFS is the foundation for file operations and user-space compatibility.
- Implement full file system operations (open, read, write, etc.)
- Add device file support and directory operations
- Implement file permissions and ownership
- Add file locking and advisory locking support

### 5. Complete F2FS Implementation (Phase 7)
**Why Critical**: File system support is essential for data persistence.
- Implement double-indirect block pointers
- Add full F2FS file system operations
- Implement file system mounting and unmounting
- Add file system caching and buffering

## Priority 3: User Experience Features (Phase 8-9)

### 6. POSIX Timers (Phase 8)
**Why Critical**: Timers are essential for applications and system scheduling.
- Complete timer creation and expiration
- Implement timer signal delivery
- Add timer synchronization primitives
- Implement timer accounting and statistics

### 7. Poll/Select/Epoll (Phase 9)
**Why Critical**: Event notification is fundamental for I/O multiplexing.
- Complete poll implementation
- Implement select with proper event handling
- Complete epoll with efficient event notification
- Add performance optimizations for large fd sets

## Priority 4: Additional System Components

### 8. Network Stack Implementation (net server)
**Why Important**: Network connectivity is essential for modern systems.
- Implement complete network protocol support (TCP, UDP)
- Add socket operations (bind, connect, listen, accept)
- Implement network device drivers
- Add network configuration support

### 9. Audio Server Implementation
**Why Important**: Audio support provides user experience.
- Implement complete audio server with device support
- Add audio format and mixing support
- Implement audio streaming capabilities
- Add audio configuration and control

### 10. Input Device Support
**Why Important**: Input devices are essential for user interaction.
- Implement complete input device drivers
- Add input event handling and processing
- Implement input device abstraction and configuration
- Add support for various input device types

### 11. Graphics Subsystem
**Why Important**: Graphics support is essential for visual applications.
- Implement complete graphics drivers
- Add graphics API support and GPU acceleration
- Implement graphics memory management
- Add display management capabilities

## Implementation Strategy

### Phase-by-Phase Approach:
1. **Phase 2 (Signal Handling)** - Start with x86-64 signal delivery
2. **Phase 4 (Thread Management)** - Implement thread-local storage and synchronization
3. **Phase 6 (Memory Management)** - Add heap management and memory pools
4. **Phase 7 (VFS Server)** - Complete file system operations
5. **Phase 8 (POSIX Timers)** - Implement timer functionality
6. **Phase 9 (Poll/Select/Epoll)** - Complete event notification systems
7. **Phase 3 (VFS Integration)** - Complete VFS server implementation
8. **Network Stack** - Implement net server
9. **Audio Server** - Implement audio server
10. **Input Device Support** - Implement input drivers
11. **Graphics Subsystem** - Implement graphics drivers

### Key Implementation Considerations:
1. **Architecture Consistency** - Ensure both AArch64 and x86-64 are fully supported
2. **Performance** - Optimize critical paths for system calls
3. **Security** - Implement proper access controls and memory protection
4. **Compatibility** - Maintain Linux ABI compatibility where possible
5. **Testing** - Implement comprehensive unit and integration tests

## Todo List in Order of Priority

1. Complete x86-64 Signal Handling (Phase 2)
2. Complete Thread Management (Phase 4)
3. Complete Memory Management (Phase 6)
4. Complete VFS Server Implementation (Phase 3/7)
5. Complete F2FS Implementation (Phase 7)
6. POSIX Timers (Phase 8)
7. Poll/Select/Epoll (Phase 9)
8. Network Stack Implementation (net server)
9. Audio Server Implementation
10. Input Device Support
11. Graphics Subsystem