#!/usr/bin/env python3
"""Build the cua-driver wheel from a local Rust build.

This script:
1. Picks the wheel's platform tag from the host and the built binary: the
   host's glibc version and the binary's architecture on Linux, and the
   binary's architecture on macOS and Windows. --plat-name overrides it.
2. Copies a locally built cua-driver binary into src/cua_driver/bin/ and the
   cua-driver-sdk UniFFI library into src/cua_driver/
3. Builds the wheel with hatchling

It never downloads anything. Build the Rust artifacts first, from rust/:

    cargo build --release -p cua-driver -p cua-driver-sdk

On Windows, add `-p cua-driver-uia` to also bundle the UIAccess worker.

Usage:
    python build_wheel.py [--target-dir DIR] [--binary PATH]
                          [--native-library PATH] [--plat-name TAG]
"""

import argparse
import os
import platform
import re
import shutil
import struct
import subprocess
import sys
from pathlib import Path

CARGO_BUILD_HINT = "cargo build --release -p cua-driver -p cua-driver-sdk"


# Keep this aligned with MACOSX_DEPLOYMENT_TARGET in rust/.cargo/config.toml,
# which sets the minimum macOS version of the binary and the SDK dylib.
MACOS_MINIMUM = "13_0"

LINUX_WHEEL_ARCHES = {"x86_64": "x86_64", "arm64": "aarch64"}
WINDOWS_WHEEL_ARCHES = {"x86_64": "amd64", "arm64": "arm64"}

# Architecture fields of executable headers: ELF e_machine, Mach-O cputype
# and PE Machine.
ELF_MACHINES = {62: "x86_64", 183: "arm64"}
MACHO_CPU_TYPES = {0x01000007: "x86_64", 0x0100000C: "arm64"}
PE_MACHINES = {0x8664: "x86_64", 0xAA64: "arm64"}


def get_host_platform() -> str:
    """Return darwin, linux or windows for the machine running this script."""
    system = platform.system().lower()
    if system in ("darwin", "linux", "windows"):
        return system
    raise ValueError(f"Unsupported platform: {system}")


def normalize_arch(name: str) -> str:
    """Map an architecture name to x86_64 or arm64."""
    lowered = name.lower()
    if lowered in ("x86_64", "amd64", "x64"):
        return "x86_64"
    if lowered in ("arm64", "aarch64"):
        return "arm64"
    raise ValueError(f"Unsupported architecture: {name}")


def get_host_arch() -> str:
    """Return the architecture of the machine running this script."""
    return normalize_arch(platform.machine())


def get_binary_arch(path: Path) -> str | None:
    """Read the architecture from an ELF, Mach-O or PE executable header.

    Returns x86_64 or arm64, or universal for a Mach-O universal binary with
    both. Returns None for any other format or architecture.
    """
    with open(path, "rb") as file:
        header = file.read(64)
        magic = header[:4]
        if magic == b"\x7fELF" and len(header) >= 20:
            byte_order = "<" if header[5] == 1 else ">"
            (machine,) = struct.unpack_from(byte_order + "H", header, 18)
            return ELF_MACHINES.get(machine)
        if magic == b"\xcf\xfa\xed\xfe" and len(header) >= 8:
            (cpu_type,) = struct.unpack_from("<I", header, 4)
            return MACHO_CPU_TYPES.get(cpu_type)
        if magic in (b"\xca\xfe\xba\xbe", b"\xca\xfe\xba\xbf") and len(header) >= 8:
            (count,) = struct.unpack_from(">I", header, 4)
            # Java class files share this magic; their version field reads
            # as a large slice count.
            if not 0 < count <= 16:
                return None
            entry_size = 32 if magic[3] == 0xBF else 20
            file.seek(8)
            table = file.read(count * entry_size)
            if len(table) < count * entry_size:
                return None
            arches = {
                MACHO_CPU_TYPES.get(struct.unpack_from(">I", table, i * entry_size)[0])
                for i in range(count)
            }
            arches.discard(None)
            if arches == {"x86_64", "arm64"}:
                return "universal"
            return arches.pop() if len(arches) == 1 else None
        if header[:2] == b"MZ" and len(header) >= 64:
            (pe_offset,) = struct.unpack_from("<I", header, 0x3C)
            file.seek(pe_offset)
            pe_header = file.read(6)
            if len(pe_header) == 6 and pe_header[:4] == b"PE\0\0":
                (machine,) = struct.unpack_from("<H", pe_header, 4)
                return PE_MACHINES.get(machine)
    return None


