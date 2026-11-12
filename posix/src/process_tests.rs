// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
//! Host tests of the POSIX operations against the fake kernel and
//! filesystem.

use crate::errno::*;
use crate::process::Process;
use crate::testing::{Fake, FsNode, NAMESPACE, ROOT};
use crate::types::*;
use deepwyrm_syscall::*;

fn setup() -> (Fake, Box<Process>) {
    let fake = Fake::with_filesystem();
    let mut process = Box::new(Process::new());
    process.namespace = Some(NAMESPACE);
    process.self_root = Some(ROOT);
    (fake, process)
}

fn open(fake: &mut Fake, process: &mut Process, path: &str, flags: i32) -> Result<i32> {
    process
        .openat(fake, AT_FDCWD, path.as_bytes(), flags)
        .map(|fd| fd as i32)
}

fn names(buffer: &[u8]) -> Vec<(String, u8)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < buffer.len() {
        let length = u16::from_ne_bytes([buffer[at + 16], buffer[at + 17]]) as usize;
        let kind = buffer[at + 18];
        let name = &buffer[at + DIRENT64_HEAD..at + length];
        let end = name.iter().position(|&byte| byte == 0).unwrap();
        out.push((String::from_utf8(name[..end].to_vec()).unwrap(), kind));
        assert_eq!(length % 8, 0);
        at += length;
    }
    out
}

#[test]
fn a_file_is_created_written_read_back_and_synced_on_close() {
    let (mut fake, mut process) = setup();
    let fd = open(
        &mut fake,
        &mut process,
        "/notes.txt",
        O_RDWR | O_CREAT | O_TRUNC,
    )
    .unwrap();
    // 0-2 are not installed here, so the first descriptor is 0.
    assert_eq!(fd, 0);
    let text = vec![b'z'; 20_000];
    assert_eq!(process.write(&mut fake, fd, &text), Ok(20_000));
    assert_eq!(fake.file("/notes.txt").unwrap().len(), 20_000);
    assert_eq!(process.lseek(&mut fake, fd, 0, SEEK_SET), Ok(0));
    let mut buffer = vec![0u8; 20_000];
    // One read returns at most one WRFS request's worth.
    assert_eq!(process.read(&mut fake, fd, &mut buffer), Ok(8192));
    assert_eq!(process.lseek(&mut fake, fd, -8, SEEK_END), Ok(19_992));
    assert_eq!(process.read(&mut fake, fd, &mut buffer), Ok(8));
    assert_eq!(process.read(&mut fake, fd, &mut buffer), Ok(0));
    let mut stat = Kstat::default();
    process.fstat(&mut fake, fd, &mut stat).unwrap();
    assert_eq!(stat.st_mode & S_IFREG, S_IFREG);
    assert_eq!(stat.st_size, 20_000);
    let syncs = fake.syncs;
    assert_eq!(process.close(&mut fake, fd), Ok(0));
    assert_eq!(fake.syncs, syncs + 1);
    assert_eq!(process.read(&mut fake, fd, &mut buffer), Err(EBADF));
}

#[test]
fn append_writes_at_the_end_and_access_modes_are_enforced() {
    let (mut fake, mut process) = setup();
    fake.put("/log", FsNode::File(b"one\n".to_vec()));
    let fd = open(&mut fake, &mut process, "/log", O_WRONLY | O_APPEND).unwrap();
    assert_eq!(process.lseek(&mut fake, fd, 0, SEEK_SET), Ok(0));
    assert_eq!(process.write(&mut fake, fd, b"two\n"), Ok(4));
    assert_eq!(fake.file("/log").unwrap(), b"one\ntwo\n");
    assert_eq!(process.read(&mut fake, fd, &mut [0u8; 4]), Err(EBADF));
    let reader = open(&mut fake, &mut process, "/log", O_RDONLY).unwrap();
    assert_eq!(process.write(&mut fake, reader, b"x"), Err(EBADF));
}

#[test]
fn open_reports_missing_existing_and_directory_errors() {
    let (mut fake, mut process) = setup();
    fake.put("/d", FsNode::Directory);
    fake.put("/f", FsNode::File(Vec::new()));
    assert_eq!(
        open(&mut fake, &mut process, "/nope", O_RDONLY),
        Err(ENOENT)
    );
    assert_eq!(
        open(&mut fake, &mut process, "/f", O_RDWR | O_CREAT | O_EXCL),
        Err(EEXIST)
    );
    assert_eq!(open(&mut fake, &mut process, "/d", O_RDWR), Err(EISDIR));
    assert_eq!(
        open(&mut fake, &mut process, "/f", O_RDONLY | O_DIRECTORY),
        Err(ENOTDIR)
    );
    assert_eq!(open(&mut fake, &mut process, "/f/", O_RDONLY), Err(ENOTDIR));
    assert_eq!(
        open(&mut fake, &mut process, "/missing/f", O_WRONLY | O_CREAT),
        Err(ENOENT)
    );
    let directory = open(&mut fake, &mut process, "/d", O_RDONLY).unwrap();
    assert_eq!(
        process.read(&mut fake, directory, &mut [0u8; 4]),
        Err(EISDIR)
    );
    // A failed open leaves no Channel behind.
    let opened = fake.closed().len();
    assert_eq!(
        open(&mut fake, &mut process, "/nope", O_RDONLY),
        Err(ENOENT)
    );
    assert_eq!(fake.closed().len(), opened + 1);
}

