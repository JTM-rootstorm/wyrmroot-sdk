// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
//! One process's POSIX state and the operations over it: descriptors,
//! paths, memory and time. Every operation takes the native interface as
//! an argument, so host tests run the same code against a fake.

use crate::arena::{ARENA_PAGES, Arena, Arenas, MAX_ARENA_PAGES, Place};
use crate::errno::*;
use crate::fdtable::FdTable;
use crate::native::Native;
use crate::node::{self, Scratch};
use crate::path::{self, PathBuf};
use crate::stream::{self, Input};
use crate::types::*;
use deepwyrm_syscall::*;
use wyrmroot_fs_proto::node::{Attr, Kind};
use wyrmroot_fs_proto::{
    MAX_PATH_BYTES, OPEN_CREATE, OPEN_DIRECTORY, OPEN_EXCLUSIVE, OPEN_FILE, OPEN_READ,
    OPEN_TRUNCATE, OPEN_WRITE,
};

/// Open file descriptions a process may hold at once.
pub const MAX_FILES: usize = 64;
pub const PAGE_SIZE: u64 = 4096;
/// The resolution `clock_getres` reports until Deepwyrm publishes one
/// (request D4).
pub const CLOCK_RESOLUTION_NS: i64 = 1000;
const NULL_PATH: &[u8] = b"/dev/null";

pub struct NodeFile {
    pub channel: DwHandle,
    pub directory: bool,
    /// File position, or the directory listing's current page cookie.
    pub offset: u64,
    /// Entries of the current page already returned.
    skip: usize,
    ended: bool,
    /// Entries returned so far, for `d_off` and synthetic `d_ino`.
    returned: u64,
    id: u64,
    dirty: bool,
    path: PathBuf,
    path_length: usize,
}

pub enum FileKind {
    Null,
    Input(Input),
    Output(DwHandle),
    Node(NodeFile),
}

pub struct OpenFile {
    pub kind: FileKind,
    /// Access mode and status flags (`O_*`).
    pub flags: i32,
}

impl OpenFile {
    fn readable(&self) -> bool {
        self.flags & O_PATH == 0 && self.flags & 3 != O_WRONLY
    }

    fn writable(&self) -> bool {
        self.flags & O_PATH == 0 && self.flags & 3 != O_RDONLY
    }

    fn nonblocking(&self) -> bool {
        self.flags & O_NONBLOCK != 0
    }
}

pub struct Process {
    pub files: FdTable<OpenFile, MAX_FILES>,
    pub scratch: Scratch,
    pub self_root: Option<DwHandle>,
    pub namespace: Option<DwHandle>,
    cwd: PathBuf,
    cwd_length: usize,
    /// A private Channel nobody writes to; sleeps wait on it with a
    /// deadline.
    sleeper: Option<(DwHandle, DwHandle)>,
    arenas: Arenas,
}

impl Default for Process {
    fn default() -> Self {
        Self::new()
    }
}

impl Process {
    /// All zero bytes, so the process state lives in `.bss`, not the file.
    pub const fn new() -> Self {
        Self {
            files: FdTable::new(),
            scratch: Scratch::new(),
            self_root: None,
            namespace: None,
            cwd: [0; MAX_PATH_BYTES],
            cwd_length: 0,
            sleeper: None,
            arenas: Arenas::new(),
        }
    }

    /// The working directory; `/` until something else is set.
    pub fn cwd(&self) -> &[u8] {
        if self.cwd_length == 0 {
            b"/"
        } else {
            &self.cwd[..self.cwd_length]
        }
    }

    /// Takes the working directory from `PWD` when it is already canonical
    /// and absolute; otherwise it stays `/`. It is only a string: every
    /// open still goes through the namespace.
    pub fn set_cwd_from_environment(&mut self, pwd: &[u8]) {
        let mut buffer = [0u8; MAX_PATH_BYTES];
        if let Ok(normalized) = path::normalize(b"/", pwd, &mut buffer)
            && pwd.first() == Some(&b'/')
            && &buffer[..normalized.length] == pwd
        {
            self.cwd[..pwd.len()].copy_from_slice(pwd);
            self.cwd_length = pwd.len();
        }
    }

    /// Installs a startup stream at the lowest free descriptor.
    pub fn install_stream(&mut self, channel: DwHandle, input: bool) -> Result<i32> {
        let file = if input {
            OpenFile {
                kind: FileKind::Input(Input::new(channel)),
                flags: O_RDONLY,
            }
        } else {
            OpenFile {
                kind: FileKind::Output(channel),
                flags: O_WRONLY,
            }
        };
        self.files
            .install(file, 0, false)
            .map_err(|(_, error)| error)
    }

