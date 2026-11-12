// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
//! The C ABI: one `__wr_*` function per operation, each returning a
//! non-negative result or `-errno` (root `TOOLCHAIN_POSIX_LAYER_DESIGN.md`
//! §1). musl's `syscall_arch.h` reaches them through the dispatcher in the
//! SDK's musl port; `wr_posix.h` is generated from [`SIGNATURES`] by the
//! `wr-posix-header` host program, never written by hand.
//!
//! The function bodies exist only for the guest. On the host only the
//! signature table is compiled, so no host program can call into the
//! kernel interface.

#![allow(clippy::missing_safety_doc)]

#[cfg(target_os = "wyrmroot")]
use crate::{errno::*, native::Kernel, process::Process, types::*};
#[cfg(target_os = "wyrmroot")]
use core::ffi::{c_char, c_int, c_long, c_ulong, c_void};
#[cfg(target_os = "wyrmroot")]
use deepwyrm_syscall::DwHandle;

/// One exported function: its name, `(name, Rust type)` arguments and
/// Rust return type, as written in this file.
#[derive(Clone, Copy, Debug)]
pub struct Signature {
    pub name: &'static str,
    pub arguments: &'static [(&'static str, &'static str)],
    pub returns: &'static str,
}

macro_rules! exports {
    ($(fn $name:ident($($argument:ident: $type:ty),* $(,)?) -> $returns:ty $body:block)*) => {
        $(
            #[cfg(target_os = "wyrmroot")]
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn $name($($argument: $type),*) -> $returns $body
        )*
        /// Every export, in this file's order, after the two that never
        /// return a value.
        pub const SIGNATURES: &[Signature] = &[
            Signature {
                name: "__wr_posix_init",
                arguments: &[("launch", "u64"), ("version", "u64"), ("stack", "*const usize")],
                returns: "()",
            },
            Signature {
                name: "__wr_exit",
                arguments: &[("code", "c_int")],
                returns: "!",
            },
            $(Signature {
                name: stringify!($name),
                arguments: &[$((stringify!($argument), stringify!($type))),*],
                returns: stringify!($returns),
            }),*
        ];
    };
}

// ---- process-wide state ---------------------------------------------------

#[cfg(target_os = "wyrmroot")]
mod global {
    use crate::process::Process;
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicBool, Ordering};

    pub struct State {
        locked: AtomicBool,
        process: UnsafeCell<Process>,
    }

    // SAFETY: the process is reached only through `with`, which holds the
    // lock for the whole borrow.
    unsafe impl Sync for State {}

    pub static STATE: State = State {
        locked: AtomicBool::new(false),
        process: UnsafeCell::new(Process::new()),
    };

    /// Runs `f` with exclusive access to the process state. Not reentrant:
    /// no operation calls back into the layer. A blocking operation holds
    /// the lock while it waits, which is sound while a process has one
    /// thread; pthreads (step 3) must revisit it.
    pub fn with<R>(f: impl FnOnce(&mut Process) -> R) -> R {
        while STATE
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        // SAFETY: the lock is held, so this is the only reference.
        let result = f(unsafe { &mut *STATE.process.get() });
        STATE.locked.store(false, Ordering::Release);
        result
    }
}

#[cfg(target_os = "wyrmroot")]
fn with<R>(f: impl FnOnce(&mut Process, &mut Kernel) -> R) -> R {
    global::with(|process| f(process, &mut Kernel))
}

#[cfg(target_os = "wyrmroot")]
fn fd(value: c_int) -> i32 {
    value
}

/// A NUL-terminated path from C.
#[cfg(target_os = "wyrmroot")]
unsafe fn c_path<'a>(pointer: *const c_char) -> Result<&'a [u8]> {
    if pointer.is_null() {
        return Err(EFAULT);
    }
    let limit = wyrmroot_fs_proto::MAX_PATH_BYTES * 4;
    for length in 0..=limit {
        // SAFETY: C promises a NUL-terminated string; we stop at the NUL.
        if unsafe { *pointer.add(length) } == 0 {
            // SAFETY: the bytes before the NUL were just read.
            return Ok(unsafe { core::slice::from_raw_parts(pointer.cast::<u8>(), length) });
        }
    }
    Err(ENAMETOOLONG)
}

#[cfg(target_os = "wyrmroot")]
unsafe fn user_buffer<'a>(pointer: *mut c_void, length: usize) -> Result<&'a mut [u8]> {
    if length == 0 {
        return Ok(&mut []);
    }
    if pointer.is_null() {
        return Err(EFAULT);
    }
    if length > isize::MAX as usize {
        return Err(EINVAL);
    }
    // SAFETY: C promises `length` writable bytes at `pointer`.
    Ok(unsafe { core::slice::from_raw_parts_mut(pointer.cast::<u8>(), length) })
}