#[test]
fn a_listing_spans_pages_and_small_buffers() {
    let (mut fake, mut process) = setup();
    for name in ["a", "b", "c", "d", "e"] {
        fake.put(&format!("/dir/{name}"), FsNode::File(Vec::new()));
    }
    fake.put("/dir", FsNode::Directory);
    fake.put("/dir/sub", FsNode::Directory);
    let fd = open(&mut fake, &mut process, "/dir", O_RDONLY | O_DIRECTORY).unwrap();
    let mut seen = Vec::new();
    // Room for one record at a time: every page is resumed mid-way.
    let mut buffer = [0u8; 32];
    loop {
        let used = process.getdents(&mut fake, fd, &mut buffer).unwrap();
        if used == 0 {
            break;
        }
        seen.extend(names(&buffer[..used]));
    }
    let expected: Vec<(String, u8)> = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|name| (name.to_string(), DT_REG))
        .chain([("sub".to_string(), DT_DIR)])
        .collect();
    assert_eq!(seen, expected);
    // A rewind lists again, with a large buffer.
    assert_eq!(process.lseek(&mut fake, fd, 0, SEEK_SET), Ok(0));
    let mut large = [0u8; 4096];
    let mut again = Vec::new();
    loop {
        let used = process.getdents(&mut fake, fd, &mut large).unwrap();
        if used == 0 {
            break;
        }
        again.extend(names(&large[..used]));
    }
    assert_eq!(again, expected);
    // Too small for even one record.
    assert_eq!(process.lseek(&mut fake, fd, 0, SEEK_SET), Ok(0));
    assert_eq!(process.getdents(&mut fake, fd, &mut [0u8; 8]), Err(EINVAL));
}

#[test]
fn mkdir_unlink_and_rmdir_tell_files_from_directories() {
    let (mut fake, mut process) = setup();
    assert_eq!(process.mkdirat(&mut fake, AT_FDCWD, b"/tmp"), Ok(0));
    assert_eq!(process.mkdirat(&mut fake, AT_FDCWD, b"/tmp"), Err(EEXIST));
    assert_eq!(process.mkdirat(&mut fake, AT_FDCWD, b"/"), Err(EEXIST));
    fake.put("/tmp/f", FsNode::File(Vec::new()));
    assert_eq!(
        process.unlinkat(&mut fake, AT_FDCWD, b"/tmp", 0),
        Err(EISDIR)
    );
    assert_eq!(
        process.unlinkat(&mut fake, AT_FDCWD, b"/tmp/f", AT_REMOVEDIR),
        Err(ENOTDIR)
    );
    assert_eq!(
        process.unlinkat(&mut fake, AT_FDCWD, b"/tmp", AT_REMOVEDIR),
        Err(ENOTEMPTY)
    );
    assert_eq!(process.unlinkat(&mut fake, AT_FDCWD, b"/tmp/f", 0), Ok(0));
    assert_eq!(fake.file("/tmp/f"), None);
    assert_eq!(
        process.unlinkat(&mut fake, AT_FDCWD, b"/tmp", AT_REMOVEDIR),
        Ok(0)
    );
    assert_eq!(
        process.unlinkat(&mut fake, AT_FDCWD, b"/tmp", AT_REMOVEDIR),
        Err(ENOENT)
    );
    assert_eq!(
        process.unlinkat(&mut fake, AT_FDCWD, b"/", AT_REMOVEDIR),
        Err(EBUSY)
    );
}

