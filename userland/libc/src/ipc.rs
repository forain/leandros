//! IPC subsystem wrappers for user-space.

use crate::syscall::{self, nr};
use ipc::Message;

/// Send a message to a port (non-blocking).
pub unsafe fn ipc_send(port: u32, msg: &Message) -> isize {
    syscall::syscall2(nr::IPC_SEND, port as usize, msg as *const Message as usize)
}

/// Receive a message from a port (blocking).
pub unsafe fn ipc_recv(port: u32, msg: &mut Message) -> isize {
    syscall::syscall2(nr::IPC_RECV, port as usize, msg as *mut Message as usize)
}

/// Synchronous call: send a message and wait for a reply on the same port.
/// The kernel automatically manages the reply port for the task.
pub unsafe fn ipc_call(port: u32, msg: &mut Message) -> isize {
    syscall::syscall2(nr::IPC_CALL, port as usize, msg as *mut Message as usize)
}
