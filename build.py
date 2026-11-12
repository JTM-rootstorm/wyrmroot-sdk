#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Builds the Wyrmroot SDK sysroot and its test programs.

    python3 -I build.py [--fetch]

Everything lands in out/ (recreated on every run): the sysroot with musl,
the POSIX layer merged into libc.a, startup objects and compiler-rt
builtins; a compiler wrapper; the test programs; and out/sdk-manifest.toml,
which records every input revision and output hash. Inputs are pinned in
sdk.toml. Nothing is installed outside this directory, and no guest program
is ever run on the host: programs are only inspected.

--fetch downloads the pinned musl release into .cache/sources if it is
missing. The download is checked against the pinned hash before use.
"""

import hashlib
import os
import shutil
import subprocess
import sys
import tarfile
import tomllib
from pathlib import Path

SDK = Path(__file__).resolve().parent
PROJECT = SDK.parent
OUT = SDK / "out"
CACHE = SDK / ".cache"
WYRMROOT = PROJECT / "wyrmroot"
LLVM_PROJECT = PROJECT / "llvm-project"
TARGET = "x86_64-unknown-wyrmroot"

# musl's x86_64 sources that issue `syscall` themselves (or exist only for
# dynamic linking). The port uses musl's generic C versions instead, which
# go through syscall_arch.h and so through the POSIX layer.
MUSL_X86_64_EXCLUDED = {
    "__set_thread_area.s",
    "__unmapself.s",
    "clone.s",
    "syscall_cp.s",
    "restore.s",
    "vfork.s",
    "dlsym.s",
    "tlsdesc.s",
}
# musl's port files that replace the copied x86_64 ones.
PORT_ARCH_FILES = ("syscall_arch.h", "crt_arch.h")

# What the Wyrmroot loader accepts in a program (WYR0_E0 loading contract):
# static ET_EXEC, x86_64, PT_LOAD (never W+X), PT_PHDR, PT_GNU_STACK, PT_TLS.
ALLOWED_PROGRAM_HEADERS = {"PHDR", "LOAD", "GNU_STACK", "TLS"}
FORBIDDEN_INSTRUCTIONS = ("syscall", "sysenter", "int $0x80")

PROGRAMS = {"hello": ["programs/hello.c"]}


def fail(message):
    print(f"build.py: {message}", file=sys.stderr)
    sys.exit(1)


def step(message):
    print(f"== {message}", flush=True)


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as file:
        for block in iter(lambda: file.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def clean_environment(extra=None):
    environment = {
        "PATH": "/usr/bin:/bin",
        "HOME": os.environ.get("HOME", "/nonexistent"),
        "LC_ALL": "C",
        "SOURCE_DATE_EPOCH": "0",
    }
    environment.update(extra or {})
    return environment


def run(command, cwd=None, environment=None, capture=False):
    """Runs `command`. With `capture`, its output is returned and its
    diagnostics are shown only if it fails."""
    result = subprocess.run(
        [str(part) for part in command],
        cwd=cwd,
        env=environment or clean_environment(),
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.PIPE if capture else None,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        if capture:
            print(result.stdout[-4000:], result.stderr[-4000:], file=sys.stderr)
        fail(f"{command[0]} failed with exit code {result.returncode}")
    return result.stdout if capture else None


def git(repository, *arguments):
    return run(["git", "-C", repository, *arguments], capture=True).strip()


class Inputs:
    def __init__(self):
        with open(SDK / "sdk.toml", "rb") as file:
            self.pins = tomllib.load(file)
        if self.pins.get("schema") != "wyrmroot-sdk-inputs-v1":
            fail("sdk.toml has an unknown schema")
        llvm = self.pins["llvm"]
        rust = self.pins["rust"]
        self.clang = PROJECT / llvm["build"] / "bin" / "clang"
        self.resource = PROJECT / llvm["build"] / "lib" / "clang" / llvm["resource_version"]
        self.tools = Path(llvm["host_tools"])
        self.toolchain = PROJECT / rust["toolchain"]
        self.host_cargo = Path(rust["host_toolchain"]) / "cargo"
        self.cargo_home = PROJECT / rust["cargo_home"]
        self.musl_archive = CACHE / "sources" / f"musl-{self.pins['musl']['version']}.tar.gz"

    def tool(self, name):
        return self.tools / name

    def check(self):
        llvm = self.pins["llvm"]
        rust = self.pins["rust"]
        if sha256(self.clang.resolve()) != llvm["clang_sha256"]:
            fail(f"{self.clang} is not the pinned clang build")
        if sha256(self.toolchain / "bin" / "rustc") != rust["rustc_sha256"]:
            fail(f"{self.toolchain} is not the pinned Rust toolchain")
        for name in ("llvm-ar", "llvm-ranlib", "llvm-nm", "llvm-objdump", "llvm-readelf"):
            if not self.tool(name).is_file():
                fail(f"host tool missing: {self.tool(name)}")
        if git(LLVM_PROJECT, "rev-parse", f"{llvm['revision']}^{{commit}}") != llvm["revision"]:
            fail("the pinned llvm-project revision is not in the checkout")
        # The layer's deepwyrm-syscall must be the revision Wyrmroot pins.
        with open(WYRMROOT / "toolchain" / "versions.toml", "rb") as file:
            deepwyrm = tomllib.load(file)["deepwyrm"]["revision"]
        manifest = (SDK / "posix" / "Cargo.toml").read_text()
        if f'rev = "{deepwyrm}"' not in manifest:
            fail(f"posix/Cargo.toml must pin deepwyrm-syscall at Wyrmroot's {deepwyrm}")
        self.deepwyrm = deepwyrm

    def fetch_musl(self, allowed):
        musl = self.pins["musl"]
        if not self.musl_archive.is_file():
            if not allowed:
                fail(f"{self.musl_archive} is missing; run with --fetch to download {musl['url']}")
            self.musl_archive.parent.mkdir(parents=True, exist_ok=True)
            partial = self.musl_archive.with_suffix(".partial")
            run(["curl", "-sSfL", "-o", partial, musl["url"]])
            partial.rename(self.musl_archive)
        if sha256(self.musl_archive) != musl["sha256"]:
            fail(f"{self.musl_archive} does not match the pinned hash")


def fresh(path):
    """Recreates a directory under out/ (and nowhere else)."""
    path = path.resolve()
    if OUT.resolve() not in path.parents and path != OUT.resolve():
        fail(f"refusing to clear {path} outside {OUT}")
    if path.exists():
        shutil.rmtree(path)
    path.mkdir(parents=True)
    return path


def build_builtins(inputs, build):
    step("compiler-rt builtins")
    source = build / "llvm-src"
    source.mkdir()
    archive = subprocess.run(
        ["git", "-C", LLVM_PROJECT, "archive", "--format=tar", inputs.pins["llvm"]["revision"],
         "compiler-rt", "cmake", "llvm/cmake"],
        stdout=subprocess.PIPE, check=True, env=clean_environment(),
    ).stdout
    subprocess.run(["tar", "-x", "-C", source], input=archive, check=True, env=clean_environment())
    binary = build / "compiler-rt"
    binary.mkdir()
    clang = inputs.clang
    run([
        "cmake", "-G", "Ninja", source / "compiler-rt" / "lib" / "builtins",
        "-DCMAKE_BUILD_TYPE=Release",
        f"-DCMAKE_C_COMPILER={clang}", f"-DCMAKE_ASM_COMPILER={clang}",
        f"-DCMAKE_AR={inputs.tool('llvm-ar')}", f"-DCMAKE_RANLIB={inputs.tool('llvm-ranlib')}",
        f"-DCMAKE_NM={inputs.tool('llvm-nm')}",
        f"-DCMAKE_C_COMPILER_TARGET={TARGET}", f"-DCMAKE_ASM_COMPILER_TARGET={TARGET}",
        "-DCMAKE_SYSTEM_NAME=Generic", "-DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY",
        "-DCOMPILER_RT_BAREMETAL_BUILD=ON", "-DCOMPILER_RT_DEFAULT_TARGET_ONLY=ON",
        "-DLLVM_ENABLE_PER_TARGET_RUNTIME_DIR=ON", f"-DCOMPILER_RT_OS_DIR={TARGET}",
        "-DCMAKE_C_FLAGS=-ffreestanding", "-DLLVM_CONFIG_PATH=",
        "-Wno-dev",
    ], cwd=binary, capture=True)
    run(["ninja"], cwd=binary, capture=True)
    return binary / "lib" / TARGET / "libclang_rt.builtins.a"


def cargo_environment(inputs, cargo_bin):
    return clean_environment({
        "PATH": f"{cargo_bin}:/usr/bin:/bin",
        "CARGO_HOME": str(inputs.cargo_home),
        "CARGO_NET_OFFLINE": "true",
        "CARGO_INCREMENTAL": "0",
    })


def build_posix(inputs, build):
    step("POSIX layer: wr_posix.h and libwrposix.a")
    posix = SDK / "posix"
    host = cargo_environment(inputs, inputs.host_cargo.parent)
    header = run([
        inputs.host_cargo, "run", "--offline", "--locked", "-q", "--features", "header",
        "--bin", "wr-posix-header", "--target-dir", build / "posix-host",
    ], cwd=posix, environment=host, capture=True)
    header_path = build / "wr_posix.h"
    header_path.write_text(header)
    toolchain = inputs.toolchain / "bin"
    guest = cargo_environment(inputs, toolchain)
    guest["RUSTC"] = str(toolchain / "rustc")
    guest["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join([
        f"--remap-path-prefix={SDK}=/source/wyrmroot-sdk",
        f"--remap-path-prefix={WYRMROOT}=/source/wyrmroot",
        f"--remap-path-prefix={inputs.cargo_home}=/cargo-home",
    ])
    run([
        toolchain / "cargo", "rustc", "--offline", "--locked", "--release", "--lib",
        "--target", TARGET, "--target-dir", build / "posix-guest", "--crate-type", "staticlib",
    ], cwd=posix, environment=guest)
    return header_path, build / "posix-guest" / TARGET / "release" / "libwyrmroot_posix.a"


def port_musl(inputs, build, header):
    step("musl: unpack and port")
    with tarfile.open(inputs.musl_archive) as archive:
        archive.extractall(build, filter="data")
    tree = build / f"musl-{inputs.pins['musl']['version']}"
    arch = tree / "arch" / "wyrmroot_x86_64"
    shutil.copytree(tree / "arch" / "x86_64", arch)
    for name in PORT_ARCH_FILES:
        shutil.copyfile(SDK / "musl" / "arch" / "wyrmroot_x86_64" / name, arch / name)
    for source in sorted(list(tree.glob("src/*/x86_64")) + [tree / "crt" / "x86_64"]):
        destination = source.parent / "wyrmroot_x86_64"
        destination.mkdir()
        for file in sorted(source.iterdir()):
            if file.name not in MUSL_X86_64_EXCLUDED:
                shutil.copyfile(file, destination / file.name)
    internal = tree / "src" / "internal"
    (internal / "wyrmroot_x86_64").mkdir()
    shutil.copyfile(SDK / "musl" / "src" / "internal" / "wyrmroot_x86_64" / "wr_syscall.c",
                    internal / "wyrmroot_x86_64" / "wr_syscall.c")
    shutil.copyfile(header, internal / "wr_posix.h")
    return tree


def build_musl(inputs, tree, sysroot):
    step("musl: configure and build")
    environment = clean_environment({
        "CC": f"{inputs.clang} --target={TARGET}",
        "AR": str(inputs.tool("llvm-ar")),
        "RANLIB": str(inputs.tool("llvm-ranlib")),
    })
    run(["./configure", f"--target={TARGET}", "--prefix=/", "--disable-shared", "--disable-wrapper"],
        cwd=tree, environment=environment, capture=True)
    with open(tree / "config.mak", "a") as config:
        # configure maps the triple to x86_64; the port is its own arch.
        config.write("ARCH = wyrmroot_x86_64\n")
    run(["make", f"-j{os.cpu_count() or 4}"], cwd=tree, environment=environment, capture=True)
    run(["make", "install", f"DESTDIR={sysroot}"], cwd=tree, environment=environment, capture=True)
    for unused in ("rcrt1.o", "Scrt1.o"):
        (sysroot / "lib" / unused).unlink()


def merge_libc(inputs, sysroot, posix):
    """Appends libwrposix.a's members to libc.a, after musl's, so musl's
    definitions are found first."""
    step("libc.a: merge the POSIX layer")
    libc = sysroot / "lib" / "libc.a"
    musl = sysroot / "lib" / "libc-musl.a"
    libc.rename(musl)
    script = f"CREATE {libc}\nADDLIB {musl}\nADDLIB {posix}\nSAVE\nEND\n"
    subprocess.run([str(inputs.tool("llvm-ar")), "-M"], input=script, text=True, check=True,
                   env=clean_environment())
    musl.unlink()
    run([inputs.tool("llvm-ranlib"), libc])


def install_resources(inputs, sysroot, builtins):
    step("clang resource directory")
    resource = sysroot / "lib" / "clang" / inputs.pins["llvm"]["resource_version"]
    shutil.copytree(inputs.resource / "include", resource / "include")
    (resource / "lib" / TARGET).mkdir(parents=True)
    shutil.copyfile(builtins, resource / "lib" / TARGET / "libclang_rt.builtins.a")
    return resource


def write_wrapper(inputs, sysroot, resource):
    wrapper = OUT / "bin" / "wyrmroot-cc"
    wrapper.parent.mkdir(parents=True)
    wrapper.write_text(
        "#!/bin/sh\n"
        "# Generated by wyrmroot-sdk/build.py: clang for Wyrmroot against this SDK.\n"
        f'exec "{inputs.clang}" --target={TARGET} --sysroot="{sysroot}" '
        f'-resource-dir="{resource}" "$@"\n'
    )
    wrapper.chmod(0o755)
    return wrapper


def kernel_entries(inputs, path):
    """Every instruction in `path` that enters the kernel, by function. The
    one sanctioned entry is Deepwyrm's generated veneer, `dw_syscall6`."""
    listing = run([inputs.tool("llvm-objdump"), "-d", "--no-show-raw-insn", path], capture=True)
    entries, function = [], "?"
    for line in listing.splitlines():
        if line.endswith(">:"):
            function = line[line.find("<") + 1 : -2]
            continue
        if "\t" not in line:
            continue
        text = " ".join(part.strip() for part in line.split("\t")[1:])
        if any(text == bad or text.startswith(bad + " ") for bad in FORBIDDEN_INSTRUCTIONS):
            entries.append(function)
    stray = sorted({function for function in entries if function != "dw_syscall6"})
    if stray:
        fail(f"{path.name} enters the kernel outside the Deepwyrm veneer: {stray[:5]}")
    return len(entries)


