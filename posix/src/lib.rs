// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
//! The Wyrmroot POSIX layer: the libc-neutral userspace layer between a C
//! library and Wyrmroot's native objects and protocols (root
//! `TOOLCHAIN_PLAN.md` §3, `TOOLCHAIN_POSIX_LAYER_DESIGN.md`).
//!
//! This is the step-2 slice: process startup, file descriptors over WRST
//! streams and `vfsd`, lexical paths, anonymous memory, clocks and sleeps.
//! It adds no kernel primitive. Built for `x86_64-unknown-wyrmroot` it is
//! `libwrposix.a`, which the SDK merges into musl's `libc.a`; on the host it
//! is only tested.

#![cfg_attr(not(test), no_std)]

pub mod arena;
pub mod errno;
pub mod exports;
pub mod fdtable;
pub mod native;
pub mod node;
pub mod path;
pub mod process;
pub mod startup;
pub mod stream;
pub mod types;

#[cfg(test)]
mod process_tests;
#[cfg(test)]
mod testing;