#[cfg(target_os = "wyrmroot")]
unsafe fn user_bytes<'a>(pointer: *const c_void, length: usize) -> Result<&'a [u8]> {
    if length == 0 {
        return Ok(&[]);
    }
    if pointer.is_null() {
        return Err(EFAULT);
    }
    if length > isize::MAX as usize {
        return Err(EINVAL);
    }
    // SAFETY: C promises `length` readable bytes at `pointer`.
    Ok(unsafe { core::slice::from_raw_parts(pointer.cast::<u8>(), length) })
}

#[cfg(target_os = "wyrmroot")]
unsafe fn user_ref<'a, T>(pointer: *mut T) -> Result<&'a mut T> {
    // SAFETY: C promises a valid, aligned object when non-null.
    unsafe { pointer.as_mut() }.ok_or(EFAULT)
}

#[cfg(target_os = "wyrmroot")]
fn r(result: Result<usize>) -> c_long {
    ret(result) as c_long
}

/// `IOV_MAX`.
#[cfg(target_os = "wyrmroot")]
const IOV_MAX: c_int = 1024;

#[cfg(target_os = "wyrmroot")]
unsafe fn vectors<'a>(iov: *const Iovec, count: c_int) -> Result<&'a [Iovec]> {
    if !(0..=IOV_MAX).contains(&count) {
        return Err(EINVAL);
    }
    if count == 0 {
        return Ok(&[]);
    }
    if iov.is_null() {
        return Err(EFAULT);
    }
    // SAFETY: C promises `count` iovec records at `iov`.
    Ok(unsafe { core::slice::from_raw_parts(iov, count as usize) })
}

// ---- startup and exit -----------------------------------------------------

/// Called by `_start` before musl: runs the launch exchange. A failure ends
/// the process with `0x5750_0000 | step`.
#[cfg(target_os = "wyrmroot")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wr_posix_init(launch: u64, version: u64, stack: *const usize) {
    // The startup block: argc, argv..., NULL, envp..., NULL, auxv.
    let mut pwd: Option<&[u8]> = None;
    if !stack.is_null() {
        // SAFETY: the loader builds this block; argc and the NULL
        // terminators bound every read.
        unsafe {
            let argc = *stack;
            let mut entry = stack.add(1 + argc + 1).cast::<*const c_char>();
            while !(*entry).is_null() {
                if let Ok(text) = c_path(*entry)
                    && let Some(value) = crate::startup::find_pwd(core::iter::once(text))
                {
                    pwd = Some(value);
                }
                entry = entry.add(1);
            }
        }
    }
    let started = with(|process, kernel| {
        crate::startup::start(kernel, process, DwHandle(launch), version, pwd)
    });
    if let Err(failure) = started {
        deepwyrm_syscall::process_exit(failure.exit_code());
        loop {
            core::hint::spin_loop();
        }
    }
}

/// `_exit`: the low 8 bits of `code` are the exit status.
#[cfg(target_os = "wyrmroot")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wr_exit(code: c_int) -> ! {
    deepwyrm_syscall::process_exit((code & 0xff) as u32);
    loop {
        core::hint::spin_loop();
    }
}

