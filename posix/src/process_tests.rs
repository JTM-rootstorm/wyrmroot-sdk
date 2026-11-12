// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
//! Host tests of the POSIX operations against the fake kernel and
//! filesystem.

use crate::errno::*;
use crate::process::Process;
use crate::testing::{FILESYSTEM, Fake, FsNode, NAMESPACE, ROOT};
use crate::types::*;
use deepwyrm_syscall::*;
use wyrmroot_fs_lock::Outcome;
use wyrmroot_fs_proto::node::{Lock, LockMode};

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

const ARENA: u64 = crate::arena::ARENA_PAGES * 4096;
const RW: DwMemoryProtection = DwMemoryProtection(3);

#[test]
fn anonymous_memory_comes_from_arenas_and_is_zeroed_on_reuse() {
    let (mut fake, mut process) = setup();
    let anonymous = MAP_PRIVATE | MAP_ANONYMOUS;
    let first = process
        .mmap(&mut fake, 0, 5000, PROT_READ | PROT_WRITE, anonymous)
        .unwrap() as u64;
    // One 4 MiB read-write arena, and the request at its start.
    assert_eq!(fake.mappings, [(first, ARENA, RW)]);
    let second = process
        .mmap(&mut fake, 0, 4096, PROT_READ, anonymous)
        .unwrap() as u64;
    assert_eq!(second, first + 8192);
    assert_eq!(fake.mappings.len(), 1);
    assert!(fake.zeroed.is_empty());
    // Unmapping returns the pages; reusing them zeroes them first.
    assert_eq!(process.munmap(&mut fake, first, 8192), Ok(0));
    assert_eq!(fake.mappings.len(), 1);
    let again = process
        .mmap(&mut fake, 0, 8192, PROT_READ | PROT_WRITE, anonymous)
        .unwrap() as u64;
    assert_eq!(again, first);
    assert_eq!(fake.zeroed, [(first, 8192)]);
    assert_eq!(process.munmap(&mut fake, first + 1, 8192), Err(EINVAL));
    // Arena pages cannot be protected: they are zeroed for reuse through
    // their read-write mapping (security review M1).
    assert_eq!(
        process.mprotect(&mut fake, first, 4096, PROT_READ),
        Err(ENOTSUP)
    );
    assert_eq!(
        process.mprotect(&mut fake, first + ARENA - 4096, 8192, PROT_READ),
        Err(ENOTSUP)
    );
    assert_eq!(fake.mappings, [(first, ARENA, RW)]);
    // A range that would split an arena is refused.
    assert_eq!(
        process.munmap(&mut fake, first + ARENA - 4096, 8192),
        Err(EINVAL)
    );
}

#[test]
fn the_arenas_stop_at_the_job_budget() {
    let (mut fake, mut process) = setup();
    let anonymous = MAP_PRIVATE | MAP_ANONYMOUS;
    // Larger than an arena: its own arena of exactly that size.
    let big = 6 * 1024 * 1024;
    let address = process
        .mmap(&mut fake, 0, big, PROT_READ | PROT_WRITE, anonymous)
        .unwrap() as u64;
    assert_eq!(fake.mappings, [(address, big, RW)]);
    // More than the whole 16 MiB budget at once is refused.
    let too_big = crate::arena::MAX_ARENA_PAGES * 4096 + 4096;
    assert_eq!(
        process.mmap(&mut fake, 0, too_big, PROT_READ, anonymous),
        Err(ENOMEM)
    );
    for _ in 0..3 {
        process
            .mmap(&mut fake, 0, ARENA, PROT_READ, anonymous)
            .unwrap();
    }
    assert_eq!(fake.mappings.len(), 4);
    // Four arenas are the job's MemoryObject budget.
    assert_eq!(
        process.mmap(&mut fake, 0, 4096, PROT_READ, anonymous),
        Err(ENOMEM)
    );
}

