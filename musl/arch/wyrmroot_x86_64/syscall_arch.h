/* SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0 */
/*
 * Wyrmroot: musl's system calls never reach the kernel directly. Every
 * __syscallN goes to __wr_syscall, the dispatcher in
 * src/internal/wyrmroot_x86_64/wr_syscall.c, which calls the Wyrmroot POSIX
 * layer (root TOOLCHAIN_POSIX_LAYER_DESIGN.md section 1). There is no
 * `syscall` instruction and no vDSO.
 */
#define __SYSCALL_LL_E(x) (x)
#define __SYSCALL_LL_O(x) (x)

hidden long __wr_syscall(long, long, long, long, long, long, long);

static __inline long __syscall0(long n)
{
	return __wr_syscall(n, 0, 0, 0, 0, 0, 0);
}

static __inline long __syscall1(long n, long a1)
{
	return __wr_syscall(n, a1, 0, 0, 0, 0, 0);
}

static __inline long __syscall2(long n, long a1, long a2)
{
	return __wr_syscall(n, a1, a2, 0, 0, 0, 0);
}

static __inline long __syscall3(long n, long a1, long a2, long a3)
{
	return __wr_syscall(n, a1, a2, a3, 0, 0, 0);
}

static __inline long __syscall4(long n, long a1, long a2, long a3, long a4)
{
	return __wr_syscall(n, a1, a2, a3, a4, 0, 0);
}

static __inline long __syscall5(long n, long a1, long a2, long a3, long a4, long a5)
{
	return __wr_syscall(n, a1, a2, a3, a4, a5, 0);
}

static __inline long __syscall6(long n, long a1, long a2, long a3, long a4, long a5, long a6)
{
	return __wr_syscall(n, a1, a2, a3, a4, a5, a6);
}

#define IPC_64 0
