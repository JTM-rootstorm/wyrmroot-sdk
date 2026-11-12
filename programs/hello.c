/* SPDX-License-Identifier: GPL-3.0-or-later */
/*
 * First light for the SDK: a static musl program. It allocates through
 * malloc (anonymous mmap), formats an integer and a double (SSE, SysV
 * psABI) and writes through stdio to its stdout stream.
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>

int main(int argc, char **argv)
{
	(void)argv;
	size_t size = 256 * 1024;
	unsigned char *block = malloc(size);
	if (!block) {
		int malloc_error = errno;
		void *page = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
		fprintf(stderr, "hello: malloc failed: %s; mmap: %s\n", strerror(malloc_error),
			page == MAP_FAILED ? strerror(errno) : "ok");
		return 2;
	}
	memset(block, 0x5a, size);
	unsigned long sum = 0;
	for (size_t i = 0; i < size; i += 4096) sum += block[i];
	free(block);
	printf("hello from musl: %d argument%s, 6*7=%d, pi=%.5f, pages=%lu\n",
	       argc, argc == 1 ? "" : "s", 6 * 7, 3.14159265358979, sum / 0x5a);
	return 0;
}
