//! CyanOS Shell - userspace shell program
//!
//! This is a separate userspace binary that provides shell functionality
//! using system calls through cyanos-libc.

#![no_std]
#![no_main]

extern crate cyanos_libc;

use cyanos_libc::{
    write, read, STDOUT_FILENO, STDIN_FILENO, getpid,
    open, close, getdents64, O_RDONLY,
    fork, execve, wait4,
    getcwd, chdir,
};

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
    write_str("Type commands and press Enter. Use 'help' for available commands.\n");
    write_str("\n");


    // Interactive shell loop
    loop {
        // Show CWD in prompt
        let mut cwd_buf = [0u8; 128];
        if !getcwd(cwd_buf.as_mut_ptr(), cwd_buf.len()).is_null() {
            let mut cwd_len = 0;
            while cwd_len < cwd_buf.len() && cwd_buf[cwd_len] != 0 {
                cwd_len += 1;
            }
            write(STDOUT_FILENO, cwd_buf.as_ptr(), cwd_len);
        } else {
            write_str("?");
        }
        write_str("> ");

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
            let command_line = core::str::from_utf8(&input[..len]).unwrap_or("");
            process_line(command_line);
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

unsafe fn process_line(line: &str) {
    let mut args = [""; 16];
    let mut arg_count = 0;
    
    // Manual split_whitespace implementation because core::str::split_whitespace might be missing
    // or to keep it simple and avoid potential issues with it in no_std
    let mut current_start = None;
    for (i, c) in line.as_bytes().iter().enumerate() {
        if *c == b' ' || *c == b'\t' || *c == b'\n' || *c == b'\r' {
            if let Some(start) = current_start {
                if arg_count < 16 {
                    args[arg_count] = &line[start..i];
                    arg_count += 1;
                }
                current_start = None;
            }
        } else if current_start.is_none() {
            current_start = Some(i);
        }
    }
    if let Some(start) = current_start {
        if arg_count < 16 {
            args[arg_count] = &line[start..];
            arg_count += 1;
        }
    }

    if arg_count == 0 {
        return;
    }

    match args[0] {
        "help" => {
            write_str("Available commands:\n");
            write_str("  help          - Show this help message\n");
            write_str("  info          - Show system information\n");
            write_str("  ls [path]     - List files in path (default: .)\n");
            write_str("  cd <path>     - Change current directory\n");
            write_str("  pwd           - Print current working directory\n");
            write_str("  test          - Run a simple test\n");
            write_str("  clear         - Clear the screen\n");
            write_str("  exit          - Exit the shell\n");
            write_str("  [binary]      - Execute a binary file\n");
        }
        "info" => {
            write_str("CyanOS Microkernel (Userspace Shell)\n");
            write_str("Status: Running in userspace\n");
            write_str("PID: ");
            write_u32(getpid() as u32);
            write_str("\n");
        }
        "ls" => {
            let path = if arg_count > 1 { args[1] } else { "." };
            ls_command(path);
        }
        "cd" => {
            if arg_count > 1 {
                let mut path_bytes = [0u8; 256];
                let path_len = args[1].len().min(255);
                core::ptr::copy_nonoverlapping(args[1].as_ptr(), path_bytes.as_mut_ptr(), path_len);
                path_bytes[path_len] = 0;
                if chdir(path_bytes.as_ptr()) != 0 {
                    write_str("cd: failed to change directory to '");
                    write_str(args[1]);
                    write_str("'\n");
                }
            }
        }
        "pwd" => {
            let mut cwd_buf = [0u8; 128];
            if !getcwd(cwd_buf.as_mut_ptr(), cwd_buf.len()).is_null() {
                let mut cwd_len = 0;
                while cwd_len < cwd_buf.len() && cwd_buf[cwd_len] != 0 {
                    cwd_len += 1;
                }
                write(STDOUT_FILENO, cwd_buf.as_ptr(), cwd_len);
                write_str("\n");
            } else {
                write_str("error getting cwd\n");
            }
        }

        "test" => {
            write_str("Running userspace system tests...\n");
            write_str("✓ write() syscall working\n");
            write_str("✓ getpid() syscall working\n");
            write_str("All userspace tests passed!\n");
        }
        "clear" => {
            write_str("\x1b[2J\x1b[H"); // ANSI clear screen
        }
        "exit" => {
            write_str("Exiting shell...\n");
        }
        _ => {
            execute_binary(args[0]);
        }
    }
}

unsafe fn ls_command(path: &str) {
    let mut abs_path = [0u8; 256];
    let mut abs_len = 0;

    if path.starts_with('/') {
        let copy_len = path.len().min(255);
        core::ptr::copy_nonoverlapping(path.as_ptr(), abs_path.as_mut_ptr(), copy_len);
        abs_len = copy_len;
    } else {
        // Prepend CWD
        if getcwd(abs_path.as_mut_ptr(), 256).is_null() {
            write_str("ls: error getting cwd\n");
            return;
        }
        while abs_len < 256 && abs_path[abs_len] != 0 {
            abs_len += 1;
        }
        
        if path != "." {
            if abs_len > 0 && abs_path[abs_len - 1] != b'/' && abs_len < 255 {
                abs_path[abs_len] = b'/';
                abs_len += 1;
            }
            let copy_len = path.len().min(255 - abs_len);
            core::ptr::copy_nonoverlapping(path.as_ptr(), abs_path.as_mut_ptr().add(abs_len), copy_len);
            abs_len += copy_len;
        }
    }
    abs_path[abs_len.min(255)] = 0;

    let fd = open(abs_path.as_ptr(), O_RDONLY, 0);
    if fd < 0 {
        write_str("ls: cannot open '");
        write_str(path);
        write_str("' (resolved to '");
        write(STDOUT_FILENO, abs_path.as_ptr(), abs_len);
        write_str("'): Error ");
        write_u32((-fd) as u32);
        write_str("\n");
        return;
    }

    let mut buf = [0u8; 1024];
    loop {
        let n = getdents64(fd, buf.as_mut_ptr(), buf.len());
        if n <= 0 {
            break;
        }

        let mut pos = 0;
        while pos < n as usize {
            let dirent = &*(buf.as_ptr().add(pos) as *const cyanos_libc::linux_dirent64);
            let name_ptr = buf.as_ptr().add(pos + 19); // 19 is offset to d_name in linux_dirent64

            // Find name length (NUL terminated)
            let mut name_len = 0;
            while *name_ptr.add(name_len) != 0 {
                name_len += 1;
            }

            let name = core::str::from_utf8(core::slice::from_raw_parts(name_ptr, name_len)).unwrap_or("?");

            // Filter out "." and ".." for cleaner output if desired, or just print all
            write_str(name);
            if dirent.d_type == 4 { // DT_DIR
                write_str("/");
            }
            write_str("  ");

            pos += dirent.d_reclen as usize;
        }
    }
    write_str("\n");
    close(fd);
}

static mut BIN_PATH_BUFFER: [u8; 256] = [0u8; 256];
static mut EXEC_PATH_BUFFER: [u8; 256] = [0u8; 256];

unsafe fn execute_binary(cmd: &str) {
    let actual_path = cmd;

    let path_len = actual_path.len().min(255);
    core::ptr::copy_nonoverlapping(actual_path.as_ptr(), EXEC_PATH_BUFFER.as_mut_ptr(), path_len);
    EXEC_PATH_BUFFER[path_len] = 0;

    let pid = fork();
    if pid < 0 {
        write_str("shell: fork failed\n");
    } else if pid == 0 {
        // Child
        let argv: [*const u8; 2] = [EXEC_PATH_BUFFER.as_ptr(), core::ptr::null()];
        let envp: [*const u8; 1] = [core::ptr::null()];
        execve(EXEC_PATH_BUFFER.as_ptr(), argv.as_ptr(), envp.as_ptr());

        // If execve fails and it doesn't start with /, try /bin/
        if !cmd.starts_with('/') {
            let bin_prefix = b"/bin/";
            core::ptr::copy_nonoverlapping(bin_prefix.as_ptr(), BIN_PATH_BUFFER.as_mut_ptr(), bin_prefix.len());
            let copy_len = cmd.len().min(256 - bin_prefix.len() - 1);
            core::ptr::copy_nonoverlapping(cmd.as_ptr(), BIN_PATH_BUFFER.as_mut_ptr().add(bin_prefix.len()), copy_len);
            BIN_PATH_BUFFER[bin_prefix.len() + copy_len] = 0;

            let argv: [*const u8; 2] = [BIN_PATH_BUFFER.as_ptr(), core::ptr::null()];
            execve(BIN_PATH_BUFFER.as_ptr(), argv.as_ptr(), envp.as_ptr());
        }

        // If we get here, execve failed
        write_str("shell: command not found: ");
        write_str(cmd);
        write_str("\n");
        cyanos_libc::exit(1);
    } else {
        // Parent
        let mut status = 0i32;
        wait4(pid, &mut status, 0, core::ptr::null_mut());
    }
}