#[test]
fn relative_paths_follow_the_working_directory_and_dirfd() {
    let (mut fake, mut process) = setup();
    fake.put("/home", FsNode::Directory);
    fake.put("/home/u", FsNode::Directory);
    fake.put("/home/u/f", FsNode::File(b"data".to_vec()));
    assert_eq!(process.chdir(&mut fake, b"/home/u"), Ok(0));
    let mut cwd = [0u8; 64];
    assert_eq!(process.getcwd(&mut cwd), Ok(8));
    assert_eq!(&cwd[..8], b"/home/u\0");
    assert_eq!(process.getcwd(&mut [0u8; 7]), Err(ERANGE));
    let fd = open(&mut fake, &mut process, "f", O_RDONLY).unwrap();
    let mut buffer = [0u8; 8];
    assert_eq!(process.read(&mut fake, fd, &mut buffer), Ok(4));
    let home = open(&mut fake, &mut process, "/home", O_RDONLY | O_DIRECTORY).unwrap();
    let mut stat = Kstat::default();
    assert_eq!(
        process.fstatat(&mut fake, home, b"u/f", &mut stat, 0),
        Ok(0)
    );
    assert_eq!(stat.st_size, 4);
    assert_eq!(
        process.fstatat(&mut fake, fd, b"x", &mut stat, 0),
        Err(ENOTDIR)
    );
    assert_eq!(process.chdir(&mut fake, b"f"), Err(ENOTDIR));
    assert_eq!(process.fchdir(home), Ok(0));
    assert_eq!(process.getcwd(&mut cwd), Ok(6));
    assert_eq!(
        process.faccessat(&mut fake, AT_FDCWD, b"u/f", R_OK, 0),
        Ok(0)
    );
    assert_eq!(
        process.faccessat(&mut fake, AT_FDCWD, b"u/g", R_OK, 0),
        Err(ENOENT)
    );
}

#[test]
fn dev_null_works_without_a_namespace_and_nothing_else_does() {
    let mut fake = Fake::default();
    let mut process = Box::new(Process::new());
    let fd = open(&mut fake, &mut process, "/dev/null", O_RDWR).unwrap();
    assert_eq!(process.write(&mut fake, fd, b"gone"), Ok(4));
    assert_eq!(process.read(&mut fake, fd, &mut [0u8; 4]), Ok(0));
    assert_eq!(
        open(&mut fake, &mut process, "/etc/passwd", O_RDONLY),
        Err(ENOENT)
    );
}

#[test]
fn duplicates_share_the_file_position() {
    let (mut fake, mut process) = setup();
    fake.put("/f", FsNode::File(b"abcdef".to_vec()));
    let fd = open(&mut fake, &mut process, "/f", O_RDONLY).unwrap();
    let copy = process.dup(fd).unwrap() as i32;
    let mut two = [0u8; 2];
    assert_eq!(process.read(&mut fake, fd, &mut two), Ok(2));
    assert_eq!(process.read(&mut fake, copy, &mut two), Ok(2));
    assert_eq!(&two, b"cd");
    assert_eq!(process.dup3(&mut fake, fd, 9, O_CLOEXEC), Ok(9));
    assert_eq!(process.fcntl(9, F_GETFD, 0), Ok(FD_CLOEXEC as usize));
    assert_eq!(process.fcntl(fd, F_GETFD, 0), Ok(0));
    assert_eq!(process.fcntl(fd, F_DUPFD, 5), Ok(5));
    assert_eq!(process.dup3(&mut fake, fd, 9, 0x40), Err(EINVAL));
    assert_eq!(process.lseek(&mut fake, 9, 0, SEEK_CUR), Ok(4));
}

#[test]
fn status_flags_can_be_read_and_changed() {
    let (mut fake, mut process) = setup();
    let fd = open(&mut fake, &mut process, "/dev/null", O_WRONLY | O_CLOEXEC).unwrap();
    assert_eq!(process.fcntl(fd, F_GETFL, 0), Ok(O_WRONLY as usize));
    assert_eq!(
        process.fcntl(fd, F_SETFL, (O_NONBLOCK | O_RDWR) as u64),
        Ok(0)
    );
    // The access mode stays; only O_APPEND and O_NONBLOCK change.
    assert_eq!(
        process.fcntl(fd, F_GETFL, 0),
        Ok((O_WRONLY | O_NONBLOCK) as usize)
    );
    assert_eq!(process.ioctl(fd, TIOCGWINSZ), Err(ENOTTY));
    assert_eq!(process.ioctl(77, TIOCGWINSZ), Err(EBADF));
}

#[test]
fn anonymous_memory_maps_protects_and_unmaps() {
    let (mut fake, mut process) = setup();
    let address = process
        .mmap(
            &mut fake,
            0,
            5000,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
        )
        .unwrap() as u64;
    assert_eq!(fake.mappings, [(address, 8192, DwMemoryProtection(3))]);
    assert_eq!(process.mprotect(&mut fake, address, 8192, PROT_READ), Ok(0));
    assert_eq!(fake.mappings[0].2, DW_MEMORY_PROTECTION_READ);
    assert_eq!(
        process.mprotect(&mut fake, address, 8192, PROT_NONE_FOR_TEST),
        Err(ENOTSUP)
    );
    assert_eq!(process.munmap(&mut fake, address, 8192), Ok(0));
    assert!(fake.mappings.is_empty());
    assert_eq!(process.munmap(&mut fake, address + 1, 8192), Err(EINVAL));
}

