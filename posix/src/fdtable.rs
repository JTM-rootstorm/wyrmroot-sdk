// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
//! The file descriptor table (root `TOOLCHAIN_POSIX_LAYER_DESIGN.md` §3).
//!
//! Descriptors name open file descriptions, which hold the handles and the
//! protocol state. `dup` and friends share one description inside the
//! process, so no kernel duplicate is needed; a description is handed back
//! to the caller for closing only when its last descriptor goes. New
//! descriptors are the lowest free number, as POSIX requires.

use crate::errno::{EBADF, EINVAL, EMFILE, Result};

/// `RLIMIT_NOFILE` for the slice.
pub const MAX_FDS: usize = 1024;

struct Slot<T> {
    file: T,
    references: u32,
}

pub struct FdTable<T, const FILES: usize> {
    /// Per descriptor: 0 when free, else the description's index plus one.
    fds: [u16; MAX_FDS],
    cloexec: [u64; MAX_FDS / 64],
    files: [Option<Slot<T>>; FILES],
}

impl<T, const FILES: usize> Default for FdTable<T, FILES> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const FILES: usize> FdTable<T, FILES> {
    pub const fn new() -> Self {
        assert!(FILES < u16::MAX as usize);
        Self {
            fds: [0; MAX_FDS],
            cloexec: [0; MAX_FDS / 64],
            files: [const { None }; FILES],
        }
    }

    fn index(&self, fd: i32) -> Result<usize> {
        let fd = usize::try_from(fd).map_err(|_| EBADF)?;
        match self.fds.get(fd) {
            Some(&slot) if slot != 0 => Ok(usize::from(slot - 1)),
            _ => Err(EBADF),
        }
    }

    fn lowest_free(&self, minimum: usize) -> Result<usize> {
        (minimum..MAX_FDS)
            .find(|&fd| self.fds[fd] == 0)
            .ok_or(EMFILE)
    }

    fn set_flag(&mut self, fd: usize, cloexec: bool) {
        let bit = 1u64 << (fd % 64);
        if cloexec {
            self.cloexec[fd / 64] |= bit;
        } else {
            self.cloexec[fd / 64] &= !bit;
        }
    }

    /// Gives a new description its first descriptor, at least `minimum`.
    /// When no descriptor or description is free the file comes back with
    /// `EMFILE`, so the caller can close its handles.
    pub fn install(
        &mut self,
        file: T,
        minimum: usize,
        cloexec: bool,
    ) -> core::result::Result<i32, (T, crate::errno::Errno)> {
        let Ok(fd) = self.lowest_free(minimum) else {
            return Err((file, EMFILE));
        };
        let Some(index) = self.files.iter().position(Option::is_none) else {
            return Err((file, EMFILE));
        };
        self.files[index] = Some(Slot {
            file,
            references: 1,
        });
        self.fds[fd] = (index + 1) as u16;
        self.set_flag(fd, cloexec);
        Ok(fd as i32)
    }

    /// The description `fd` names.
    /// Every open description, once each.
    pub fn descriptions(&self) -> impl Iterator<Item = &T> {
        self.files.iter().flatten().map(|slot| &slot.file)
    }

    pub fn get(&mut self, fd: i32) -> Result<&mut T> {
        let index = self.index(fd)?;
        match &mut self.files[index] {
            Some(slot) => Ok(&mut slot.file),
            None => Err(EBADF),
        }
    }

    /// Another descriptor for `fd`'s description: the lowest free one at
    /// least `minimum` (`F_DUPFD`, and `dup` with 0).
    pub fn dup(&mut self, fd: i32, minimum: i32, cloexec: bool) -> Result<i32> {
        let index = self.index(fd)?;
        let minimum = usize::try_from(minimum).map_err(|_| EINVAL)?;
        if minimum >= MAX_FDS {
            return Err(EINVAL);
        }
        let new = self.lowest_free(minimum)?;
        self.share(index, new, cloexec);
        Ok(new as i32)
    }

    fn share(&mut self, index: usize, fd: usize, cloexec: bool) {
        if let Some(slot) = &mut self.files[index] {
            slot.references += 1;
        }
        self.fds[fd] = (index + 1) as u16;
        self.set_flag(fd, cloexec);
    }

    /// Makes `new` name `old`'s description (`dup2`, `dup3`). Whatever `new`
    /// named before is released; a description losing its last descriptor
    /// comes back for closing. `dup2(fd, fd)` changes nothing.
    pub fn dup_to(&mut self, old: i32, new: i32, cloexec: bool) -> Result<(i32, Option<T>)> {
        let index = self.index(old)?;
        let target = usize::try_from(new).map_err(|_| EBADF)?;
        if target >= MAX_FDS {
            return Err(EBADF);
        }
        if old == new {
            return Ok((new, None));
        }
        let released = if self.fds[target] != 0 {
            self.close(new)?
        } else {
            None
        };
        self.share(index, target, cloexec);
        Ok((new, released))
    }

