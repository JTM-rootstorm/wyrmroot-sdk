// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
//! POSIX record locks over WRFS 1.1 locks (root
//! `TOOLCHAIN_POSIX_LAYER_DESIGN.md` W10).
//!
//! A WRFS lock belongs to the Channel it was taken on, which is what Linux
//! calls an open file description lock, so `F_OFD_*` go straight to the
//! descriptor's own Channel. A classic `fcntl` lock belongs to the process
//! instead: two descriptions of one file in the same process never conflict,
//! and closing any descriptor of the file releases all of the process's
//! locks on it. So the process takes all of its classic locks on a file
//! through one extra Channel, opened again from a description of that file
//! and keyed by the file's identity (`st_dev`, `st_ino`); closing a
//! descriptor of the file unlocks everything on it, and the Channel goes
//! when no description of the file is left. A job that ends closes every
//! Channel, which releases every lock it held.
//!
//! The server never waits, so `F_SETLKW` retries with a growing sleep. The
//! holder of a conflicting lock is not known: `F_GETLK` reports `l_pid` -1,
//! as Linux does for description locks.

use super::{FileKind, Process};
use crate::errno::*;
use crate::native::Native;
use crate::node;
use crate::types::*;
use deepwyrm_syscall::DwHandle;
use wyrmroot_fs_proto::node::{Body, Lock, LockMode};
use wyrmroot_fs_proto::{OPEN_READ, OPEN_WRITE, Status};

/// The most files one process holds classic locks on at once.
pub const MAX_LOCKED_FILES: usize = 16;

/// The first and longest `F_SETLKW` retry sleeps.
const FIRST_RETRY_NS: u64 = 1_000_000;
const LONGEST_RETRY_NS: u64 = 50_000_000;

/// A file as the process sees it: (`st_dev`, `st_ino`).
type Identity = (u64, u64);

#[derive(Clone, Copy, Debug)]
pub(super) struct LockOwner {
    file: Identity,
    channel: DwHandle,
}

/// What a lock command asks, once its descriptor is resolved.
struct Request {
    channel: DwHandle,
    file: Option<Identity>,
    lock: Lock,
    /// WRFS flags to open the file again with, for a new lock Channel.
    reopen: u32,
}

impl Process {
    /// `fcntl`'s lock commands; `flock` is the caller's `struct flock`.
    pub fn fcntl_lock<N: Native>(
        &mut self,
        native: &mut N,
        fd: i32,
        command: i32,
        flock: &mut Flock,
    ) -> Result<usize> {
        let description = matches!(command, F_OFD_GETLK | F_OFD_SETLK | F_OFD_SETLKW);
        if description && flock.l_pid != 0 {
            return Err(EINVAL);
        }
        let testing = matches!(command, F_GETLK | F_OFD_GETLK);
        let request = self.lock_request(native, fd, flock, testing)?;
        // Description locks, and classic ones on a file without a lock
        // Channel yet (so the process holds none there), use the
        // descriptor's own Channel to test or unlock.
        let owner = if description {
            None
        } else {
            request.file.and_then(|file| self.lock_owner(file))
        };
        if testing {
            let channel = owner.map_or(request.channel, |owner| owner.channel);
            return self.test_lock(native, channel, request.lock, flock);
        }
        let waiting = matches!(command, F_SETLKW | F_OFD_SETLKW);
        if description {
            return self.set_lock(native, request.channel, request.lock, waiting);
        }
        let Some(file) = request.file else {
            return Err(ENOLCK);
        };
        if let Some(owner) = owner {
            return self.set_lock(native, owner.channel, request.lock, waiting);
        }
        if request.lock.mode == LockMode::Unlock {
            return Ok(0);
        }
        let slot = self
            .lock_owners
            .iter()
            .position(Option::is_none)
            .ok_or(ENOLCK)?;
        // No Channel for the lock is no lock, whatever ran out.
        let channel = node::reopen(native, request.channel, &mut self.scratch, request.reopen)
            .map_err(|_| ENOLCK)?;
        match self.set_lock(native, channel, request.lock, waiting) {
            Ok(done) => {
                self.lock_owners[slot] = Some(LockOwner { file, channel });
                Ok(done)
            }
            Err(error) => {
                let _ = native.close(channel);
                Err(error)
            }
        }
    }