#[test]
fn a_refused_arena_is_halved_until_it_maps() {
    // The kernel refuses large maps (its page budget, or the pages one map
    // call can publish): the arena halves, but stays an arena, so small
    // requests share it rather than each spending a MemoryObject (found on
    // Wyrmroot, 2026-11-12: zlib's deflateInit ran out after four).
    let (mut fake, mut process) = setup();
    fake.map_limit = Some(64 * 1024);
    let anonymous = MAP_PRIVATE | MAP_ANONYMOUS;
    let first = process
        .mmap(&mut fake, 0, 4096, PROT_READ, anonymous)
        .unwrap() as u64;
    assert_eq!(fake.mappings, [(first, 64 * 1024, RW)]);
    for page in 1..16 {
        let address = process
            .mmap(&mut fake, 0, 4096, PROT_READ, anonymous)
            .unwrap() as u64;
        assert_eq!(address, first + page * 4096);
    }
    assert_eq!(fake.mappings.len(), 1);
    // Never below the request: a request the kernel refuses is ENOMEM.
    assert_eq!(
        process.mmap(&mut fake, 0, 128 * 1024, PROT_READ, anonymous),
        Err(ENOMEM)
    );
}

#[test]
fn executable_memory_is_mapped_directly_and_can_be_protected() {
    let (mut fake, mut process) = setup();
    let anonymous = MAP_PRIVATE | MAP_ANONYMOUS;
    let address = process
        .mmap(&mut fake, 0, 4096, PROT_EXEC, anonymous)
        .unwrap() as u64;
    assert_eq!(fake.mappings, [(address, 4096, DwMemoryProtection(5))]);
    assert_eq!(process.mprotect(&mut fake, address, 4096, PROT_READ), Ok(0));
    assert_eq!(fake.mappings[0].2, DW_MEMORY_PROTECTION_READ);
    assert_eq!(
        process.mprotect(&mut fake, address, 4096, PROT_NONE_FOR_TEST),
        Err(ENOTSUP)
    );
    assert_eq!(process.munmap(&mut fake, address, 4096), Ok(0));
    assert!(fake.mappings.is_empty());
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
    assert!(fake.mappings.is_empty());
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
    // A fixed mapping never lands on an arena.
    let arena = process
        .mmap(&mut fake, 0, 4096, PROT_READ, anonymous)
        .unwrap() as u64;
    assert_eq!(
        process.mmap(&mut fake, arena, 4096, PROT_READ, noreplace),
        Err(EEXIST)
    );
    assert_eq!(
        process.mmap(&mut fake, arena, 4096, PROT_READ, anonymous | MAP_FIXED),
        Err(EINVAL)
    );
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

#[test]
fn an_open_answered_for_another_transaction_fails_closed() {
    let (mut fake, mut process) = setup();
    fake.put("/f", FsNode::File(b"x".to_vec()));
    fake.wrong_open_transaction = true;
    let closed = fake.closed().len();
    assert_eq!(open(&mut fake, &mut process, "/f", O_RDONLY), Err(EIO));
    // The new node Channel is closed, not leaked.
    assert_eq!(fake.closed().len(), closed + 1);
}

// ---- record locks (W10) -------------------------------------------------

/// The lock owner standing for another process.
const OTHER: u64 = 0xdead;

fn flock(l_type: i16, l_start: i64, l_len: i64) -> Flock {
    Flock {
        l_type,
        l_whence: SEEK_SET as i16,
        l_start,
        l_len,
        l_pid: 0,
    }
}

fn other_locks(fake: &mut Fake, path: &str, lock: Lock) -> bool {
    let file = fake.file_id(path);
    matches!(
        fake.locks.request(OTHER, file, lock, 0, 0, 0),
        Outcome::Granted(_)
    )
}

fn whole(mode: LockMode) -> Lock {
    Lock {
        start: 0,
        length: 0,
        mode,
    }
}

#[test]
fn classic_locks_belong_to_the_process_and_go_with_any_close() {
    let (mut fake, mut process) = setup();
    fake.put("/db", FsNode::File(vec![0; 100]));
    let first = open(&mut fake, &mut process, "/db", O_RDWR).unwrap();
    let second = open(&mut fake, &mut process, "/db", O_RDONLY).unwrap();
    let mut stat = Kstat::default();
    process.fstat(&mut fake, first, &mut stat).unwrap();
    assert_eq!(stat.st_dev, FILESYSTEM);

    // Two descriptions in one process never conflict.
    let mut write = flock(F_WRLCK, 0, 10);
    assert_eq!(
        process.fcntl_lock(&mut fake, first, F_SETLK, &mut write),
        Ok(0)
    );
    let mut read = flock(F_RDLCK, 5, 10);
    assert_eq!(
        process.fcntl_lock(&mut fake, second, F_SETLK, &mut read),
        Ok(0)
    );
    // The process sees no conflict with itself; another process does.
    let mut probe = flock(F_WRLCK, 0, 0);
    assert_eq!(
        process.fcntl_lock(&mut fake, second, F_GETLK, &mut probe),
        Ok(0)
    );
    assert_eq!(probe.l_type, F_UNLCK);
    assert!(!other_locks(&mut fake, "/db", whole(LockMode::Shared)));
    assert!(other_locks(
        &mut fake,
        "/db",
        Lock {
            start: 50,
            length: 10,
            mode: LockMode::Exclusive,
        }
    ));
    let mut probe = flock(F_RDLCK, 0, 0);
    assert_eq!(
        process.fcntl_lock(&mut fake, first, F_GETLK, &mut probe),
        Ok(0)
    );
    assert_eq!(
        (probe.l_type, probe.l_start, probe.l_len, probe.l_pid),
        (F_WRLCK, 50, 10, -1)
    );
    let mut blocked = flock(F_RDLCK, 55, 1);
    assert_eq!(
        process.fcntl_lock(&mut fake, first, F_SETLK, &mut blocked),
        Err(EAGAIN)
    );
    fake.locks.release(OTHER);

    // Closing either descriptor releases every lock on the file, and the
    // lock Channel goes with the last description.
    let closed = fake.closed().len();
    assert_eq!(process.close(&mut fake, second), Ok(0));
    assert_eq!(fake.closed().len(), closed + 1, "only the descriptor");
    assert!(other_locks(&mut fake, "/db", whole(LockMode::Exclusive)));
    fake.locks.release(OTHER);
    let mut again = flock(F_WRLCK, 0, 0);
    assert_eq!(
        process.fcntl_lock(&mut fake, first, F_SETLK, &mut again),
        Ok(0)
    );
    assert_eq!(process.close(&mut fake, first), Ok(0));
    assert_eq!(
        fake.closed().len(),
        closed + 3,
        "the descriptor and the lock Channel"
    );
    assert_eq!(fake.locks.table().iter().count(), 0);
}

#[test]
fn lock_requests_are_checked_against_the_descriptor() {
    let (mut fake, mut process) = setup();
    fake.put("/db", FsNode::File(vec![0; 100]));
    let reader = open(&mut fake, &mut process, "/db", O_RDONLY).unwrap();
    let writer = open(&mut fake, &mut process, "/db", O_WRONLY).unwrap();
    for (fd, mut request, error) in [
        (reader, flock(F_WRLCK, 0, 1), EBADF),
        (writer, flock(F_RDLCK, 0, 1), EBADF),
        (reader, flock(7, 0, 1), EINVAL),
        (reader, flock(F_RDLCK, -1, 1), EINVAL),
        (reader, flock(F_RDLCK, 0, -1), EINVAL),
        (reader, flock(F_RDLCK, i64::MAX, 2), EOVERFLOW),
        // W10 review D3: a length that cannot be negated.
        (reader, flock(F_RDLCK, 0, i64::MIN), EINVAL),
    ] {
        assert_eq!(
            process.fcntl_lock(&mut fake, fd, F_SETLK, &mut request),
            Err(error),
            "{request:?}"
        );
    }
    let mut unlock = flock(F_UNLCK, 0, 0);
    assert_eq!(
        process.fcntl_lock(&mut fake, reader, F_GETLK, &mut unlock),
        Err(EINVAL)
    );
    assert_eq!(
        process.fcntl_lock(&mut fake, reader, F_SETLK, &mut unlock),
        Ok(0)
    );
    // SEEK_CUR with a negative length locks the bytes before the offset.
    assert_eq!(process.lseek(&mut fake, reader, 40, SEEK_SET), Ok(40));
    let mut before = Flock {
        l_whence: SEEK_CUR as i16,
        ..flock(F_RDLCK, 0, -30)
    };
    assert_eq!(
        process.fcntl_lock(&mut fake, reader, F_SETLK, &mut before),
        Ok(0)
    );
    let held: Vec<_> = fake
        .locks
        .table()
        .iter()
        .map(|held| (held.start, held.end))
        .collect();
    assert_eq!(held, [(10, 40)]);
}

#[test]
fn description_locks_conflict_between_descriptions_but_not_duplicates() {
    let (mut fake, mut process) = setup();
    fake.put("/db", FsNode::File(Vec::new()));
    let first = open(&mut fake, &mut process, "/db", O_RDWR).unwrap();
    let copy = process.dup(first).unwrap() as i32;
    let second = open(&mut fake, &mut process, "/db", O_RDWR).unwrap();
    let mut write = flock(F_WRLCK, 0, 0);
    assert_eq!(
        process.fcntl_lock(&mut fake, first, F_OFD_SETLK, &mut write),
        Ok(0)
    );
    assert_eq!(
        process.fcntl_lock(&mut fake, copy, F_OFD_SETLK, &mut write),
        Ok(0)
    );
    assert_eq!(
        process.fcntl_lock(&mut fake, second, F_OFD_SETLK, &mut write),
        Err(EAGAIN)
    );
    let mut probe = flock(F_RDLCK, 0, 0);
    assert_eq!(
        process.fcntl_lock(&mut fake, second, F_OFD_GETLK, &mut probe),
        Ok(0)
    );
    assert_eq!((probe.l_type, probe.l_pid), (F_WRLCK, -1));
    let mut with_pid = Flock {
        l_pid: 1,
        ..flock(F_RDLCK, 0, 0)
    };
    assert_eq!(
        process.fcntl_lock(&mut fake, second, F_OFD_GETLK, &mut with_pid),
        Err(EINVAL)
    );
    // The description goes with its last descriptor, and its locks with it.
    assert_eq!(process.close(&mut fake, first), Ok(0));
    assert_eq!(
        process.fcntl_lock(&mut fake, second, F_OFD_SETLK, &mut write),
        Err(EAGAIN)
    );
    assert_eq!(process.close(&mut fake, copy), Ok(0));
    assert_eq!(
        process.fcntl_lock(&mut fake, second, F_OFD_SETLK, &mut write),
        Ok(0)
    );
}

#[test]
fn a_waiting_lock_is_taken_once_the_holder_lets_go() {
    let (mut fake, mut process) = setup();
    fake.put("/db", FsNode::File(Vec::new()));
    let fd = open(&mut fake, &mut process, "/db", O_RDWR).unwrap();
    assert!(other_locks(&mut fake, "/db", whole(LockMode::Exclusive)));
    let mut write = flock(F_WRLCK, 0, 0);
    assert_eq!(
        process.fcntl_lock(&mut fake, fd, F_SETLK, &mut write),
        Err(EAGAIN)
    );
    // The server answers the wait when the holder lets go; nothing polls.
    fake.release_when_waited = Some(OTHER);
    let clock = fake.clock;
    assert_eq!(
        process.fcntl_lock(&mut fake, fd, F_SETLKW, &mut write),
        Ok(0)
    );
    assert_eq!(fake.clock, clock);
    assert_eq!(
        process.fcntl_lock(&mut fake, fd, F_SETLK, &mut flock(F_UNLCK, 0, 0)),
        Ok(0)
    );
    assert_eq!(fake.locks.table().iter().count(), 0);
}

#[test]
fn waits_that_never_end_are_edeadlk() {
    let (mut fake, mut process) = setup();
    fake.put("/a", FsNode::File(Vec::new()));
    fake.put("/b", FsNode::File(Vec::new()));
    let a = open(&mut fake, &mut process, "/a", O_RDWR).unwrap();
    let b = open(&mut fake, &mut process, "/b", O_RDWR).unwrap();
    // The process's own description lock blocks its classic lock: Linux
    // would wait for ever.
    let mut write = flock(F_WRLCK, 0, 0);
    assert_eq!(
        process.fcntl_lock(&mut fake, a, F_OFD_SETLK, &mut write),
        Ok(0)
    );
    assert_eq!(
        process.fcntl_lock(&mut fake, a, F_SETLKW, &mut write),
        Err(EDEADLK)
    );
    // Another process holds /b and waits for /a, which this one holds by
    // its description lock: waiting for /b would close the cycle.
    let file_b = fake.file_id("/b");
    let file_a = fake.file_id("/a");
    let Outcome::Granted(other) =
        fake.locks
            .request(OTHER, file_b, whole(LockMode::Exclusive), 0, 0, 0)
    else {
        panic!("the other process locks /b");
    };
    assert_eq!(
        fake.locks.request(
            OTHER + 1,
            file_a,
            whole(LockMode::Exclusive),
            wyrmroot_fs_proto::node::LOCK_WAIT,
            other,
            7
        ),
        Outcome::Waiting
    );
    assert_eq!(
        process.fcntl_lock(&mut fake, b, F_SETLKW, &mut write),
        Err(EDEADLK)
    );
    // One group for the process on the filesystem.
    let mut plain = flock(F_RDLCK, 0, 0);
    assert_eq!(
        process.fcntl_lock(&mut fake, a, F_SETLK, &mut plain),
        Err(EAGAIN)
    );
}

#[test]
fn a_lock_without_room_for_its_channel_is_enolck() {
    // W10 review D4: not EMFILE or ENFILE, which fcntl never returns.
    let (mut fake, mut process) = setup();
    fake.put("/db", FsNode::File(Vec::new()));
    let fd = open(&mut fake, &mut process, "/db", O_RDWR).unwrap();
    fake.refuse_reopen = true;
    let mut write = flock(F_WRLCK, 0, 0);
    assert_eq!(
        process.fcntl_lock(&mut fake, fd, F_SETLK, &mut write),
        Err(ENOLCK)
    );
    assert_eq!(fake.locks.table().iter().count(), 0);
}

#[test]
fn a_group_that_went_with_its_last_channel_is_replaced() {
    let (mut fake, mut process) = setup();
    fake.put("/a", FsNode::File(Vec::new()));
    fake.put("/b", FsNode::File(Vec::new()));
    let a = open(&mut fake, &mut process, "/a", O_RDWR).unwrap();
    let mut write = flock(F_WRLCK, 0, 0);
    assert_eq!(process.fcntl_lock(&mut fake, a, F_SETLK, &mut write), Ok(0));
    // The lock Channel, the group's only member, goes with the file.
    assert_eq!(process.close(&mut fake, a), Ok(0));
    let b = open(&mut fake, &mut process, "/b", O_RDWR).unwrap();
    assert_eq!(process.fcntl_lock(&mut fake, b, F_SETLK, &mut write), Ok(0));
}
