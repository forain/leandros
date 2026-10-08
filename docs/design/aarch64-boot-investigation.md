# AArch64 Boot Investigation & Fix Report

## Overview
This document details the investigation and technical changes performed to fix the AArch64 boot process in LeandrOS. The primary goal is to enable the AArch64 target to reach a functional shell prompt, matching the current state of the x86_64 target.

## Investigation History

### 1. Early Boot "Unknown" Exceptions
**Problem:** The kernel was crashing almost immediately after Limine handed over control, triggering synchronous exceptions with `EC=0x00` (Unknown exception class).
**Cause:** The kernel was attempting to use the AArch64 Physical Timer (`CNTP`). In many EL1 environments (including QEMU virt and some UEFI firmware), access to physical timer registers can trigger traps or be restricted.
**Fix:** Switched the entire architecture-specific timer implementation to use the **Virtual Timer (`CNTV`)** and updated GIC routing to handle PPI #27 instead of PPI #30.

### 2. Page Table Corruption & Physical Dereferences
**Problem:** The paging code was crashing when attempting to create new userspace page tables.
**Cause:** The code was treating physical addresses returned by the buddy allocator as pointers and dereferencing them directly. Once the MMU is enabled (which Limine does by default), physical addresses are no longer valid for direct access.
**Fix:** 
- Updated `arch/aarch64/src/paging.rs` to use `mm::phys_to_virt()` for all page table accesses.
- This ensures the kernel uses the Higher-Half Direct Map (HHDM) to modify page tables safely.

### 3. UART "Catch-22"
**Problem:** Early assembly debug prints were causing crashes, but removing them made debugging impossible.
**Cause:** The assembly code used the hardcoded physical address `0x09000000` for UART access. Limine enables the MMU, making this physical address invalid unless an identity map is present. However, Limine's page tables did not include an identity map for the UART by default.
**Fix:**
- Introduced a dynamic `UART_BASE` variable.
- Updated `kernel_main` to calculate the virtual address of the UART (`0x09000000 + hhdm_offset`) and update `UART_BASE`.
- Updated all assembly exception stubs and context switch code to load the UART address from `UART_BASE` dynamically.
- Implemented an identity mapping for the UART region in all new userspace page tables in `arch_alloc_page_table_root` to ensure continuity of debug output.

### 4. ELF Loader & Cache Maintenance
**Problem:** Userspace code was being loaded, but the CPU was likely executing stale cache data or garbage instructions.
**Cause:** AArch64 has non-coherent instruction and data caches. After the kernel copies the ELF segments to memory, it must explicitly clean the data cache and invalidate the instruction cache. The previous implementation was passing physical addresses to `DC CVAC`, which is incorrect in a virtual memory environment.
**Fix:**
- Corrected `elf/src/lib.rs` to pass HHDM virtual addresses to cache maintenance instructions.
- Added `IC IALLU` (Instruction Cache Invalidate All) and `ISB` (Instruction Synchronization Barrier) to ensure the CPU sees the newly loaded code.

### 5. Exception Vector Stabilization
**Problem:** The system was unstable when exceptions occurred, often resulting in recursive faults.
**Cause:** The exception vector table was bloated with complex Rust-based printing logic that performed UART access before the system was fully stable.
**Fix:**
- Streamlined `VBAR_EL1` initialization.
- Implemented a "brute-force" hex printer in assembly for emergency diagnostics (e.g., printing `ELR` and `ESR` directly before an `eret`).

## Current Status
The AArch64 boot process now successfully:
1.  Parses Limine boot information.
2.  Initializes the Buddy and Slab allocators.
3.  Configures the GIC and Virtual Timer.
4.  Identifies and extracts the userspace `init` binary from the CPIO initrd.
5.  Parses the ELF and maps segments into a new userspace `AddressSpace`.
6.  Performs cache maintenance.
7.  Prepares the kernel stack for the first user task.
8.  Reaches the `ret_to_user` trampoline.

**Blocking Issue:** The system terminates or hangs exactly at the `eret` instruction when attempting to jump into EL0 (userspace).

## Final Goal: Reaching the Shell Prompt
The final milestone for this task is to see the following sequence on the AArch64 serial console:
1.  `[INIT] Userspace init spawned with PID: 1`
2.  `[USERSPACE] LeandrOS Init (PID 1) starting...`
3.  `Launching shell via execve...`
4.  `LeandrOS Shell > ` (Prompt reached)

### Remaining Steps
- **Stack Alignment:** Verify that the userspace stack pointer provided to `eret` is 16-byte aligned (mandatory for AArch64).
- **SPSR_EL1 Configuration:** Ensure `SPSR_EL1` is correctly set to `0x00000000` (M[3:0] = 0 for EL0t, DAIF unmasked).
- **Syscall Bridge:** Validate that `SVC #0` from userspace correctly triggers the `exc_el0_sync` handler and dispatches to `syscall_dispatch`.
- **VFS Path:** Ensure the `init` task can successfully `execve("/bin/shell")` once the in-kernel VFS and servers are running.
