# Wyrmroot SDK

The Wyrmroot SDK is the sysroot that applications and ports build against for the `x86_64-unknown-wyrmroot` target. Programs compile with the Wyrmroot toolchain forks (`rust/` and `llvm-project/`) against this SDK, never against host headers or libraries.

When complete, the SDK will contain:

- a C library: musl first, with functions moving to LLVM libc as it matures;
- the Wyrmroot POSIX layer that both libraries sit on;
- compiler-rt builtins, libunwind, libc++abi and libc++;
- C headers and libraries for native Wyrmroot services, generated from the same schemas as the Rust protocol crates and `deepwyrm-abi`;
- the Rust target libraries for `x86_64-unknown-wyrmroot`, once `std` exists.

Native first-party Wyrmroot services do not use this SDK; they stay libc-free.

## Licensing

First-party build tooling in this repository is `GPL-3.0-or-later` (`LICENSES/GPL-3.0-or-later.txt`). Third-party sources such as musl are pinned by revision and keep their upstream licenses. The license of the first-party runtime code that is linked into applications, chiefly the POSIX layer, is not yet chosen.
