/* SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0 */
/*
 * musl's system call numbers, mapped onto the Wyrmroot POSIX layer. The
 * Linux numbering stays here, in musl's tree; the layer only exports
 * __wr_* functions (root TOOLCHAIN_POSIX_LAYER_DESIGN.md section 1). A
 * call the layer does not provide is -ENOSYS, never a silent success.
 */
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <string.h>
#include "syscall.h"
#include "ksigaction.h"
#include "wr_posix.h"

#define ARCH_SET_FS 0x1002

/* This process is its own only task: kill and tkill reach only itself. */
static long self_signal(long target, long sig)
{
	if (target != 0 && target != 1 && target != -1) return -ESRCH;
	if (sig < 0 || sig > 64) return -EINVAL;
	if (sig == 0) return 0;
	return __wr_raise(sig);
}

hidden long __wr_syscall(long n, long a, long b, long c, long d, long e, long f)
{
	switch (n) {
	case SYS_read: return __wr_read(a, (void *)b, c);
	case SYS_write: return __wr_write(a, (const void *)b, c);
	case SYS_readv: return __wr_readv(a, (const struct iovec *)b, c);
	case SYS_writev: return __wr_writev(a, (const struct iovec *)b, c);
	case SYS_close: return __wr_close(a);
	case SYS_lseek: return __wr_lseek(a, b, c);
	case SYS_open: return __wr_openat(AT_FDCWD, (const char *)a, b, c);
	case SYS_openat: return __wr_openat(a, (const char *)b, c, d);
	case SYS_fstat: return __wr_fstat(a, (struct kstat *)b);
	case SYS_stat: return __wr_fstatat(AT_FDCWD, (const char *)a, (struct kstat *)b, 0);
	case SYS_lstat: return __wr_fstatat(AT_FDCWD, (const char *)a, (struct kstat *)b, AT_SYMLINK_NOFOLLOW);
	case SYS_newfstatat: return __wr_fstatat(a, (const char *)b, (struct kstat *)c, d);
	case SYS_access: return __wr_faccessat(AT_FDCWD, (const char *)a, b, 0);
	case SYS_faccessat: return __wr_faccessat(a, (const char *)b, c, 0);
	case SYS_faccessat2: return __wr_faccessat(a, (const char *)b, c, d);
	case SYS_getdents64: return __wr_getdents64(a, (void *)b, c);
	case SYS_mkdir: return __wr_mkdirat(AT_FDCWD, (const char *)a, b);
	case SYS_mkdirat: return __wr_mkdirat(a, (const char *)b, c);
	case SYS_unlink: return __wr_unlinkat(AT_FDCWD, (const char *)a, 0);
	case SYS_rmdir: return __wr_unlinkat(AT_FDCWD, (const char *)a, AT_REMOVEDIR);
	case SYS_unlinkat: return __wr_unlinkat(a, (const char *)b, c);
	case SYS_getcwd: return __wr_getcwd((char *)a, b);
	case SYS_chdir: return __wr_chdir((const char *)a);
	case SYS_fchdir: return __wr_fchdir(a);
	case SYS_dup: return __wr_dup(a);
	case SYS_dup2: return __wr_dup3(a, b, 0);
	case SYS_dup3: return __wr_dup3(a, b, c);
	case SYS_fcntl: return __wr_fcntl(a, b, c);
	case SYS_ioctl: return __wr_ioctl(a, b, (void *)c);
	case SYS_fsync: return __wr_fsync(a);
	case SYS_fdatasync: return __wr_fsync(a);
	case SYS_ftruncate: return __wr_ftruncate(a, b);
	case SYS_readlink: return -EINVAL; /* nothing here is a symbolic link */
	case SYS_umask: return 022;

	case SYS_mmap: return __wr_mmap((void *)a, b, c, d, e, f);
	case SYS_munmap: return __wr_munmap((void *)a, b);
	case SYS_mprotect: return __wr_mprotect((void *)a, b, c);
	case SYS_madvise: return 0; /* advice only */
	case SYS_mremap: return -ENOMEM; /* callers fall back to a copy */
	case SYS_brk: return 0; /* no break: malloc uses mmap */

	case SYS_clock_gettime: return __wr_clock_gettime(a, (struct timespec *)b);
	case SYS_clock_getres: return __wr_clock_getres(a, (struct timespec *)b);
	case SYS_nanosleep: return __wr_nanosleep((const struct timespec *)a, (struct timespec *)b);
	case SYS_clock_nanosleep: return __wr_clock_nanosleep(a, b, (const struct timespec *)c, (struct timespec *)d);

	case SYS_exit:
	case SYS_exit_group: __wr_exit(a);
	case SYS_set_thread_area: return __wr_set_thread_area((void *)a);
	case SYS_arch_prctl: return a == ARCH_SET_FS ? __wr_set_thread_area((void *)b) : -EINVAL;
	case SYS_set_tid_address: return 1;
	case SYS_gettid: return 1;
	case SYS_getpid: return 1;
	case SYS_getppid: return 0;
	case SYS_getuid: return 0;
	case SYS_geteuid: return 0;
	case SYS_getgid: return 0;
	case SYS_getegid: return 0;
	case SYS_sched_yield: return 0;
	case SYS_uname: return __wr_uname((struct utsname *)a);

	case SYS_rt_sigprocmask: return __wr_sigprocmask(a, (const void *)b, (void *)c, d);
	case SYS_rt_sigaction:
		/* Handlers are not run yet (design section 9); report defaults. */
		if (c) memset((void *)c, 0, sizeof(struct k_sigaction));
		return 0;
	case SYS_kill: return self_signal(a, b);
	case SYS_tkill: return self_signal(a, b);
	case SYS_tgkill: return self_signal(b, c);
	}
	return -ENOSYS;
}