    // ---- descriptors -------------------------------------------------

    pub fn read<N: Native>(&mut self, native: &mut N, fd: i32, output: &mut [u8]) -> Result<usize> {
        let Self { files, scratch, .. } = self;
        let file = files.get(fd)?;
        if !file.readable() {
            return Err(EBADF);
        }
        let nonblocking = file.nonblocking();
        match &mut file.kind {
            FileKind::Null => Ok(0),
            FileKind::Input(input) => input.read(native, output, nonblocking),
            FileKind::Output(_) => Err(EBADF),
            FileKind::Node(node) if node.directory => Err(EISDIR),
            FileKind::Node(node) => {
                let count = node::read(native, node.channel, scratch, node.offset, output)?;
                node.offset += count as u64;
                Ok(count)
            }
        }
    }

    pub fn write<N: Native>(&mut self, native: &mut N, fd: i32, data: &[u8]) -> Result<usize> {
        let Self { files, scratch, .. } = self;
        let file = files.get(fd)?;
        if !file.writable() {
            return Err(EBADF);
        }
        let nonblocking = file.nonblocking();
        let append = file.flags & O_APPEND != 0;
        match &mut file.kind {
            FileKind::Null => Ok(data.len()),
            FileKind::Input(_) => Err(EBADF),
            FileKind::Output(channel) => stream::write(native, *channel, data, nonblocking),
            FileKind::Node(node) if node.directory => Err(EISDIR),
            FileKind::Node(node) => {
                if data.is_empty() {
                    return Ok(0);
                }
                if append {
                    node.offset = node::get_attr(native, node.channel, scratch)?.size;
                }
                let count = node::write(native, node.channel, scratch, node.offset, data)?;
                node.offset += count as u64;
                node.dirty = true;
                Ok(count)
            }
        }
    }

    fn release<N: Native>(&mut self, native: &mut N, file: OpenFile) -> Result<()> {
        match file.kind {
            FileKind::Null => Ok(()),
            FileKind::Input(input) => native.close(input.channel).map_err(|_| EIO),
            FileKind::Output(channel) => native.close(channel).map_err(|_| EIO),
            FileKind::Node(node) => {
                // Written files are made durable on their last close.
                let synced = if node.dirty {
                    node::status(
                        native,
                        node.channel,
                        &mut self.scratch,
                        wyrmroot_fs_proto::node::Body::Sync,
                    )
                    .map(|_| ())
                } else {
                    Ok(())
                };
                let closed = native.close(node.channel).map_err(|_| EIO);
                synced.and(closed)
            }
        }
    }

    pub fn close<N: Native>(&mut self, native: &mut N, fd: i32) -> Result<usize> {
        match self.files.close(fd)? {
            Some(file) => self.release(native, file).map(|()| 0),
            None => Ok(0),
        }
    }

    pub fn dup(&mut self, fd: i32) -> Result<usize> {
        self.files.dup(fd, 0, false).map(|fd| fd as usize)
    }

    pub fn dup3<N: Native>(
        &mut self,
        native: &mut N,
        old: i32,
        new: i32,
        flags: i32,
    ) -> Result<usize> {
        if flags & !O_CLOEXEC != 0 {
            return Err(EINVAL);
        }
        let (fd, released) = self.files.dup_to(old, new, flags & O_CLOEXEC != 0)?;
        if let Some(file) = released {
            // POSIX dup2 ignores errors closing the replaced descriptor.
            let _ = self.release(native, file);
        }
        Ok(fd as usize)
    }

    pub fn fcntl(&mut self, fd: i32, command: i32, argument: u64) -> Result<usize> {
        match command {
            F_DUPFD | F_DUPFD_CLOEXEC => {
                let minimum = i32::try_from(argument).map_err(|_| EINVAL)?;
                self.files
                    .dup(fd, minimum, command == F_DUPFD_CLOEXEC)
                    .map(|fd| fd as usize)
            }
            F_GETFD => Ok(if self.files.cloexec(fd)? {
                FD_CLOEXEC as usize
            } else {
                0
            }),
            F_SETFD => {
                self.files
                    .set_cloexec(fd, argument as i32 & FD_CLOEXEC != 0)?;
                Ok(0)
            }
            F_GETFL => Ok(self.files.get(fd)?.flags as u32 as usize),
            F_SETFL => {
                let file = self.files.get(fd)?;
                file.flags = (file.flags & !O_SETTABLE) | (argument as i32 & O_SETTABLE);
                Ok(0)
            }
            _ => {
                self.files.get(fd)?;
                Err(EINVAL)
            }
        }
    }

