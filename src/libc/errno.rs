/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0.
 * If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `errno.h`

use crate::dyld::FunctionExports;
use crate::export_c_func;
use crate::mem::{ConstPtr, MutPtr};
use crate::Environment;
use std::io::Write;

// Standard errno constants (iOS/macOS BSD)
pub const EPERM: i32 = 1;
pub const ENOENT: i32 = 2;
pub const ESRCH: i32 = 3;
pub const EINTR: i32 = 4;
pub const EIO: i32 = 5;
pub const ENXIO: i32 = 6;
pub const E2BIG: i32 = 7;
pub const ENOEXEC: i32 = 8;
pub const EBADF: i32 = 9;
pub const ECHILD: i32 = 10;
pub const EDEADLK: i32 = 11;
pub const ENOMEM: i32 = 12;
pub const EACCES: i32 = 13;
pub const EFAULT: i32 = 14;
pub const ENOTBLK: i32 = 15;
pub const EBUSY: i32 = 16;
pub const EEXIST: i32 = 17;
pub const EXDEV: i32 = 18;
pub const ENODEV: i32 = 19;
pub const ENOTDIR: i32 = 20;
pub const EISDIR: i32 = 21;
pub const EINVAL: i32 = 22;
pub const ENFILE: i32 = 23;
pub const EMFILE: i32 = 24;
pub const ENOTTY: i32 = 25;
pub const ETXTBSY: i32 = 26;
pub const EFBIG: i32 = 27;
pub const ENOSPC: i32 = 28;
pub const ESPIPE: i32 = 29;
pub const EROFS: i32 = 30;
pub const EMLINK: i32 = 31;
pub const EPIPE: i32 = 32;
pub const EDOM: i32 = 33;
pub const ERANGE: i32 = 34;
pub const EAGAIN: i32 = 35;
pub const EINPROGRESS: i32 = 36;
pub const EALREADY: i32 = 37;
pub const ENOTSOCK: i32 = 38;
pub const EDESTADDRREQ: i32 = 39;
pub const EMSGSIZE: i32 = 40;
pub const EPROTOTYPE: i32 = 41;
pub const ENOPROTOOPT: i32 = 42;
pub const EPROTONOSUPPORT: i32 = 43;
pub const ESOCKTNOSUPPORT: i32 = 44;
pub const ENOTSUP: i32 = 45;
pub const EPFNOSUPPORT: i32 = 46;
pub const EAFNOSUPPORT: i32 = 47;
pub const EADDRINUSE: i32 = 48;
pub const EADDRNOTAVAIL: i32 = 49;
pub const ENETDOWN: i32 = 50;
pub const ENETUNREACH: i32 = 51;
pub const ENETRESET: i32 = 52;
pub const ECONNABORTED: i32 = 53;
pub const ECONNRESET: i32 = 54;
pub const ENOBUFS: i32 = 55;
pub const EISCONN: i32 = 56;
pub const ENOTCONN: i32 = 57;
pub const ESHUTDOWN: i32 = 58;
pub const ETOOMANYREFS: i32 = 59;
pub const ETIMEDOUT: i32 = 60;
pub const ECONNREFUSED: i32 = 61;
pub const ELOOP: i32 = 62;
pub const ENAMETOOLONG: i32 = 63;
pub const EHOSTDOWN: i32 = 64;
pub const EHOSTUNREACH: i32 = 65;
pub const ENOTEMPTY: i32 = 66;
pub const EPROCLIM: i32 = 67;
pub const EUSERS: i32 = 68;
pub const EDQUOT: i32 = 69;
pub const ESTALE: i32 = 70;
pub const EREMOTE: i32 = 71;
pub const EBADRPC: i32 = 72;
pub const ERPCMISMATCH: i32 = 73;
pub const EPROGUNAVAIL: i32 = 74;
pub const EPROGMISMATCH: i32 = 75;
pub const EPROCUNAVAIL: i32 = 76;
pub const ENOLCK: i32 = 77;
pub const ENOSYS: i32 = 78;
pub const EFTYPE: i32 = 79;
pub const EAUTH: i32 = 80;
pub const ENEEDAUTH: i32 = 81;
pub const EPWROFF: i32 = 82;
pub const EDEVERR: i32 = 83;
pub const EOVERFLOW: i32 = 84;
pub const EBADEXEC: i32 = 85;
pub const EBADARCH: i32 = 86;
pub const ESHLIBVERS: i32 = 87;
pub const EBADMACHO: i32 = 88;
pub const ECANCELED: i32 = 89;
pub const EIDRM: i32 = 90;
pub const ENOMSG: i32 = 91;
pub const EILSEQ: i32 = 92;
pub const ENOATTR: i32 = 93;
pub const EBADMSG: i32 = 94;
pub const EMULTIHOP: i32 = 95;
pub const ENODATA: i32 = 96;
pub const ENOLINK: i32 = 97;
pub const ENOSR: i32 = 98;
pub const ENOSTR: i32 = 99;
pub const EPROTO: i32 = 100;
pub const ETIME: i32 = 101;
pub const EOPNOTSUPP: i32 = 102;
pub const ENOPOLICY: i32 = 103;
pub const ENOTRECOVERABLE: i32 = 104;
pub const EOWNERDEAD: i32 = 105;
pub const EQFULL: i32 = 106;
pub const ELAST: i32 = 106;