const PROT_NONE_FOR_TEST: i32 = 0;

#[test]
fn unsupported_mappings_fail_with_the_right_errno() {
    let (mut fake, mut process) = setup();
    let anonymous = MAP_PRIVATE | MAP_ANONYMOUS;
    assert_eq!(
        process.mmap(&mut fake, 0, 0, PROT_READ, anonymous),
        Err(EINVAL)
    );
    assert_eq!(
        process.mmap(&mut fake, 0, 4096, PROT_READ, MAP_PRIVATE),
        Err(ENODEV)
    );
    assert_eq!(process.mmap(&mut fake, 0, 4096, 0, anonymous), Err(ENOTSUP));
    assert_eq!(
        process.mmap(&mut fake, 0, 4096, PROT_WRITE | PROT_EXEC, anonymous),
        Err(ENOTSUP)
    );
    assert_eq!(
        process.mmap(&mut fake, 0, 4096, PROT_READ, MAP_ANONYMOUS),
        Err(EINVAL)
    );
    assert_eq!(
        process.mmap(&mut fake, 0, u64::MAX, PROT_READ, anonymous),
        Err(ENOMEM)
    );
    assert_eq!(
        process.mmap(&mut fake, 0x1001, 4096, PROT_READ, anonymous | MAP_FIXED),
        Err(EINVAL)
    );
    // Write-only and execute-only gain read, which x86_64 cannot omit.
    process
        .mmap(&mut fake, 0, 4096, PROT_WRITE, anonymous)
        .unwrap();
    process
        .mmap(&mut fake, 0, 4096, PROT_EXEC, anonymous)
        .unwrap();
    assert_eq!(fake.mappings[0].2, DwMemoryProtection(3));
    assert_eq!(fake.mappings[1].2, DwMemoryProtection(5));
}

#[test]
fn fixed_mappings_replace_and_noreplace_refuses() {
    let (mut fake, mut process) = setup();
    let anonymous = MAP_PRIVATE | MAP_ANONYMOUS;
    let at = 0x7000_0000u64;
    let noreplace = anonymous | MAP_FIXED_NOREPLACE;
    assert_eq!(
        process.mmap(&mut fake, at, 4096, PROT_READ, noreplace),
        Ok(at as usize)
    );
    assert_eq!(
        process.mmap(&mut fake, at, 4096, PROT_READ, noreplace),
        Err(EEXIST)
    );
    assert_eq!(
        process.mmap(
            &mut fake,
            at,
            4096,
            PROT_READ | PROT_WRITE,
            anonymous | MAP_FIXED
        ),
        Ok(at as usize)
    );
    assert_eq!(fake.mappings, [(at, 4096, DwMemoryProtection(3))]);
}

#[test]
fn memory_needs_the_self_root() {
    let mut fake = Fake::default();
    let mut process = Box::new(Process::new());
    let anonymous = MAP_PRIVATE | MAP_ANONYMOUS;
    assert_eq!(
        process.mmap(&mut fake, 0, 4096, PROT_READ, anonymous),
        Err(ENOMEM)
    );
}

#[test]
fn clocks_and_sleeps_use_the_monotonic_clock() {
    let (mut fake, mut process) = setup();
    fake.clock = 3_500_000_000;
    let mut now = Timespec::default();
    assert_eq!(
        process.clock_gettime(&mut fake, CLOCK_MONOTONIC, &mut now),
        Ok(0)
    );
    assert_eq!(
        now,
        Timespec {
            seconds: 3,
            nanoseconds: 500_000_000
        }
    );
    assert_eq!(process.clock_gettime(&mut fake, 2, &mut now), Err(EINVAL));
    let mut resolution = Timespec::default();
    assert_eq!(
        process.clock_getres(CLOCK_REALTIME, Some(&mut resolution)),
        Ok(0)
    );
    assert_eq!(resolution.nanoseconds, 1000);
    let request = Timespec {
        seconds: 1,
        nanoseconds: 250,
    };
    assert_eq!(process.nanosleep(&mut fake, &request), Ok(0));
    assert_eq!(fake.clock, 4_500_000_250);
    let absolute = Timespec {
        seconds: 9,
        nanoseconds: 0,
    };
    assert_eq!(
        process.clock_nanosleep(&mut fake, CLOCK_MONOTONIC, TIMER_ABSTIME, &absolute),
        Ok(0)
    );
    assert_eq!(fake.clock, 9_000_000_000);
    let bad = Timespec {
        seconds: 0,
        nanoseconds: 1_000_000_000,
    };
    assert_eq!(process.nanosleep(&mut fake, &bad), Err(EINVAL));
    let negative = Timespec {
        seconds: -1,
        nanoseconds: 0,
    };
    assert_eq!(process.nanosleep(&mut fake, &negative), Err(EINVAL));
}