    pub fn ioctl(&mut self, fd: i32, request: u64) -> Result<usize> {
        self.files.get(fd)?;
        // No descriptor is a terminal until INIT marks consoles (W7), so
        // TIOCGWINSZ fails and stdio stays fully buffered.
        let _ = request;
        Err(ENOTTY)
    }

    pub fn lseek<N: Native>(
        &mut self,
        native: &mut N,
        fd: i32,
        offset: i64,
        whence: i32,
    ) -> Result<usize> {
        let Self { files, scratch, .. } = self;
        let file = files.get(fd)?;
        match &mut file.kind {
            FileKind::Null => Ok(0),
            FileKind::Input(_) | FileKind::Output(_) => Err(ESPIPE),
            FileKind::Node(node) if node.directory => {
                // Only a rewind, as `rewinddir` does.
                if offset == 0 && whence == SEEK_SET {
                    node.offset = 0;
                    node.skip = 0;
                    node.ended = false;
                    node.returned = 0;
                    Ok(0)
                } else {
                    Err(EINVAL)
                }
            }
            FileKind::Node(node) => {
                let base = match whence {
                    SEEK_SET => 0i64,
                    SEEK_CUR => i64::try_from(node.offset).map_err(|_| EOVERFLOW)?,
                    SEEK_END => i64::try_from(node::get_attr(native, node.channel, scratch)?.size)
                        .map_err(|_| EOVERFLOW)?,
                    _ => return Err(EINVAL),
                };
                let position = base.checked_add(offset).ok_or(EOVERFLOW)?;
                if position < 0 {
                    return Err(EINVAL);
                }
                node.offset = position as u64;
                Ok(position as usize)
            }
        }
    }

    pub fn fstat<N: Native>(&mut self, native: &mut N, fd: i32, out: &mut Kstat) -> Result<usize> {
        let Self { files, scratch, .. } = self;
        let file = files.get(fd)?;
        *out = Kstat::default();
        out.st_nlink = 1;
        out.st_blksize = 4096;
        match &file.kind {
            FileKind::Null => out.st_mode = S_IFCHR | 0o666,
            FileKind::Input(_) | FileKind::Output(_) => out.st_mode = S_IFIFO | 0o600,
            FileKind::Node(node) => {
                let attr = node::get_attr(native, node.channel, scratch)?;
                fill_stat(out, &attr, node.id);
            }
        }
        Ok(0)
    }

    pub fn fsync<N: Native>(&mut self, native: &mut N, fd: i32) -> Result<usize> {
        let Self { files, scratch, .. } = self;
        match &mut files.get(fd)?.kind {
            FileKind::Node(node) => {
                node::status(
                    native,
                    node.channel,
                    scratch,
                    wyrmroot_fs_proto::node::Body::Sync,
                )?;
                node.dirty = false;
                Ok(0)
            }
            _ => Err(EINVAL),
        }
    }

    pub fn ftruncate<N: Native>(&mut self, native: &mut N, fd: i32, length: i64) -> Result<usize> {
        let size = u64::try_from(length).map_err(|_| EINVAL)?;
        let Self { files, scratch, .. } = self;
        let file = files.get(fd)?;
        if !file.writable() {
            return Err(EBADF);
        }
        match &mut file.kind {
            FileKind::Node(node) if !node.directory => {
                node::status(
                    native,
                    node.channel,
                    scratch,
                    wyrmroot_fs_proto::node::Body::Truncate { size },
                )?;
                node.dirty = true;
                Ok(0)
            }
            _ => Err(EINVAL),
        }
    }