#[derive(Default)]
pub struct State {
    errnos: std::collections::HashMap<crate::ThreadId, MutPtr<i32>>,
    strings_cache: std::collections::HashMap<i32, ConstPtr<u8>>,
}

impl State {
    fn errno_ptr_for_thread(
        &mut self,
        mem: &mut crate::mem::Mem,
        thread: crate::ThreadId,
    ) -> MutPtr<i32> {
        *self
            .errnos
            .entry(thread)
            .or_insert_with(|| mem.alloc_and_write(0i32))
    }

    pub fn set_errno_for_thread(
        &mut self,
        mem: &mut crate::mem::Mem,
        thread: crate::ThreadId,
        val: i32,
    ) {
        let ptr = self.errno_ptr_for_thread(mem, thread);
        mem.write(ptr, val);
    }
}

/// Helper function, not a part of libc errno
pub fn set_errno(env: &mut Environment, val: i32) {
    env.libc_state
        .errno
        .set_errno_for_thread(&mut env.mem, env.current_thread, val);
}

/// Helper to read the current thread's `errno`, mirroring the C `errno`
/// macro. Not a libc export — used by other host code (e.g. `mkstemp`) to
/// decide whether to retry after a failed I/O call.
pub fn get_errno(env: &mut Environment) -> i32 {
    let ptr = env
        .libc_state
        .errno
        .errno_ptr_for_thread(&mut env.mem, env.current_thread);
    env.mem.read(ptr)
}

fn __error(env: &mut Environment) -> MutPtr<i32> {
    env.libc_state
        .errno
        .errno_ptr_for_thread(&mut env.mem, env.current_thread)
}

fn perror(env: &mut Environment, s: ConstPtr<u8>) {
    let errno_ptr = __error(env);
    let str_error = strerror(env, env.mem.read(errno_ptr));
    let errno_msg = format!("{}\n", env.mem.cstr_at_utf8(str_error).unwrap());

    let msg = if !s.is_null() {
        if let Ok(str) = env.mem.cstr_at_utf8(s) {
            format!("{str}: {errno_msg}")
        } else {
            errno_msg.to_string()
        }
    } else {
        errno_msg.to_string()
    };

    let _ = crate::guest_console::stderr().write_all(msg.as_bytes());
}