exports! {
    fn __wr_read(file: c_int, buffer: *mut c_void, length: usize) -> c_long {
        r((|| {
            // SAFETY: forwarded from the caller's contract.
            let buffer = unsafe { user_buffer(buffer, length)? };
            with(|process, kernel| process.read(kernel, fd(file), buffer))
        })())
    }

    fn __wr_write(file: c_int, buffer: *const c_void, length: usize) -> c_long {
        r((|| {
            // SAFETY: forwarded from the caller's contract.
            let bytes = unsafe { user_bytes(buffer, length)? };
            with(|process, kernel| process.write(kernel, fd(file), bytes))
        })())
    }

    fn __wr_readv(file: c_int, iov: *const Iovec, count: c_int) -> c_long {
        r((|| {
            // SAFETY: forwarded from the caller's contract.
            let vectors = unsafe { vectors(iov, count)? };
            let mut total = 0usize;
            for vector in vectors {
                // SAFETY: each iovec describes caller-owned memory.
                let buffer = unsafe { user_buffer(vector.base.cast(), vector.length)? };
                let wanted = buffer.len();
                match with(|process, kernel| process.read(kernel, fd(file), buffer)) {
                    Ok(count) => {
                        total += count;
                        if count < wanted {
                            break;
                        }
                    }
                    Err(_) if total != 0 => break,
                    Err(error) => return Err(error),
                }
            }
            Ok(total)
        })())
    }

    fn __wr_writev(file: c_int, iov: *const Iovec, count: c_int) -> c_long {
        r((|| {
            // SAFETY: forwarded from the caller's contract.
            let vectors = unsafe { vectors(iov, count)? };
            let mut total = 0usize;
            for vector in vectors {
                // SAFETY: each iovec describes caller-owned memory.
                let bytes = unsafe { user_bytes(vector.base.cast_const().cast(), vector.length)? };
                match with(|process, kernel| process.write(kernel, fd(file), bytes)) {
                    Ok(count) => {
                        total += count;
                        if count < bytes.len() {
                            break;
                        }
                    }
                    Err(_) if total != 0 => break,
                    Err(error) => return Err(error),
                }
            }
            Ok(total)
        })())
    }

    fn __wr_close(file: c_int) -> c_long {
        r(with(|process, kernel| process.close(kernel, fd(file))))
    }

    fn __wr_lseek(file: c_int, offset: c_long, whence: c_int) -> c_long {
        r(with(|process, kernel| process.lseek(kernel, fd(file), offset, whence)))
    }

    fn __wr_openat(directory: c_int, path: *const c_char, flags: c_int, mode: c_int) -> c_long {
        let _ = mode;
        r((|| {
            // SAFETY: forwarded from the caller's contract.
            let path = unsafe { c_path(path)? };
            with(|process, kernel| process.openat(kernel, directory, path, flags))
        })())
    }

    fn __wr_fstat(file: c_int, status: *mut Kstat) -> c_long {
        r((|| {
            // SAFETY: forwarded from the caller's contract.
            let status = unsafe { user_ref(status)? };
            with(|process, kernel| process.fstat(kernel, fd(file), status))
        })())
    }

    fn __wr_fstatat(directory: c_int, path: *const c_char, status: *mut Kstat, flags: c_int) -> c_long {
        r((|| {
            // SAFETY: forwarded from the caller's contract.
            let (path, status) = unsafe { (c_path(path)?, user_ref(status)?) };
            with(|process, kernel| process.fstatat(kernel, directory, path, status, flags))
        })())
    }

    fn __wr_faccessat(directory: c_int, path: *const c_char, mode: c_int, flags: c_int) -> c_long {
        r((|| {
            // SAFETY: forwarded from the caller's contract.
            let path = unsafe { c_path(path)? };
            with(|process, kernel| process.faccessat(kernel, directory, path, mode, flags))
        })())
    }

    fn __wr_getdents64(file: c_int, buffer: *mut c_void, length: usize) -> c_long {
        r((|| {
            // SAFETY: forwarded from the caller's contract.
            let buffer = unsafe { user_buffer(buffer, length)? };
            with(|process, kernel| process.getdents(kernel, fd(file), buffer))
        })())
    }

    fn __wr_mkdirat(directory: c_int, path: *const c_char, mode: c_int) -> c_long {
        let _ = mode;
        r((|| {
            // SAFETY: forwarded from the caller's contract.
            let path = unsafe { c_path(path)? };
            with(|process, kernel| process.mkdirat(kernel, directory, path))
        })())
    }

    fn __wr_unlinkat(directory: c_int, path: *const c_char, flags: c_int) -> c_long {
        r((|| {
            // SAFETY: forwarded from the caller's contract.
            let path = unsafe { c_path(path)? };
            with(|process, kernel| process.unlinkat(kernel, directory, path, flags))
        })())
    }

    fn __wr_getcwd(buffer: *mut c_char, size: usize) -> c_long {
        r((|| {
            // SAFETY: forwarded from the caller's contract.
            let buffer = unsafe { user_buffer(buffer.cast(), size)? };
            with(|process, _| process.getcwd(buffer))
        })())
    }

    fn __wr_chdir(path: *const c_char) -> c_long {
        r((|| {
            // SAFETY: forwarded from the caller's contract.
            let path = unsafe { c_path(path)? };
            with(|process, kernel| process.chdir(kernel, path))
        })())
    }

    fn __wr_fchdir(file: c_int) -> c_long {
        r(with(|process, _| process.fchdir(fd(file))))
    }

    fn __wr_dup(file: c_int) -> c_long {
        r(with(|process, _| process.dup(fd(file))))
    }

    fn __wr_dup3(old: c_int, new: c_int, flags: c_int) -> c_long {
        r(with(|process, kernel| process.dup3(kernel, old, new, flags)))
    }

    fn __wr_fcntl(file: c_int, command: c_int, argument: c_ulong) -> c_long {
        r(with(|process, _| process.fcntl(fd(file), command, argument)))
    }

    fn __wr_ioctl(file: c_int, request: c_ulong, argument: *mut c_void) -> c_long {
        let _ = argument;
        r(with(|process, _| process.ioctl(fd(file), request)))
    }

    fn __wr_fsync(file: c_int) -> c_long {
        r(with(|process, kernel| process.fsync(kernel, fd(file))))
    }

    fn __wr_ftruncate(file: c_int, length: c_long) -> c_long {
        r(with(|process, kernel| process.ftruncate(kernel, fd(file), length)))
    }

    fn __wr_mmap(
        address: *mut c_void,
        length: usize,
        prot: c_int,
        flags: c_int,
        file: c_int,
        offset: c_long,
    ) -> c_long {
        let _ = (file, offset);
        r(with(|process, kernel| {
            process.mmap(kernel, address as u64, length as u64, prot, flags)
        }))
    }

    fn __wr_munmap(address: *mut c_void, length: usize) -> c_long {
        r(with(|process, kernel| process.munmap(kernel, address as u64, length as u64)))
    }

    fn __wr_mprotect(address: *mut c_void, length: usize, prot: c_int) -> c_long {
        r(with(|process, kernel| {
            process.mprotect(kernel, address as u64, length as u64, prot)
        }))
    }

    fn __wr_clock_gettime(clock: c_int, out: *mut Timespec) -> c_long {
        r((|| {
            // SAFETY: forwarded from the caller's contract.
            let out = unsafe { user_ref(out)? };
            with(|process, kernel| process.clock_gettime(kernel, clock, out))
        })())
    }

    fn __wr_clock_getres(clock: c_int, out: *mut Timespec) -> c_long {
        // SAFETY: a null result pointer is allowed.
        let out = unsafe { out.as_mut() };
        r(with(|process, _| process.clock_getres(clock, out)))
    }

    fn __wr_nanosleep(request: *const Timespec, remaining: *mut Timespec) -> c_long {
        let _ = remaining;
        r((|| {
            // SAFETY: forwarded from the caller's contract.
            let request = unsafe { *user_ref(request.cast_mut())? };
            with(|process, kernel| process.nanosleep(kernel, &request))
        })())
    }

    fn __wr_clock_nanosleep(
        clock: c_int,
        flags: c_int,
        request: *const Timespec,
        remaining: *mut Timespec,
    ) -> c_long {
        let _ = remaining;
        r((|| {
            // SAFETY: forwarded from the caller's contract.
            let request = unsafe { *user_ref(request.cast_mut())? };
            with(|process, kernel| process.clock_nanosleep(kernel, clock, flags, &request))
        })())
    }

    fn __wr_set_thread_area(pointer: *mut c_void) -> c_long {
        use crate::native::Native;
        r(Kernel
            .set_fs_base(pointer as u64)
            .map(|()| 0)
            .map_err(|_| EINVAL))
    }

    fn __wr_uname(out: *mut Utsname) -> c_long {
        r((|| {
            // SAFETY: forwarded from the caller's contract.
            let out = unsafe { user_ref(out)? };
            let fields: [&[u8]; 6] = [b"Wyrmroot", b"wyrmroot", b"0", b"0", b"x86_64", b"(none)"];
            for (field, text) in out.fields.iter_mut().zip(fields) {
                field.fill(0);
                field[..text.len()].copy_from_slice(text);
            }
            Ok(0)
        })())
    }

    fn __wr_sigprocmask(how: c_int, set: *const c_void, old: *mut c_void, size: usize) -> c_long {
        // No asynchronous signals: the mask is always empty and changing it
        // has no effect (design §9).
        let _ = (how, set);
        r((|| {
            // SAFETY: forwarded from the caller's contract.
            let old = unsafe { user_buffer(old, size)? };
            old.fill(0);
            Ok(0)
        })())
    }

    fn __wr_raise(signal: c_int) -> c_long {
        // Default disposition for a self-sent signal: the process ends with
        // the shell's `128 + signal` convention. Handlers are not run yet.
        if !(1..=64).contains(&signal) {
            return r(Err(EINVAL));
        }
        deepwyrm_syscall::process_exit(128 + signal as u32);
        loop {
            core::hint::spin_loop();
        }
    }
}

#[cfg(target_os = "wyrmroot")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    deepwyrm_syscall::process_exit(0x5750_ffff);
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_export_is_named_once_and_typed_for_c() {
        let mut names: Vec<&str> = SIGNATURES.iter().map(|signature| signature.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), SIGNATURES.len());
        assert!(
            SIGNATURES
                .iter()
                .all(|signature| signature.name.starts_with("__wr_"))
        );
        assert!(names.contains(&"__wr_posix_init") && names.contains(&"__wr_write"));
    }
}