    /// Fills `buffer` with `getdents64` records; zero at the end.
    pub fn getdents<N: Native>(
        &mut self,
        native: &mut N,
        fd: i32,
        buffer: &mut [u8],
    ) -> Result<usize> {
        let Self { files, scratch, .. } = self;
        let FileKind::Node(node) = &mut files.get(fd)?.kind else {
            return Err(ENOTDIR);
        };
        if !node.directory {
            return Err(ENOTDIR);
        }
        let mut used = 0usize;
        while !node.ended {
            let mut index = 0usize;
            let mut stopped_at = None;
            let skip = node.skip;
            let mut returned = node.returned;
            let next = node::read_dir(native, node.channel, scratch, node.offset, |kind, name| {
                let this = index;
                index += 1;
                if this < skip || stopped_at.is_some() {
                    return;
                }
                let length = (DIRENT64_HEAD + name.len() + 1).next_multiple_of(8);
                if used + length > buffer.len() {
                    stopped_at = Some(this);
                    return;
                }
                returned += 1;
                let record = &mut buffer[used..used + length];
                record.fill(0);
                record[0..8].copy_from_slice(&returned.to_ne_bytes());
                record[8..16].copy_from_slice(&(returned as i64).to_ne_bytes());
                record[16..18].copy_from_slice(&(length as u16).to_ne_bytes());
                record[18] = if kind == Kind::Directory {
                    DT_DIR
                } else {
                    DT_REG
                };
                record[DIRENT64_HEAD..DIRENT64_HEAD + name.len()].copy_from_slice(name);
                used += length;
            })?;
            node.returned = returned;
            match stopped_at {
                Some(position) => {
                    node.skip = position;
                    return if used == 0 { Err(EINVAL) } else { Ok(used) };
                }
                None => {
                    node.offset = next;
                    node.skip = 0;
                    node.ended = next == 0;
                    if used != 0 {
                        return Ok(used);
                    }
                }
            }
        }
        Ok(used)
    }

    // ---- paths -------------------------------------------------------

    /// Resolves `input` against `dirfd` (`AT_FDCWD` or a directory).
    fn resolve(
        &mut self,
        dirfd: i32,
        input: &[u8],
        output: &mut PathBuf,
    ) -> Result<path::Normalized> {
        if input.first() == Some(&b'/') || dirfd == AT_FDCWD {
            let mut base = [0u8; MAX_PATH_BYTES];
            let length = self.cwd().len();
            base[..length].copy_from_slice(self.cwd());
            return path::normalize(&base[..length], input, output);
        }
        match &self.files.get(dirfd)?.kind {
            FileKind::Node(node) if node.directory => {
                let base = node.path;
                path::normalize(&base[..node.path_length], input, output)
            }
            _ => Err(ENOTDIR),
        }
    }

    fn open_node<N: Native>(
        &mut self,
        native: &mut N,
        flags: u32,
        path: &[u8],
    ) -> Result<(DwHandle, Attr)> {
        let namespace = self.namespace.ok_or(ENOENT)?;
        node::open(native, namespace, &mut self.scratch, flags, path)
    }

    pub fn openat<N: Native>(
        &mut self,
        native: &mut N,
        dirfd: i32,
        input: &[u8],
        flags: i32,
    ) -> Result<usize> {
        let mut buffer = [0u8; MAX_PATH_BYTES];
        let normalized = self.resolve(dirfd, input, &mut buffer)?;
        let path = &buffer[..normalized.length];
        let access = flags & 3;
        if access == 3 {
            return Err(EINVAL);
        }
        let cloexec = flags & O_CLOEXEC != 0;
        let status =
            flags & !(O_CREAT | O_EXCL | O_NOCTTY | O_TRUNC | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW);
        if path == NULL_PATH {
            let file = OpenFile {
                kind: FileKind::Null,
                flags: status,
            };
            return self
                .files
                .install(file, 0, cloexec)
                .map(|fd| fd as usize)
                .map_err(|(_, error)| error);
        }
        let want_directory = flags & O_DIRECTORY != 0 || normalized.directory;
        let mut wrfs = if flags & O_PATH != 0 {
            OPEN_READ
        } else {
            match access {
                O_WRONLY => OPEN_WRITE,
                O_RDWR => OPEN_READ | OPEN_WRITE,
                _ => OPEN_READ,
            }
        };
        if want_directory {
            wrfs |= OPEN_DIRECTORY;
        }
        if flags & O_CREAT != 0 && !want_directory {
            wrfs |= OPEN_CREATE | OPEN_FILE;
            if flags & O_EXCL != 0 {
                wrfs |= OPEN_EXCLUSIVE;
            }
        } else if flags & O_CREAT != 0 {
            // Creating a directory is mkdir's job.
            return Err(EISDIR);
        }
        if flags & O_TRUNC != 0 && wrfs & OPEN_WRITE != 0 && !want_directory {
            wrfs |= OPEN_TRUNCATE;
        }
        let (channel, attr) = self.open_node(native, wrfs, path)?;
        let directory = attr.kind == Kind::Directory;
        if directory && wrfs & OPEN_WRITE != 0 {
            let _ = native.close(channel);
            return Err(EISDIR);
        }
        let mut stored = [0u8; MAX_PATH_BYTES];
        stored[..path.len()].copy_from_slice(path);
        let file = OpenFile {
            kind: FileKind::Node(NodeFile {
                channel,
                directory,
                offset: 0,
                skip: 0,
                ended: false,
                returned: 0,
                id: attr.id,
                dirty: flags & O_TRUNC != 0 && !directory,
                path: stored,
                path_length: path.len(),
            }),
            flags: status,
        };
        match self.files.install(file, 0, cloexec) {
            Ok(fd) => Ok(fd as usize),
            Err((file, error)) => {
                let _ = self.release(native, file);
                Err(error)
            }
        }
    }