def check_no_kernel_entry(inputs, sysroot):
    step("check: the libraries enter the kernel only through the veneer")
    for path in [sysroot / "lib" / "libc.a", sysroot / "lib" / "crt1.o"]:
        kernel_entries(inputs, path)


def inspect_program(inputs, path):
    headers = run([inputs.tool("llvm-readelf"), "-h", "-l", "-W", path], capture=True)
    if "Type:                              EXEC" not in headers:
        fail(f"{path.name} is not a static ET_EXEC program")
    if "Machine:                           Advanced Micro Devices X86-64" not in headers:
        fail(f"{path.name} is not x86_64")
    kinds, in_table = [], False
    for line in headers.splitlines():
        fields = line.split()
        if fields[:2] == ["Type", "Offset"]:
            in_table = True
            continue
        if not in_table:
            continue
        if not fields:
            break
        kinds.append(fields[0])
        # Type, Offset, VirtAddr, PhysAddr, FileSiz, MemSiz, flags..., Align
        flags = "".join(fields[6:-1])
        if fields[0] == "LOAD" and "W" in flags and "E" in flags:
            fail(f"{path.name} has a W+X segment")
    unexpected = set(kinds) - ALLOWED_PROGRAM_HEADERS
    if unexpected:
        fail(f"{path.name} has program headers the loader refuses: {sorted(unexpected)}")
    return {"program_headers": kinds, "syscalls": kernel_entries(inputs, path)}