def get_glibc_version() -> tuple[int, int]:
    """Return the glibc version of the machine running this script."""
    try:
        version = os.confstr("CS_GNU_LIBC_VERSION")
    except (AttributeError, ValueError, OSError):
        version = None
    match = re.fullmatch(r"glibc (\d+)\.(\d+)(?:\.\d+)?", version or "")
    if not match:
        raise ValueError(
            f"Could not read the glibc version (got {version!r}). "
            "Pass --plat-name to set the platform tag."
        )
    return int(match.group(1)), int(match.group(2))


def get_platform_tag(
    platform_name: str, arch: str, glibc_version: tuple[int, int] | None = None
) -> str:
    """Return the wheel platform tag for a binary built for arch.

    A Linux binary only runs where glibc is at least as new as on the machine
    that built it, so the manylinux tag takes this machine's glibc version
    unless glibc_version is given.
    """
    if platform_name == "darwin" and arch in ("x86_64", "arm64", "universal"):
        mac_arch = "universal2" if arch == "universal" else arch
        return f"macosx_{MACOS_MINIMUM}_{mac_arch}"
    if platform_name == "linux" and arch in LINUX_WHEEL_ARCHES:
        major, minor = glibc_version or get_glibc_version()
        return f"manylinux_{major}_{minor}_{LINUX_WHEEL_ARCHES[arch]}"
    if platform_name == "windows" and arch in WINDOWS_WHEEL_ARCHES:
        return f"win_{WINDOWS_WHEEL_ARCHES[arch]}"
    raise ValueError(f"Unsupported wheel target: {platform_name}-{arch}")


def normalize_plat_name(plat_name: str) -> str:
    """Turn a --plat-name value into a tag, as bdist_wheel does."""
    tag = plat_name.strip().replace("-", "_").replace(".", "_")
    if not tag:
        raise ValueError("--plat-name must not be empty")
    return tag


def select_platform_tag(
    platform_name: str, binary: Path, plat_name: str | None = None
) -> str:
    """Pick the platform tag: --plat-name, else the binary's, else the host's."""
    if plat_name:
        return normalize_plat_name(plat_name)
    require_file(binary, "cua-driver binary")
    arch = get_binary_arch(binary)
    if arch is None:
        arch = get_host_arch()
        print(f"Note: could not read the architecture of {binary}; using the host's ({arch}).")
    return get_platform_tag(platform_name, arch)


def get_wheel_tag(platform_tag: str) -> str:
    """Return the full wheel tag for a platform tag."""
    return f"py3-none-{platform_tag}"


def get_binary_names(platform_name: str) -> tuple[str, str]:
    """Return the file names of the CLI binary and the UniFFI library."""
    if platform_name == "darwin":
        return "cua-driver", "libcua_driver_sdk.dylib"
    if platform_name == "linux":
        return "cua-driver", "libcua_driver_sdk.so"
    if platform_name == "windows":
        return "cua-driver.exe", "cua_driver_sdk.dll"
    raise ValueError(f"Unknown platform: {platform_name}")


def get_default_target_dir(script_dir: Path) -> Path:
    """Return the Cargo target directory that `cargo build` writes to."""
    target_dir = os.environ.get("CARGO_TARGET_DIR")
    if target_dir:
        return Path(target_dir)
    return script_dir.parent / "rust" / "target"


def require_file(path: Path, what: str) -> None:
    """Fail with a build hint when a local build artifact is missing."""
    if not path.is_file():
        raise FileNotFoundError(
            f"{what} not found at {path}. Build it first, from rust/: "
            f"{CARGO_BUILD_HINT}"
        )