    /// Resolves `flock` against `fd`: the descriptor's Channel, the file,
    /// and the absolute range and mode.
    fn lock_request<N: Native>(
        &mut self,
        native: &mut N,
        fd: i32,
        flock: &Flock,
        testing: bool,
    ) -> Result<Request> {
        let Self { files, scratch, .. } = self;
        let file = files.get(fd)?;
        let (readable, writable) = (file.readable(), file.writable());
        let FileKind::Node(node) = &file.kind else {
            return Err(EINVAL);
        };
        if file.flags & O_PATH != 0 {
            return Err(EBADF);
        }
        let mode = match flock.l_type {
            F_RDLCK if testing || readable => LockMode::Shared,
            F_WRLCK if testing || writable => LockMode::Exclusive,
            F_RDLCK | F_WRLCK => return Err(EBADF),
            F_UNLCK if !testing => LockMode::Unlock,
            _ => return Err(EINVAL),
        };
        let base = match i32::from(flock.l_whence) {
            SEEK_SET => 0,
            SEEK_CUR => i64::try_from(node.offset).map_err(|_| EOVERFLOW)?,
            SEEK_END => i64::try_from(node::get_attr(native, node.channel, scratch)?.size)
                .map_err(|_| EOVERFLOW)?,
            _ => return Err(EINVAL),
        };
        let start = base.checked_add(flock.l_start).ok_or(EOVERFLOW)?;
        // A negative length covers the bytes before `start`.
        let (start, length) = if flock.l_len < 0 {
            (
                start.checked_add(flock.l_len).ok_or(EINVAL)?,
                flock.l_len.checked_neg().ok_or(EINVAL)?,
            )
        } else {
            (start, flock.l_len)
        };
        if start < 0 {
            return Err(EINVAL);
        }
        // The last byte locked must be a valid offset.
        if length > 0 && start.checked_add(length - 1).is_none() {
            return Err(EOVERFLOW);
        }
        let lock = Lock {
            start: start as u64,
            length: length as u64,
            mode,
        };
        let reopen = if readable { OPEN_READ } else { 0 } | if writable { OPEN_WRITE } else { 0 };
        Ok(Request {
            channel: node.channel,
            file: (node.filesystem != 0 && node.id != 0).then_some((node.filesystem, node.id)),
            lock,
            reopen,
        })
    }

    fn lock_owner(&self, file: Identity) -> Option<LockOwner> {
        self.lock_owners
            .iter()
            .flatten()
            .find(|owner| owner.file == file)
            .copied()
    }

    fn set_lock<N: Native>(
        &mut self,
        native: &mut N,
        channel: DwHandle,
        lock: Lock,
        waiting: bool,
    ) -> Result<usize> {
        let mut retry = FIRST_RETRY_NS;
        loop {
            match node::status(native, channel, &mut self.scratch, Body::Lock(lock)) {
                Ok(_) => return Ok(0),
                Err(EBUSY) if waiting => {
                    let now = native.now().map_err(from_status)?;
                    self.sleep_until(native, now.saturating_add(retry))?;
                    retry = (retry * 2).min(LONGEST_RETRY_NS);
                }
                Err(EBUSY) => return Err(EAGAIN),
                Err(ENFILE | ENOTSUP) => return Err(ENOLCK),
                Err(error) => return Err(error),
            }
        }
    }

    fn test_lock<N: Native>(
        &mut self,
        native: &mut N,
        channel: DwHandle,
        lock: Lock,
        flock: &mut Flock,
    ) -> Result<usize> {
        let used = node::request(native, channel, &mut self.scratch, Body::TestLock(lock))?;
        let held = match wyrmroot_fs_proto::node::parse(&self.scratch.reply[..used], 0)
            .map(|message| message.body)
        {
            Ok(Body::LockInfo(held)) => held,
            Ok(Body::Status {
                status: Status::NotSupported,
                ..
            }) => return Err(ENOLCK),
            Ok(Body::Status { status, .. }) => return Err(from_fs(status)),
            _ => return Err(EIO),
        };
        match held {
            None => flock.l_type = F_UNLCK,
            Some(held) => {
                flock.l_type = if held.mode == LockMode::Exclusive {
                    F_WRLCK
                } else {
                    F_RDLCK
                };
                flock.l_whence = SEEK_SET as i16;
                flock.l_start = i64::try_from(held.start).map_err(|_| EOVERFLOW)?;
                flock.l_len = i64::try_from(held.length).map_err(|_| EOVERFLOW)?;
                flock.l_pid = -1;
            }
        }
        Ok(0)
    }

    /// The file `fd` names, for [`Self::closed_file`].
    pub(super) fn file_identity(&mut self, fd: i32) -> Option<Identity> {
        match &self.files.get(fd).ok()?.kind {
            FileKind::Node(node) if node.filesystem != 0 && node.id != 0 => {
                Some((node.filesystem, node.id))
            }
            _ => None,
        }
    }

    /// After a descriptor of `file` closed: the process's classic locks on
    /// it are released, and its lock Channel goes once no description of
    /// the file is left open.
    pub(super) fn closed_file<N: Native>(&mut self, native: &mut N, file: Option<Identity>) {
        let Some(file) = file else {
            return;
        };
        let Some(slot) = self
            .lock_owners
            .iter()
            .position(|owner| owner.is_some_and(|owner| owner.file == file))
        else {
            return;
        };
        let channel = self.lock_owners[slot].map(|owner| owner.channel);
        let still_open = self.files.descriptions().any(
            |open| matches!(&open.kind, FileKind::Node(node) if (node.filesystem, node.id) == file),
        );
        let Some(channel) = channel else {
            return;
        };
        if still_open {
            let everything = Lock {
                start: 0,
                length: 0,
                mode: LockMode::Unlock,
            };
            if node::status(native, channel, &mut self.scratch, Body::Lock(everything)).is_ok() {
                return;
            }
        }
        // Closing the Channel releases whatever it still held.
        self.lock_owners[slot] = None;
        let _ = native.close(channel);
    }
}