    pub fn fstatat<N: Native>(
        &mut self,
        native: &mut N,
        dirfd: i32,
        input: &[u8],
        out: &mut Kstat,
        flags: i32,
    ) -> Result<usize> {
        if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH | AT_NO_AUTOMOUNT) != 0 {
            return Err(EINVAL);
        }
        if input.is_empty() && flags & AT_EMPTY_PATH != 0 {
            if dirfd == AT_FDCWD {
                return self.stat_path(native, b".", out);
            }
            return self.fstat(native, dirfd, out);
        }
        let mut buffer = [0u8; MAX_PATH_BYTES];
        let normalized = self.resolve(dirfd, input, &mut buffer)?;
        self.stat_path(native, &buffer[..normalized.length], out)
            .and_then(|done| {
                if normalized.directory && out.st_mode & S_IFDIR == 0 {
                    Err(ENOTDIR)
                } else {
                    Ok(done)
                }
            })
    }

    fn stat_path<N: Native>(
        &mut self,
        native: &mut N,
        input: &[u8],
        out: &mut Kstat,
    ) -> Result<usize> {
        let mut buffer = [0u8; MAX_PATH_BYTES];
        let normalized = self.resolve(AT_FDCWD, input, &mut buffer)?;
        let path = &buffer[..normalized.length];
        *out = Kstat::default();
        out.st_nlink = 1;
        out.st_blksize = 4096;
        if path == NULL_PATH {
            out.st_mode = S_IFCHR | 0o666;
            return Ok(0);
        }
        let (channel, attr) = self.open_node(native, OPEN_READ, path)?;
        let _ = native.close(channel);
        fill_stat(out, &attr, attr.id);
        Ok(0)
    }

    pub fn faccessat<N: Native>(
        &mut self,
        native: &mut N,
        dirfd: i32,
        input: &[u8],
        mode: i32,
        flags: i32,
    ) -> Result<usize> {
        if mode & !(R_OK | W_OK | X_OK) != 0 || flags & !(AT_EACCESS | AT_SYMLINK_NOFOLLOW) != 0 {
            return Err(EINVAL);
        }
        let mut buffer = [0u8; MAX_PATH_BYTES];
        let normalized = self.resolve(dirfd, input, &mut buffer)?;
        let path = &buffer[..normalized.length];
        if path == NULL_PATH {
            return Ok(0);
        }
        let wrfs = if mode & W_OK != 0 {
            OPEN_READ | OPEN_WRITE
        } else {
            OPEN_READ
        };
        let (channel, _) = self.open_node(native, wrfs, path)?;
        let _ = native.close(channel);
        Ok(0)
    }

    pub fn mkdirat<N: Native>(
        &mut self,
        native: &mut N,
        dirfd: i32,
        input: &[u8],
    ) -> Result<usize> {
        let mut buffer = [0u8; MAX_PATH_BYTES];
        let normalized = self.resolve(dirfd, input, &mut buffer)?;
        let path = &buffer[..normalized.length];
        if path.len() == 1 {
            return Err(EEXIST);
        }
        let flags = OPEN_READ | OPEN_DIRECTORY | OPEN_CREATE | OPEN_EXCLUSIVE;
        let (channel, _) = self.open_node(native, flags, path)?;
        let synced = node::status(
            native,
            channel,
            &mut self.scratch,
            wyrmroot_fs_proto::node::Body::Sync,
        );
        let _ = native.close(channel);
        synced.map(|_| 0)
    }

    pub fn unlinkat<N: Native>(
        &mut self,
        native: &mut N,
        dirfd: i32,
        input: &[u8],
        flags: i32,
    ) -> Result<usize> {
        if flags & !AT_REMOVEDIR != 0 {
            return Err(EINVAL);
        }
        let remove_directory = flags & AT_REMOVEDIR != 0;
        let mut buffer = [0u8; MAX_PATH_BYTES];
        let normalized = self.resolve(dirfd, input, &mut buffer)?;
        let path = &buffer[..normalized.length];
        let Some((parent, name)) = path::split(path) else {
            return Err(EBUSY);
        };
        if normalized.directory && !remove_directory {
            return Err(EISDIR);
        }
        // POSIX tells files and directories apart here; WRFS UNLINK does not.
        let (target, attr) = self.open_node(native, OPEN_READ, path)?;
        let _ = native.close(target);
        match (attr.kind, remove_directory) {
            (Kind::Directory, false) => return Err(EISDIR),
            (Kind::File, true) => return Err(ENOTDIR),
            _ => {}
        }
        let mut parent_path = [0u8; MAX_PATH_BYTES];
        parent_path[..parent.len()].copy_from_slice(parent);
        let parent_length = parent.len();
        let mut name_bytes = [0u8; 255];
        name_bytes[..name.len()].copy_from_slice(name);
        let name_length = name.len();
        let (directory, _) = self.open_node(
            native,
            OPEN_READ | OPEN_WRITE | OPEN_DIRECTORY,
            &parent_path[..parent_length],
        )?;
        let result = node::status(
            native,
            directory,
            &mut self.scratch,
            wyrmroot_fs_proto::node::Body::Unlink {
                name: &name_bytes[..name_length],
            },
        )
        .and_then(|_| {
            node::status(
                native,
                directory,
                &mut self.scratch,
                wyrmroot_fs_proto::node::Body::Sync,
            )
        });
        let _ = native.close(directory);
        result.map(|_| 0)
    }

    pub fn getcwd(&self, output: &mut [u8]) -> Result<usize> {
        let length = self.cwd().len();
        if output.len() < length + 1 {
            return Err(ERANGE);
        }
        output[..length].copy_from_slice(self.cwd());
        output[length] = 0;
        Ok(length + 1)
    }

    pub fn chdir<N: Native>(&mut self, native: &mut N, input: &[u8]) -> Result<usize> {
        let mut buffer = [0u8; MAX_PATH_BYTES];
        let normalized = self.resolve(AT_FDCWD, input, &mut buffer)?;
        let path = &buffer[..normalized.length];
        let (channel, _) = self.open_node(native, OPEN_READ | OPEN_DIRECTORY, path)?;
        let _ = native.close(channel);
        self.cwd[..path.len()].copy_from_slice(path);
        self.cwd_length = path.len();
        Ok(0)
    }

    pub fn fchdir(&mut self, fd: i32) -> Result<usize> {
        let (path, length) = match &self.files.get(fd)?.kind {
            FileKind::Node(node) if node.directory => (node.path, node.path_length),
            _ => return Err(ENOTDIR),
        };
        self.cwd = path;
        self.cwd_length = length;
        Ok(0)
    }

    // ---- memory ------------------------------------------------------

    pub fn mmap<N: Native>(
        &mut self,
        native: &mut N,
        address: u64,
        length: u64,
        prot: i32,
        flags: i32,
    ) -> Result<usize> {
        let root = self.self_root.ok_or(ENOMEM)?;
        if length == 0 {
            return Err(EINVAL);
        }
        let length = round_pages(length).ok_or(ENOMEM)?;
        if flags & MAP_ANONYMOUS == 0 {
            // File mappings need a VFS way to export a MemoryObject.
            return Err(ENODEV);
        }
        if !matches!(
            flags & MAP_TYPE,
            MAP_SHARED | MAP_PRIVATE | MAP_SHARED_VALIDATE
        ) {
            return Err(EINVAL);
        }
        let protections = protections(prot)?;
        let replace = flags & MAP_FIXED != 0;
        let fixed = replace || flags & MAP_FIXED_NOREPLACE != 0;
        if fixed && (address == 0 || !address.is_multiple_of(PAGE_SIZE)) {
            return Err(EINVAL);
        }
        let read_write =
            DwMemoryProtection(DW_MEMORY_PROTECTION_READ.0 | DW_MEMORY_PROTECTION_WRITE.0);
        // Ordinary requests come from the arenas. Their pages are read-write
        // whatever `prot` asked short of execute, which POSIX allows.
        if !fixed && protections.0 & DW_MEMORY_PROTECTION_EXECUTE.0 == 0 {
            return self.arena_allocate(native, root, length / PAGE_SIZE, read_write);
        }
        if fixed && self.arenas.place(address, length) != Place::Outside {
            // An arena is never split or replaced.
            return Err(if replace { EINVAL } else { EEXIST });
        }
        if replace {
            // FIXED never replaces natively. Not atomic, which is fine
            // without threads (design §5).
            let _ = native.unmap(root, address, length);
        }
        let mapped = native
            .map_anonymous(
                root,
                if fixed { address } else { 0 },
                length,
                protections,
                fixed,
            )
            .map_err(|status| match status {
                DW_STATUS_ALREADY_EXISTS => EEXIST,
                DW_STATUS_NO_MEMORY | DW_STATUS_NO_RESOURCES | DW_STATUS_BAD_ADDRESS => ENOMEM,
                DW_STATUS_ACCESS_DENIED => EACCES,
                _ => EINVAL,
            })?;
        Ok(mapped as usize)
    }

    pub fn munmap<N: Native>(
        &mut self,
        native: &mut N,
        address: u64,
        length: u64,
    ) -> Result<usize> {
        let root = self.self_root.ok_or(EINVAL)?;
        if !address.is_multiple_of(PAGE_SIZE) || length == 0 {
            return Err(EINVAL);
        }
        let length = round_pages(length).ok_or(EINVAL)?;
        match self.arenas.place(address, length) {
            Place::Inside(index) => {
                self.arenas.release(index, address, length);
                Ok(0)
            }
            Place::Straddling => Err(EINVAL),
            Place::Outside => {
                native.unmap(root, address, length).map_err(|_| EINVAL)?;
                Ok(0)
            }
        }
    }

    /// `pages` zeroed pages from the arenas, mapping a new arena when none
    /// has room: 4 MiB, or the request if larger, or only the request once
    /// the job's page budget is nearly spent.
    fn arena_allocate<N: Native>(
        &mut self,
        native: &mut N,
        root: DwHandle,
        pages: u64,
        read_write: DwMemoryProtection,
    ) -> Result<usize> {
        if pages > MAX_ARENA_PAGES {
            return Err(ENOMEM);
        }
        if let Some((address, dirty)) = self.arenas.allocate(pages) {
            if dirty {
                native.zero(address, pages * PAGE_SIZE);
            }
            return Ok(address as usize);
        }
        if self.arenas.is_full() {
            return Err(ENOMEM);
        }
        let mut size = pages.max(ARENA_PAGES);
        let base = loop {
            match native.map_anonymous(root, 0, size * PAGE_SIZE, read_write, false) {
                Ok(base) => break base,
                Err(DW_STATUS_NO_RESOURCES | DW_STATUS_NO_MEMORY) if size > pages => size = pages,
                Err(DW_STATUS_ACCESS_DENIED) => return Err(EACCES),
                Err(_) => return Err(ENOMEM),
            }
        };
        let mut arena = Arena::new(base, size);
        let Some((address, _)) = arena.allocate(pages) else {
            return Err(ENOMEM);
        };
        if !self.arenas.add(arena) {
            return Err(ENOMEM);
        }
        Ok(address as usize)
    }

    pub fn mprotect<N: Native>(
        &mut self,
        native: &mut N,
        address: u64,
        length: u64,
        prot: i32,
    ) -> Result<usize> {
        let root = self.self_root.ok_or(ENOMEM)?;
        if !address.is_multiple_of(PAGE_SIZE) {
            return Err(EINVAL);
        }
        if length == 0 {
            return Ok(0);
        }
        // Arena pages stay read-write: they are reused and zeroed through
        // that mapping. Memory a program will protect must be mapped
        // executable or fixed, which is direct.
        if self
            .arenas
            .place(address, round_pages(length).ok_or(ENOMEM)?)
            != Place::Outside
        {
            return Err(ENOTSUP);
        }
        let length = round_pages(length).ok_or(ENOMEM)?;
        let protections = protections(prot)?;
        native
            .protect(root, address, length, protections)
            .map_err(|status| match status {
                DW_STATUS_ACCESS_DENIED => EACCES,
                DW_STATUS_NOT_FOUND | DW_STATUS_BAD_ADDRESS => ENOMEM,
                _ => EINVAL,
            })?;
        Ok(0)
    }

    // ---- time --------------------------------------------------------

    pub fn clock_gettime<N: Native>(
        &mut self,
        native: &mut N,
        clock: i32,
        out: &mut Timespec,
    ) -> Result<usize> {
        match clock {
            // REALTIME counts from boot until a time service publishes the
            // boot epoch (request W6).
            CLOCK_REALTIME
            | CLOCK_REALTIME_COARSE
            | CLOCK_MONOTONIC
            | CLOCK_MONOTONIC_RAW
            | CLOCK_MONOTONIC_COARSE
            | CLOCK_BOOTTIME => {
                let now = native.now().map_err(from_status)?;
                *out = timespec(now);
                Ok(0)
            }
            _ => Err(EINVAL),
        }
    }

    pub fn clock_getres(&self, clock: i32, out: Option<&mut Timespec>) -> Result<usize> {
        match clock {
            CLOCK_REALTIME
            | CLOCK_REALTIME_COARSE
            | CLOCK_MONOTONIC
            | CLOCK_MONOTONIC_RAW
            | CLOCK_MONOTONIC_COARSE
            | CLOCK_BOOTTIME => {
                if let Some(out) = out {
                    *out = Timespec {
                        seconds: 0,
                        nanoseconds: CLOCK_RESOLUTION_NS,
                    };
                }
                Ok(0)
            }
            _ => Err(EINVAL),
        }
    }

    /// Sleeps until `deadline` on the monotonic clock.
    fn sleep_until<N: Native>(&mut self, native: &mut N, deadline: u64) -> Result<usize> {
        let (waiter, _) = match self.sleeper {
            Some(pair) => pair,
            None => {
                let pair = native
                    .create_channel(DwRights(
                        DW_RIGHT_READ.0 | DW_RIGHT_WRITE.0 | DW_RIGHT_WAIT.0,
                    ))
                    .map_err(|_| EAGAIN)?;
                self.sleeper = Some(pair);
                pair
            }
        };
        loop {
            match native.wait(waiter, DW_SIGNAL_READABLE, DwDeadline(deadline)) {
                Err(DW_STATUS_TIMED_OUT) => return Ok(0),
                Err(DW_STATUS_INTERRUPTED) => continue,
                // Nothing ever writes to the sleeper; anything else is a
                // broken native contract.
                _ => return Err(EINVAL),
            }
        }
    }

    pub fn nanosleep<N: Native>(&mut self, native: &mut N, request: &Timespec) -> Result<usize> {
        let duration = nanoseconds(request)?;
        let now = native.now().map_err(from_status)?;
        self.sleep_until(native, now.saturating_add(duration))
    }

    pub fn clock_nanosleep<N: Native>(
        &mut self,
        native: &mut N,
        clock: i32,
        flags: i32,
        request: &Timespec,
    ) -> Result<usize> {
        match clock {
            CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_BOOTTIME => {}
            _ => return Err(EINVAL),
        }
        let value = nanoseconds(request)?;
        if flags & TIMER_ABSTIME != 0 {
            self.sleep_until(native, value)
        } else {
            let now = native.now().map_err(from_status)?;
            self.sleep_until(native, now.saturating_add(value))
        }
    }
}