def stage_artifacts(
    binary: Path,
    native_library: Path,
    package_dir: Path,
    platform_name: str,
) -> list[Path]:
    """Copy the local build into the package source tree.

    The CLI binary goes to package_dir/bin/ and the UniFFI library goes
    next to the generated bindings in package_dir. On Windows, the
    UIAccess worker is copied too when it sits next to the binary.

    Returns:
        List of staged paths
    """
    binary_name, library_name = get_binary_names(platform_name)
    require_file(binary, "cua-driver binary")
    require_file(native_library, "cua-driver-sdk library")

    bin_dir = package_dir / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)

    sources = [(binary, bin_dir / binary_name)]
    if platform_name == "windows":
        uia_worker = binary.parent / "cua-driver-uia.exe"
        if uia_worker.is_file():
            sources.append((uia_worker, bin_dir / uia_worker.name))
        else:
            print(f"Note: {uia_worker} not found; the UIAccess worker is not bundled.")
    sources.append((native_library, package_dir / library_name))

    staged = []
    for source, dest in sources:
        shutil.copy2(source, dest)
        staged.append(dest)
        print(f"Staged {source} -> {dest}")

    # Make executable on Unix
    if sys.platform != "win32":
        for path in staged:
            os.chmod(path, 0o755)

    return staged


def build_wheel(package_dir: Path, wheel_tag: str) -> None:
    """Build the wheel using hatchling.

    Args:
        package_dir: Directory containing pyproject.toml
        wheel_tag: Platform-specific wheel tag to write.
    """
    print("\nBuilding wheel...")

    env = os.environ.copy()
    env["CUA_DRIVER_WHEEL_TAG"] = wheel_tag

    platform_tag = wheel_tag.split("-")[-1]
    if platform_tag.startswith("win_"):
        # Upstream set this for Windows cross builds; keep it in step with the tag.
        env["_PYTHON_HOST_PLATFORM"] = platform_tag.replace("_", "-", 1)

    subprocess.run(
        [sys.executable, "-m", "build", "--wheel"],
        cwd=package_dir,
        env=env,
        check=True,
    )
    print("Wheel built successfully!")


def main():
    parser = argparse.ArgumentParser(
        description="Build the cua-driver Python wheel from a local Rust build"
    )
    parser.add_argument(
        "--target-dir",
        type=Path,
        help="Cargo target directory to take the release build from "
        "(default: $CARGO_TARGET_DIR, else ../rust/target)",
    )
    parser.add_argument(
        "--binary",
        type=Path,
        help="Path to the cua-driver binary (default: <target-dir>/release/cua-driver)",
    )
    parser.add_argument(
        "--native-library",
        type=Path,
        help="Path to the cua-driver-sdk UniFFI library "
        "(default: <target-dir>/release/libcua_driver_sdk.so, .dylib, or cua_driver_sdk.dll)",
    )
    parser.add_argument(
        "--plat-name",
        help="Wheel platform tag to use instead of the detected one "
        "(e.g. manylinux_2_31_x86_64, macosx_13_0_universal2, win_arm64)",
    )
    args = parser.parse_args()

    # Determine paths
    script_dir = Path(__file__).parent
    package_dir = script_dir / "src" / "cua_driver"
    platform_name = get_host_platform()
    binary_name, library_name = get_binary_names(platform_name)
    release_dir = (args.target_dir or get_default_target_dir(script_dir)) / "release"
    binary = args.binary or release_dir / binary_name
    native_library = args.native_library or release_dir / library_name

    wheel_tag = get_wheel_tag(select_platform_tag(platform_name, binary, args.plat_name))
    print(f"Wheel tag: {wheel_tag}")

    staged = stage_artifacts(binary, native_library, package_dir, platform_name)
    for path in staged:
        print(f"  - {path.name} ({path.stat().st_size / 1024 / 1024:.2f} MB)")

    build_wheel(script_dir, wheel_tag)


if __name__ == "__main__":
    main()