fn strerror(env: &mut Environment, err_num: i32) -> ConstPtr<u8> {
    if let Some(&c_str) = env.libc_state.errno.strings_cache.get(&err_num) {
        c_str
    } else {
        let str = match err_num {
            0 => "Undefined error: 0",
            EPERM => "Operation not permitted",
            ENOENT => "No such file or directory",
            ESRCH => "No such process",
            EINTR => "Interrupted system call",
            EIO => "Input/output error",
            ENXIO => "No such device or address",
            E2BIG => "Argument list too long",
            ENOEXEC => "Exec format error",
            EBADF => "Bad file descriptor",
            ECHILD => "No child processes",
            EDEADLK => "Resource deadlock avoided",
            ENOMEM => "Cannot allocate memory",
            EACCES => "Permission denied",
            EFAULT => "Bad address",
            ENOTBLK => "Block device required",
            EBUSY => "Resource busy",
            EEXIST => "File exists",
            EXDEV => "Cross-device link",
            ENODEV => "Operation not supported by device",
            ENOTDIR => "Not a directory",
            EISDIR => "Is a directory",
            EINVAL => "Invalid argument",
            ENFILE => "Too many open files in system",
            EMFILE => "Too many open files",
            ENOTTY => "Inappropriate ioctl for device",
            ETXTBSY => "Text file busy",
            EFBIG => "File too large",
            ENOSPC => "No space left on device",
            ESPIPE => "Illegal seek",
            EROFS => "Read-only file system",
            EMLINK => "Too many links",
            EPIPE => "Broken pipe",
            EDOM => "Numerical argument out of domain",
            ERANGE => "Result too large",
            EAGAIN => "Resource temporarily unavailable",
            EINPROGRESS => "Operation now in progress",
            EALREADY => "Operation already in progress",
            ENOTSOCK => "Socket operation on non-socket",
            EDESTADDRREQ => "Destination address required",
            EMSGSIZE => "Message too long",
            EPROTOTYPE => "Protocol wrong type for socket",
            ENOPROTOOPT => "Protocol not available",
            EPROTONOSUPPORT => "Protocol not supported",
            ESOCKTNOSUPPORT => "Socket type not supported",
            ENOTSUP => "Operation not supported",
            EPFNOSUPPORT => "Protocol family not supported",
            EAFNOSUPPORT => "Address family not supported by protocol family",
            EADDRINUSE => "Address already in use",
            EADDRNOTAVAIL => "Can't assign requested address",
            ENETDOWN => "Network is down",
            ENETUNREACH => "Network is unreachable",
            ENETRESET => "Network dropped connection on reset",
            ECONNABORTED => "Software caused connection abort",
            ECONNRESET => "Connection reset by peer",
            ENOBUFS => "No buffer space available",
            EISCONN => "Socket is already connected",
            ENOTCONN => "Socket is not connected",
            ESHUTDOWN => "Can't send after socket shutdown",
            ETOOMANYREFS => "Too many references: can't splice",
            ETIMEDOUT => "Operation timed out",
            ECONNREFUSED => "Connection refused",
            ELOOP => "Too many levels of symbolic links",
            ENAMETOOLONG => "File name too long",
            EHOSTDOWN => "Host is down",
            EHOSTUNREACH => "No route to host",
            ENOTEMPTY => "Directory not empty",
            EPROCLIM => "Too many processes",
            EUSERS => "Too many users",
            EDQUOT => "Disc quota exceeded",
            ESTALE => "Stale NFS file handle",
            EREMOTE => "Too many levels of remote in path",
            EBADRPC => "RPC struct is bad",
            ERPCMISMATCH => "RPC version wrong",
            EPROGUNAVAIL => "RPC program not available",
            EPROGMISMATCH => "Program version wrong",
            EPROCUNAVAIL => "Bad procedure for program",
            ENOLCK => "No locks available",
            ENOSYS => "Function not implemented",
            EFTYPE => "Inappropriate file type or format",
            EAUTH => "Authentication error",
            ENEEDAUTH => "Need authenticator",
            EPWROFF => "Device power is off",
            EDEVERR => "Device error",
            EOVERFLOW => "Value too large to be stored in data type",
            EBADEXEC => "Bad executable (or shared library)",
            EBADARCH => "Bad CPU type in executable",
            ESHLIBVERS => "Shared library version mismatch",
            EBADMACHO => "Malformed Mach-o file",
            ECANCELED => "Operation canceled",
            EIDRM => "Identifier removed",
            ENOMSG => "No message of desired type",
            EILSEQ => "Illegal byte sequence",
            ENOATTR => "Attribute not found",
            EBADMSG => "Bad message",
            EMULTIHOP => "EMULTIHOP (Reserved)",
            ENODATA => "No message available on STREAM",
            ENOLINK => "ENOLINK (Reserved)",
            ENOSR => "No STREAM resources",
            ENOSTR => "Not a STREAM",
            EPROTO => "Protocol error",
            ETIME => "STREAM ioctl timeout",
            EOPNOTSUPP => "Operation not supported on socket",
            ENOPOLICY => "Policy not found",
            ENOTRECOVERABLE => "State not recoverable",
            EOWNERDEAD => "Previous owner died",
            EQFULL => "Interface output queue is full",
            _ => "Unknown error",
        };

        let new_c_str = env.mem.alloc_and_write_cstr(str.as_bytes()).cast_const();
        env.libc_state
            .errno
            .strings_cache
            .insert(err_num, new_c_str);

        new_c_str
    }
}

pub const FUNCTIONS: FunctionExports = &[
    export_c_func!(__error()),
    export_c_func!(perror(_)),
    export_c_func!(strerror(_)),
];