def build_programs(inputs, wrapper):
    step("test programs")
    programs = OUT / "programs"
    programs.mkdir()
    results = {}
    for name, sources in PROGRAMS.items():
        output = programs / f"{name}.elf"
        run([wrapper, "-O2", "-Wall", "-Wextra", "-Werror", "-o", output,
             *[SDK / source for source in sources]])
        results[name] = inspect_program(inputs, output) | {"sha256": sha256(output)}
    return results


def write_manifest(inputs, sysroot, header, builtins, programs):
    def revision(repository):
        head = git(repository, "rev-parse", "HEAD")
        dirty = bool(git(repository, "status", "--porcelain"))
        return head, dirty

    sdk_head, sdk_dirty = revision(SDK)
    wyrmroot_head, wyrmroot_dirty = revision(WYRMROOT)
    lines = [
        "# Generated by build.py. The SDK's inputs and outputs.",
        'schema = "wyrmroot-sdk-manifest-v1"',
        f'target = "{TARGET}"',
        "",
        "[inputs]",
        f'wyrmroot_sdk = "{sdk_head}"',
        f"wyrmroot_sdk_dirty = {str(sdk_dirty).lower()}",
        f'wyrmroot = "{wyrmroot_head}"',
        f"wyrmroot_dirty = {str(wyrmroot_dirty).lower()}",
        f'deepwyrm_syscall = "{inputs.deepwyrm}"',
        f'llvm_project = "{inputs.pins["llvm"]["revision"]}"',
        f'clang_sha256 = "{inputs.pins["llvm"]["clang_sha256"]}"',
        f'rustc_sha256 = "{inputs.pins["rust"]["rustc_sha256"]}"',
        f'musl = "{inputs.pins["musl"]["version"]}"',
        f'musl_sha256 = "{inputs.pins["musl"]["sha256"]}"',
        "",
        "[outputs]",
    ]
    for path in [sysroot / "lib" / "libc.a", sysroot / "lib" / "crt1.o",
                 sysroot / "lib" / "crti.o", sysroot / "lib" / "crtn.o", header, builtins]:
        lines.append(f'"{path.relative_to(OUT) if OUT in path.parents else path.name}" = "{sha256(path)}"')
    for name, result in programs.items():
        lines += ["", f"[programs.{name}]",
                  f'sha256 = "{result["sha256"]}"',
                  f"program_headers = {result['program_headers']}".replace("'", '"'),
                  f"veneer_syscalls = {result['syscalls']}"]
    (OUT / "sdk-manifest.toml").write_text("\n".join(lines) + "\n")


def main():
    arguments = sys.argv[1:]
    if arguments not in ([], ["--fetch"]):
        fail("usage: build.py [--fetch]")
    inputs = Inputs()
    inputs.check()
    inputs.fetch_musl(arguments == ["--fetch"])
    fresh(OUT)
    build = fresh(OUT / "build")
    sysroot = fresh(OUT / "sysroot")
    builtins = build_builtins(inputs, build)
    header, posix = build_posix(inputs, build)
    tree = port_musl(inputs, build, header)
    build_musl(inputs, tree, sysroot)
    merge_libc(inputs, sysroot, posix)
    shutil.copyfile(header, sysroot / "include" / "wr_posix.h")
    resource = install_resources(inputs, sysroot, builtins)
    check_no_kernel_entry(inputs, sysroot)
    wrapper = write_wrapper(inputs, sysroot, resource)
    programs = build_programs(inputs, wrapper)
    write_manifest(inputs, sysroot, sysroot / "include" / "wr_posix.h",
                   resource / "lib" / TARGET / "libclang_rt.builtins.a", programs)
    step(f"done: {OUT}")


if __name__ == "__main__":
    main()
