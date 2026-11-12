// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
//! C layouts and flag values the exported functions read and write, as
//! musl defines them for x86_64 (`arch/x86_64/kstat.h`,
//! `arch/generic/bits/*.h`). Host tests pin their sizes.

/// musl's `struct kstat` for x86_64.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Kstat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_nlink: u64,
    pub st_mode: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub pad0: u32,
    pub st_rdev: u64,
    pub st_size: i64,
    pub st_blksize: i64,
    pub st_blocks: i64,
    pub st_atime_sec: i64,
    pub st_atime_nsec: i64,
    pub st_mtime_sec: i64,
    pub st_mtime_nsec: i64,
    pub st_ctime_sec: i64,
    pub st_ctime_nsec: i64,
    pub unused: [i64; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Iovec {
    pub base: *mut u8,
    pub length: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Timespec {
    pub seconds: i64,
    pub nanoseconds: i64,
}

/// `struct utsname`: six fields of 65 bytes.
#[repr(C)]
pub struct Utsname {
    pub fields: [[u8; 65]; 6],
}

/// `struct winsize`.
#[repr(C)]
pub struct Winsize {
    pub rows: u16,
    pub columns: u16,
    pub x_pixels: u16,
    pub y_pixels: u16,
}

/// The fixed head of a `getdents64` record; the NUL-terminated name follows
/// and the record is padded to 8 bytes.
pub const DIRENT64_HEAD: usize = 19;

pub const S_IFIFO: u32 = 0o010_000;
pub const S_IFCHR: u32 = 0o020_000;
pub const S_IFDIR: u32 = 0o040_000;
pub const S_IFREG: u32 = 0o100_000;

pub const DT_CHR: u8 = 2;
pub const DT_DIR: u8 = 4;
pub const DT_REG: u8 = 8;

pub const O_ACCMODE: i32 = 0o3 | O_PATH;
pub const O_RDONLY: i32 = 0;
pub const O_WRONLY: i32 = 1;
pub const O_RDWR: i32 = 2;
pub const O_CREAT: i32 = 0o100;
pub const O_EXCL: i32 = 0o200;
pub const O_NOCTTY: i32 = 0o400;
pub const O_TRUNC: i32 = 0o1000;
pub const O_APPEND: i32 = 0o2000;
pub const O_NONBLOCK: i32 = 0o4000;
pub const O_DSYNC: i32 = 0o10000;
pub const O_SYNC: i32 = 0o4010000;
pub const O_LARGEFILE: i32 = 0o100000;
pub const O_DIRECTORY: i32 = 0o200000;
pub const O_NOFOLLOW: i32 = 0o400000;
pub const O_NOATIME: i32 = 0o1000000;
pub const O_CLOEXEC: i32 = 0o2000000;
pub const O_PATH: i32 = 0o10000000;
/// Status flags `F_SETFL` may change.
pub const O_SETTABLE: i32 = O_APPEND | O_NONBLOCK;

pub const AT_FDCWD: i32 = -100;
pub const AT_SYMLINK_NOFOLLOW: i32 = 0x100;
pub const AT_REMOVEDIR: i32 = 0x200;
pub const AT_EACCESS: i32 = 0x200;
pub const AT_NO_AUTOMOUNT: i32 = 0x800;
pub const AT_EMPTY_PATH: i32 = 0x1000;

pub const F_DUPFD: i32 = 0;
pub const F_GETFD: i32 = 1;
pub const F_SETFD: i32 = 2;
pub const F_GETFL: i32 = 3;
pub const F_SETFL: i32 = 4;
pub const F_DUPFD_CLOEXEC: i32 = 1030;
pub const F_GETLK: i32 = 5;
pub const F_SETLK: i32 = 6;
pub const F_SETLKW: i32 = 7;
pub const F_OFD_GETLK: i32 = 36;
pub const F_OFD_SETLK: i32 = 37;
pub const F_OFD_SETLKW: i32 = 38;
pub const F_RDLCK: i16 = 0;
pub const F_WRLCK: i16 = 1;
pub const F_UNLCK: i16 = 2;

/// musl's `struct flock` for x86_64.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Flock {
    pub l_type: i16,
    pub l_whence: i16,
    pub l_start: i64,
    pub l_len: i64,
    pub l_pid: i32,
}
pub const FD_CLOEXEC: i32 = 1;

pub const SEEK_SET: i32 = 0;
pub const SEEK_CUR: i32 = 1;
pub const SEEK_END: i32 = 2;

pub const PROT_READ: i32 = 1;
pub const PROT_WRITE: i32 = 2;
pub const PROT_EXEC: i32 = 4;
pub const MAP_SHARED: i32 = 0x01;
pub const MAP_PRIVATE: i32 = 0x02;
pub const MAP_SHARED_VALIDATE: i32 = 0x03;
pub const MAP_TYPE: i32 = 0x0f;
pub const MAP_FIXED: i32 = 0x10;
pub const MAP_ANONYMOUS: i32 = 0x20;
pub const MAP_NORESERVE: i32 = 0x4000;
pub const MAP_FIXED_NOREPLACE: i32 = 0x100000;

pub const R_OK: i32 = 4;
pub const W_OK: i32 = 2;
pub const X_OK: i32 = 1;

pub const CLOCK_REALTIME: i32 = 0;
pub const CLOCK_MONOTONIC: i32 = 1;
pub const CLOCK_MONOTONIC_RAW: i32 = 4;
pub const CLOCK_REALTIME_COARSE: i32 = 5;
pub const CLOCK_MONOTONIC_COARSE: i32 = 6;
pub const CLOCK_BOOTTIME: i32 = 7;
pub const TIMER_ABSTIME: i32 = 1;

pub const TIOCGWINSZ: u64 = 0x5413;

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    #[test]
    fn layouts_match_musl_x86_64() {
        assert_eq!(size_of::<Kstat>(), 144);
        assert_eq!(offset_of!(Kstat, st_mode), 24);
        assert_eq!(offset_of!(Kstat, st_rdev), 40);
        assert_eq!(offset_of!(Kstat, st_size), 48);
        assert_eq!(offset_of!(Kstat, st_blksize), 56);
        assert_eq!(offset_of!(Kstat, st_mtime_sec), 88);
        assert_eq!(size_of::<Iovec>(), 16);
        assert_eq!(size_of::<Timespec>(), 16);
        assert_eq!(size_of::<Utsname>(), 390);
        assert_eq!(size_of::<Winsize>(), 8);
    }
}
