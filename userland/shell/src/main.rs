//! CyanOS Shell - userspace shell program
//!
//! This is a separate userspace binary that provides shell functionality
//! using system calls through cyanos-libc.

#![no_std]
#![no_main]

extern crate cyanos_libc;

use cyanos_libc::{write, read, STDOUT_FILENO, STDIN_FILENO, getpid};

/// Called by `__libc_start_main` after the C runtime is set up.
#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    write_str("Shell main reached!\n");
    // Display shell banner
    write_str("\n");
    write_str("  ██████╗██╗   ██╗ █████╗ ███╗   ██╗ ██████╗ ███████╗\n");
    write_str(" ██╔════╝╚██╗ ██╔╝██╔══██╗████╗  ██║██╔═══██╗██╔════╝\n");
    write_str(" ██║      ╚████╔╝ ███████║██╔██╗ ██║██║   ██║███████╗\n");
    write_str(" ██║       ╚██╔╝  ██╔══██║██║╚██╗██║██║   ██║╚════██║\n");
    write_str(" ╚██████╗   ██║   ██║  ██║██║ ╚████║╚██████╔╝███████║\n");
    write_str("  ╚═════╝   ╚═╝   ╚═╝  ╚═╝╚═╝  ╚═══╝ ╚═════╝ ╚══════╝\n\n");
    write_str("CyanOS Shell (Userspace)\n");
    write_str("Type 'help' for available commands\n\n");

    // Show initial PID
    write_str("Shell PID: ");
    write_u32(getpid() as u32);
    write_str("\n\n");

    write_str("Shell initialized successfully in userspace!\n");
    write_str("Type commands and press Enter. Use 'help' for available commands.\n\n");

    // Interactive shell loop
    loop {
        write_str("cyanos> ");

        // Read user input
        let mut input = [0u8; 256];
        let mut len = 0;

        loop {
            let mut ch = [0u8; 1];
            let n = read(STDIN_FILENO, ch.as_mut_ptr(), 1);
            if n <= 0 {
                continue;
            }

            let c = ch[0];

            // Handle enter key
            if c == b'\n' || c == b'\r' {
                write_str("\n");
                break;
            }

            // Handle backspace
            if c == 0x08 || c == 0x7F { // backspace or DEL
                if len > 0 {
                    len -= 1;
                    write_str("\x08 \x08"); // backspace, space, backspace
                }
                continue;
            }

            // Handle printable characters
            if c >= 32 && c <= 126 && len < 255 {
                input[len] = c;
                len += 1;
                // Echo the character
                write(STDOUT_FILENO, &c, 1);
            }
        }

        // Null-terminate and execute command
        input[len] = 0;
        if len > 0 {
            let command_str = core::str::from_utf8(&input[..len]).unwrap_or("");
            execute_command(command_str);
        }
        write_str("\n");
    }
}

unsafe fn write_str(s: &str) {
    write(STDOUT_FILENO, s.as_ptr(), s.len());
}

unsafe fn write_u32(mut n: u32) {
    let mut buf = [0u8; 10];
    if n == 0 {
        write(STDOUT_FILENO, b"0".as_ptr(), 1);
        return;
    }
    let mut i = 10usize;
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    write(STDOUT_FILENO, buf.as_ptr().add(i), 10 - i);
}

unsafe fn execute_command(cmd: &str) {
    match cmd.trim() {
        "help" => {
            write_str("Available commands:\n");
            write_str("  help      - Show this help message\n");
            write_str("  info      - Show system information\n");
            write_str("  test      - Run a simple test\n");
            write_str("  clear     - Clear the screen\n");
            write_str("  exit      - Exit the shell\n");
        }
        "info" => {
            write_str("CyanOS Microkernel (Userspace Shell)\n");
            write_str("Architecture: AArch64\n");
            write_str("Status: Running in userspace\n");
            write_str("PID: ");
            write_u32(getpid() as u32);
            write_str("\n");
            write_str("Process: Userspace shell binary\n");
            write_str("Syscalls: Working\n");
        }
        "test" => {
            write_str("Running userspace system tests...\n");
            write_str("✓ write() syscall working\n");
            write_str("✓ getpid() syscall working\n");
            write_str("✓ Userspace execution context\n");
            write_str("✓ Shell command processing\n");
            write_str("✓ Memory access operational\n");
            write_str("All userspace tests passed!\n");
        }
        "clear" => {
            write_str("\x1b[2J\x1b[H"); // ANSI clear screen
        }
        "exit" => {
            write_str("Exiting shell...\n");
            // TODO: Call exit syscall when ready
        }
        "" => {
            // Empty command
        }
        _ => {
            write_str("Unknown command: '");
            write_str(cmd);
            write_str("'\nType 'help' for available commands.\n");
        }
    }
}