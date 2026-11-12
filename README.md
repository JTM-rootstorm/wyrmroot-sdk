# Wyrmroot SDK

The Wyrmroot SDK is the sysroot that applications and ports build against for the `x86_64-unknown-wyrmroot` target. Programs compile with the Wyrmroot toolchain forks (`rust/` and `llvm-project/`) against this SDK, never against host headers or libraries.

When complete, the SDK will contain:

- a C library: musl first, with functions moving to LLVM libc as it matures;
- the Wyrmroot POSIX layer that both libraries sit on;
- compiler-rt builtins, libunwind, libc++abi and libc++;
- C headers and libraries for native Wyrmroot services, generated from the same schemas as the Rust protocol crates and `deepwyrm-abi`;
- the Rust target libraries for `x86_64-unknown-wyrmroot`, once `std` exists.

Native first-party Wyrmroot services do not use this SDK; they stay libc-free.

## Layout and build

- `posix/`: the Wyrmroot POSIX layer, a Rust `no_std` crate built into `libwrposix.a`. It exports one `__wr_*` C function per operation; `wr_posix.h` is generated from it.
- `musl/`: the musl port, `arch/wyrmroot_x86_64`. Its `syscall_arch.h` sends every musl system call to a dispatcher that calls the POSIX layer; no musl code enters the kernel.
- `programs/`: our own test programs. With zlib's `test/example.c` and the Lua interpreter, they are what Wyrmroot's `dev-boot --check --sdk` runs.
- `sdk.toml`: the pinned inputs (the musl, zlib and Lua releases, the LLVM fork revision and build, the Rust fork toolchain).
- `build.py`: builds everything into `out/`: the sysroot (with zlib as `libz.a`), the `out/bin/wyrmroot-cc` wrapper, the test programs and `out/sdk-manifest.toml`, which records every input revision and output hash.

```sh
python3 -I build.py --fetch
```

`--fetch` downloads the pinned releases that are not yet in `.cache/sources`.

## Licensing

- **Runtime code linked into programs** (the POSIX layer, startup objects, generated service bindings): `GPL-3.0-or-later WITH Classpath-exception-2.0` (`LICENSES/GPL-3.0-or-later.txt`, `LICENSES/Classpath-exception-2.0.txt`). A program may link it and be distributed under the program's own terms; changes to the runtime itself stay under the GPL.
- **Build tooling:** `GPL-3.0-or-later`.
- **Third-party sources** (musl, zlib, Lua) are pinned by release and hash and keep their upstream licenses. zlib's license is installed with it in `share/licenses/zlib`.

Everything else linked into a program carries a matching exception or a permissive license: Wyrmroot's protocol codecs (`wyrmroot-startup-proto`, `wyrmroot-stream-proto`, `wyrmroot-fs-proto`) and Deepwyrm's `deepwyrm-abi` and `deepwyrm-syscall` are licensed `… WITH Classpath-exception-2.0`; musl is MIT; zlib is Zlib; compiler-rt is `Apache-2.0 WITH LLVM-exception`.

The project-wide rule is in the root `LICENSING_POLICY.md`.
