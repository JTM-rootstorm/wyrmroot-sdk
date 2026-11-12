/* SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0 */
/*
 * Wyrmroot program entry. The loader starts a job with the launch Channel in
 * RDI, the startup ABI version in RSI and RSP at argc. __wr_posix_init runs
 * the launch exchange (INIT, READY, release) and sets up descriptors 0-2;
 * then musl starts as usual from the untouched argument block.
 */
__asm__(
".text \n"
".global " START " \n"
".type " START ",%function \n"
START ": \n"
"	xor %ebp,%ebp \n"
"	mov %rsp,%rbx \n"
"	mov %rsp,%rdx \n"
"	andq $-16,%rsp \n"
"	call __wr_posix_init \n"
"	mov %rbx,%rdi \n"
"	call " START "_c \n"
"	hlt \n"
);