    /// Frees `fd`. The description comes back when this was its last
    /// descriptor, for the caller to close its handles.
    pub fn close(&mut self, fd: i32) -> Result<Option<T>> {
        let index = self.index(fd)?;
        let fd = fd as usize;
        self.fds[fd] = 0;
        self.set_flag(fd, false);
        let last = match &mut self.files[index] {
            Some(slot) => {
                slot.references -= 1;
                slot.references == 0
            }
            None => return Err(EBADF),
        };
        Ok(if last {
            self.files[index].take().map(|slot| slot.file)
        } else {
            None
        })
    }

    pub fn cloexec(&self, fd: i32) -> Result<bool> {
        self.index(fd)?;
        let fd = fd as usize;
        Ok(self.cloexec[fd / 64] & (1u64 << (fd % 64)) != 0)
    }

    pub fn set_cloexec(&mut self, fd: i32, cloexec: bool) -> Result<()> {
        self.index(fd)?;
        self.set_flag(fd as usize, cloexec);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Table = FdTable<&'static str, 4>;

    fn table() -> Box<Table> {
        Box::new(Table::new())
    }

    #[test]
    fn descriptors_are_the_lowest_free_numbers() {
        let mut fds = table();
        assert_eq!(fds.install("a", 0, false), Ok(0));
        assert_eq!(fds.install("b", 0, false), Ok(1));
        assert_eq!(fds.install("c", 0, false), Ok(2));
        assert_eq!(fds.close(1), Ok(Some("b")));
        assert_eq!(fds.install("d", 0, false), Ok(1));
        assert_eq!(fds.install("e", 10, false), Ok(10));
    }

    #[test]
    fn duplicates_share_one_description_until_the_last_close() {
        let mut fds = table();
        let fd = fds.install("file", 0, false).unwrap();
        let copy = fds.dup(fd, 0, false).unwrap();
        assert_eq!(copy, 1);
        assert_eq!(fds.close(fd), Ok(None));
        assert_eq!(*fds.get(copy).unwrap(), "file");
        assert_eq!(fds.close(copy), Ok(Some("file")));
        assert_eq!(fds.get(copy), Err(EBADF));
    }

    #[test]
    fn dup_to_replaces_and_releases_the_target() {
        let mut fds = table();
        let a = fds.install("a", 0, false).unwrap();
        let b = fds.install("b", 0, false).unwrap();
        assert_eq!(fds.dup_to(a, b, false), Ok((b, Some("b"))));
        assert_eq!(*fds.get(b).unwrap(), "a");
        // The same descriptor twice changes nothing.
        assert_eq!(fds.dup_to(a, a, false), Ok((a, None)));
        assert_eq!(fds.close(a), Ok(None));
        assert_eq!(fds.close(b), Ok(Some("a")));
        assert_eq!(fds.dup_to(7, 1, false), Err(EBADF));
        let c = fds.install("c", 0, false).unwrap();
        assert_eq!(fds.dup_to(c, MAX_FDS as i32, false), Err(EBADF));
        assert_eq!(fds.dup_to(c, -1, false), Err(EBADF));
    }

    #[test]
    fn close_on_exec_belongs_to_the_descriptor() {
        let mut fds = table();
        let fd = fds.install("x", 0, true).unwrap();
        let copy = fds.dup(fd, 0, false).unwrap();
        assert_eq!(fds.cloexec(fd), Ok(true));
        assert_eq!(fds.cloexec(copy), Ok(false));
        fds.set_cloexec(copy, true).unwrap();
        assert_eq!(fds.cloexec(copy), Ok(true));
        fds.close(fd).unwrap();
        // A reused number does not inherit the old flag.
        let again = fds.dup(copy, 0, false).unwrap();
        assert_eq!(again, fd);
        assert_eq!(fds.cloexec(again), Ok(false));
    }

    #[test]
    fn exhaustion_and_bad_numbers_fail_cleanly() {
        let mut fds = table();
        for name in ["a", "b", "c", "d"] {
            fds.install(name, 0, false).unwrap();
        }
        // Four descriptions is this table's limit; the file comes back.
        assert_eq!(fds.install("e", 0, false), Err(("e", EMFILE)));
        // Sharing needs no new description.
        assert!(fds.dup(0, 0, false).is_ok());
        assert_eq!(fds.get(-1), Err(EBADF));
        assert_eq!(fds.get(MAX_FDS as i32), Err(EBADF));
        assert_eq!(fds.close(500), Err(EBADF));
        assert_eq!(fds.dup(0, MAX_FDS as i32, false), Err(EINVAL));
        assert_eq!(fds.dup(0, -3, false), Err(EINVAL));
    }

    #[test]
    fn descriptor_numbers_run_out_at_the_limit() {
        let mut fds = table();
        let fd = fds.install("a", 0, false).unwrap();
        for expected in 1..MAX_FDS as i32 {
            assert_eq!(fds.dup(fd, 0, false), Ok(expected));
        }
        assert_eq!(fds.dup(fd, 0, false), Err(EMFILE));
    }
}
