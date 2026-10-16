#!/usr/bin/env python3
"""Build the cua-driver wheel from a local Rust build.

This script:
1. Detects the current platform
2. Copies a locally built cua-driver binary into src/cua_driver/bin/ and the
   cua-driver-sdk UniFFI library into src/cua_driver/
3. Builds the wheel with hatchling

It never downloads anything. Build the Rust artifacts first, from rust/:

    cargo build --release -p cua-driver -p cua-driver-sdk

On Windows, add `-p cua-driver-uia` to also bundle the UIAccess worker.

Usage:
    python build_wheel.py [--target-dir DIR] [--binary PATH]
                          [--native-library PATH] [--arch ARCH]
"""

import argparse
import os
import platform
import shutil
import subprocess
import sys
from pathlib import Path

CARGO_BUILD_HINT = "cargo build --release -p cua-driver -p cua-driver-sdk"


def get_platform_info(arch_override: str = None):
    """Determine platform and architecture for the wheel tag.

    Args:
        arch_override: Optional architecture override (e.g., 'arm64', 'x86_64', 'universal')
    """
    system = platform.system().lower()

    if arch_override:
        # Normalize arch_override to handle common aliases/casing
        arch_lower = arch_override.lower()
        if arch_lower in ("x86_64", "amd64", "x64"):
            arch = "x86_64"
        elif arch_lower in ("arm64", "aarch64"):
            arch = "arm64"
        elif arch_lower == "universal":
            arch = "universal"
        else:
            raise ValueError(f"Unsupported architecture override: {arch_override}")
    else:
        machine = platform.machine().lower()
        # Normalize architecture names
        if machine in ("x86_64", "amd64"):
            arch = "x86_64"
        elif machine in ("arm64", "aarch64"):
            arch = "arm64"
        else:
            raise ValueError(f"Unsupported architecture: {machine}")

    if system == "darwin":
        # The macOS wheel is tagged universal2
        return "darwin", "universal"
    elif system == "linux":
        return "linux", arch
    elif system == "windows":
        return "windows", arch
    else:
        raise ValueError(f"Unsupported platform: {system}")


def get_wheel_tag(platform_name: str, arch: str) -> str:
    """Return the Python wheel tag for the bundled binary target."""
    if platform_name == "darwin":
        # Keep this aligned with the SDK dylib's LC_BUILD_VERSION minimum.
        return "py3-none-macosx_13_0_universal2"
    if platform_name == "linux":
        # Upstream built its Linux release binaries in debian:11, whose glibc
        # floor is 2.31. A local build only runs where the glibc is at least
        # as new as on the host that built it.
        if arch == "x86_64":
            return "py3-none-manylinux_2_31_x86_64"
        if arch == "arm64":
            return "py3-none-manylinux_2_31_aarch64"
    if platform_name == "windows":
        if arch == "x86_64":
            return "py3-none-win_amd64"
        if arch == "arm64":
            return "py3-none-win_arm64"

    raise ValueError(f"Unsupported wheel target: {platform_name}-{arch}")


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


def build_wheel(package_dir: Path, wheel_tag: str, target_arch: str = None) -> None:
    """Build the wheel using hatchling.

    Args:
        package_dir: Directory containing pyproject.toml
        wheel_tag: Platform-specific wheel tag to write.
        target_arch: Target architecture for cross-compilation (x86_64, arm64)
    """
    print("\nBuilding wheel...")

    env = os.environ.copy()
    env["CUA_DRIVER_WHEEL_TAG"] = wheel_tag

    # Set platform tag override for cross-compilation
    if target_arch:
        system = platform.system().lower()
        if system == "windows":
            # Override wheel platform tag for Windows cross-compilation
            if target_arch == "arm64":
                env["_PYTHON_HOST_PLATFORM"] = "win-arm64"
            elif target_arch == "x86_64":
                env["_PYTHON_HOST_PLATFORM"] = "win-amd64"
        # Linux and macOS don't need overrides for our use case
        # (macOS uses universal binary, Linux builds on native arch)

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
        "--arch",
        help="Architecture override (e.g., 'arm64', 'x86_64', 'universal')",
    )
    args = parser.parse_args()

    # Determine paths
    script_dir = Path(__file__).parent
    package_dir = script_dir / "src" / "cua_driver"
    platform_name, arch = get_platform_info(args.arch)
    wheel_tag = get_wheel_tag(platform_name, arch)
    print(f"Building for {platform_name}-{arch}")
    print(f"Wheel tag: {wheel_tag}")

    binary_name, library_name = get_binary_names(platform_name)
    release_dir = (args.target_dir or get_default_target_dir(script_dir)) / "release"
    binary = args.binary or release_dir / binary_name
    native_library = args.native_library or release_dir / library_name

    staged = stage_artifacts(binary, native_library, package_dir, platform_name)
    for path in staged:
        print(f"  - {path.name} ({path.stat().st_size / 1024 / 1024:.2f} MB)")

    # Build the wheel (pass target arch for cross-compilation)
    build_wheel(script_dir, wheel_tag, target_arch=arch)


if __name__ == "__main__":
    main()
