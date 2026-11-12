// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
//! Error numbers, and how native and filesystem failures become them.
//!
//! The values are musl's (`arch/generic/bits/errno.h`), which are Linux's.
//! Every exported function returns a non-negative result or `-errno`, the
//! shape musl's `__syscall_ret` expects.

use deepwyrm_syscall::*;
use wyrmroot_fs_proto::Status;

/// One positive error number.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Errno(pub i32);

pub type Result<T> = core::result::Result<T, Errno>;

pub const EPERM: Errno = Errno(1);
pub const ENOENT: Errno = Errno(2);
pub const ESRCH: Errno = Errno(3);
pub const EINTR: Errno = Errno(4);
pub const EIO: Errno = Errno(5);
pub const E2BIG: Errno = Errno(7);
pub const EBADF: Errno = Errno(9);
pub const ECHILD: Errno = Errno(10);
pub const EAGAIN: Errno = Errno(11);
pub const ENOMEM: Errno = Errno(12);
pub const EACCES: Errno = Errno(13);
pub const EFAULT: Errno = Errno(14);
pub const EBUSY: Errno = Errno(16);
pub const EEXIST: Errno = Errno(17);
pub const ENODEV: Errno = Errno(19);
pub const ENOTDIR: Errno = Errno(20);
pub const EISDIR: Errno = Errno(21);
pub const EINVAL: Errno = Errno(22);
pub const ENFILE: Errno = Errno(23);
pub const EMFILE: Errno = Errno(24);
pub const ENOTTY: Errno = Errno(25);
pub const EFBIG: Errno = Errno(27);
pub const ENOSPC: Errno = Errno(28);
pub const ESPIPE: Errno = Errno(29);
pub const EROFS: Errno = Errno(30);
pub const EPIPE: Errno = Errno(32);
pub const ERANGE: Errno = Errno(34);
pub const ENAMETOOLONG: Errno = Errno(36);
pub const ENOSYS: Errno = Errno(38);
pub const ENOTEMPTY: Errno = Errno(39);
pub const EOVERFLOW: Errno = Errno(75);
pub const ENOTSUP: Errno = Errno(95);
pub const ETIMEDOUT: Errno = Errno(110);

/// The errno for a native status. Success has none; callers never pass it.
#[must_use]
pub fn from_status(status: DwStatus) -> Errno {
    match status {
        DW_STATUS_INVALID_ARGUMENT | DW_STATUS_BAD_STATE | DW_STATUS_WRONG_OBJECT_TYPE => EINVAL,
        DW_STATUS_BAD_HANDLE => EBADF,
        DW_STATUS_ACCESS_DENIED => EACCES,
        DW_STATUS_NOT_FOUND => ENOENT,
        DW_STATUS_ALREADY_EXISTS => EEXIST,
        DW_STATUS_WOULD_BLOCK | DW_STATUS_NO_RESOURCES => EAGAIN,
        DW_STATUS_TIMED_OUT => ETIMEDOUT,
        DW_STATUS_INTERRUPTED => EINTR,
        DW_STATUS_PEER_CLOSED => EPIPE,
        DW_STATUS_NO_MEMORY => ENOMEM,
        DW_STATUS_NOT_SUPPORTED => ENOTSUP,
        DW_STATUS_BUFFER_TOO_SMALL => ERANGE,
        DW_STATUS_BAD_ADDRESS => EFAULT,
        _ => EIO,
    }
}

/// The errno for a WRFS or WRNS status other than OK.
#[must_use]
pub const fn from_fs(status: Status) -> Errno {
    match status {
        Status::Ok => EIO,
        Status::NotFound => ENOENT,
        Status::Exists => EEXIST,
        Status::NotDirectory => ENOTDIR,
        Status::IsDirectory => EISDIR,
        Status::AccessDenied => EACCES,
        Status::ReadOnly => EROFS,
        Status::NoSpace => ENOSPC,
        Status::InvalidPath => ENAMETOOLONG,
        Status::Invalid => EINVAL,
        Status::NotSupported => ENOTSUP,
        Status::IoError => EIO,
        Status::NotEmpty => ENOTEMPTY,
        Status::NoResources => ENFILE,
        Status::Busy => EBUSY,
    }
}

/// Folds a result into the exported `value or -errno` form.
#[must_use]
pub fn ret(result: Result<usize>) -> isize {
    match result {
        Ok(value) => isize::try_from(value).unwrap_or(isize::MAX),
        Err(Errno(code)) => -(code as isize),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_statuses_map_to_their_posix_meaning() {
        assert_eq!(from_status(DW_STATUS_BAD_HANDLE), EBADF);
        assert_eq!(from_status(DW_STATUS_NOT_FOUND), ENOENT);
        assert_eq!(from_status(DW_STATUS_WOULD_BLOCK), EAGAIN);
        assert_eq!(from_status(DW_STATUS_PEER_CLOSED), EPIPE);
        assert_eq!(from_status(DW_STATUS_NO_MEMORY), ENOMEM);
        assert_eq!(from_status(DW_STATUS_BAD_ADDRESS), EFAULT);
        assert_eq!(from_status(DW_STATUS_NOT_SUPPORTED), ENOTSUP);
        // A status the layer does not know is an I/O error, never success.
        assert_eq!(from_status(DwStatus(-999)), EIO);
    }

    #[test]
    fn filesystem_statuses_map_to_their_posix_meaning() {
        assert_eq!(from_fs(Status::NotFound), ENOENT);
        assert_eq!(from_fs(Status::Exists), EEXIST);
        assert_eq!(from_fs(Status::NotDirectory), ENOTDIR);
        assert_eq!(from_fs(Status::IsDirectory), EISDIR);
        assert_eq!(from_fs(Status::ReadOnly), EROFS);
        assert_eq!(from_fs(Status::NotEmpty), ENOTEMPTY);
        assert_eq!(from_fs(Status::NoSpace), ENOSPC);
        // OK is never an error; asking for one is a protocol failure.
        assert_eq!(from_fs(Status::Ok), EIO);
    }

    #[test]
    fn results_fold_to_value_or_negative_errno() {
        assert_eq!(ret(Ok(7)), 7);
        assert_eq!(ret(Err(EBADF)), -9);
        assert_eq!(ret(Ok(usize::MAX)), isize::MAX);
    }
}