fn fill_stat(out: &mut Kstat, attr: &Attr, id: u64) {
    out.st_ino = if id != 0 { id } else { 1 };
    match attr.kind {
        Kind::Directory => out.st_mode = S_IFDIR | 0o755,
        Kind::File => {
            out.st_mode = S_IFREG | 0o644;
            out.st_size = i64::try_from(attr.size).unwrap_or(i64::MAX);
            out.st_blocks = (out.st_size + 511) / 512;
        }
    }
}

fn round_pages(length: u64) -> Option<u64> {
    length
        .checked_add(PAGE_SIZE - 1)
        .map(|end| end & !(PAGE_SIZE - 1))
}

/// POSIX protections as native ones. W+X and PROT_NONE cannot be
/// expressed (request D3).
pub fn protections(prot: i32) -> Result<DwMemoryProtection> {
    if prot & !(PROT_READ | PROT_WRITE | PROT_EXEC) != 0 {
        return Err(EINVAL);
    }
    let write = prot & PROT_WRITE != 0;
    let execute = prot & PROT_EXEC != 0;
    Ok(match (prot == 0, write, execute) {
        (true, _, _) | (_, true, true) => return Err(ENOTSUP),
        (_, true, false) => {
            DwMemoryProtection(DW_MEMORY_PROTECTION_READ.0 | DW_MEMORY_PROTECTION_WRITE.0)
        }
        (_, false, true) => {
            DwMemoryProtection(DW_MEMORY_PROTECTION_READ.0 | DW_MEMORY_PROTECTION_EXECUTE.0)
        }
        (_, false, false) => DW_MEMORY_PROTECTION_READ,
    })
}

fn timespec(nanoseconds: u64) -> Timespec {
    Timespec {
        seconds: (nanoseconds / 1_000_000_000) as i64,
        nanoseconds: (nanoseconds % 1_000_000_000) as i64,
    }
}

fn nanoseconds(value: &Timespec) -> Result<u64> {
    if value.seconds < 0 || !(0..1_000_000_000).contains(&value.nanoseconds) {
        return Err(EINVAL);
    }
    Ok((value.seconds as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(value.nanoseconds as u64))
}
