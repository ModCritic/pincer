"""Tests for cua-driver wheel build helpers."""

import importlib.util
import os
import tempfile
from pathlib import Path
from unittest import mock

BUILD_WHEEL_PATH = Path(__file__).resolve().parents[1] / "build_wheel.py"


def load_build_wheel_module():
    spec = importlib.util.spec_from_file_location("build_wheel", BUILD_WHEEL_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def test_wheel_tags_are_platform_specific():
    build_wheel = load_build_wheel_module()
    tag = build_wheel.get_platform_tag

    assert tag("darwin", "universal") == "macosx_13_0_universal2"
    assert tag("darwin", "arm64") == "macosx_13_0_arm64"
    assert tag("darwin", "x86_64") == "macosx_13_0_x86_64"
    assert tag("linux", "x86_64", (2, 39)) == "manylinux_2_39_x86_64"
    assert tag("linux", "arm64", (2, 31)) == "manylinux_2_31_aarch64"
    assert tag("windows", "x86_64") == "win_amd64"
    assert tag("windows", "arm64") == "win_arm64"
    assert build_wheel.get_wheel_tag("win_amd64") == "py3-none-win_amd64"
    for platform_name in ("linux", "windows"):
        try:
            tag(platform_name, "universal", (2, 39))
        except ValueError:
            pass
        else:
            raise AssertionError(f"accepted a universal {platform_name} build")


def test_linux_tag_takes_the_host_glibc_version():
    build_wheel = load_build_wheel_module()

    with mock.patch.object(build_wheel.os, "confstr", return_value="glibc 2.42"):
        assert build_wheel.get_glibc_version() == (2, 42)
        assert build_wheel.get_platform_tag("linux", "x86_64") == "manylinux_2_42_x86_64"


def test_missing_glibc_version_asks_for_plat_name():
    build_wheel = load_build_wheel_module()

    for confstr in (
        mock.patch.object(build_wheel.os, "confstr", return_value=None),
        mock.patch.object(build_wheel.os, "confstr", side_effect=ValueError),
    ):
        with confstr:
            try:
                build_wheel.get_glibc_version()
            except ValueError as error:
                assert "--plat-name" in str(error)
            else:
                raise AssertionError("accepted a host without a glibc version")


def _elf(machine: int) -> bytes:
    header = bytearray(64)
    header[:6] = b"\x7fELF\x02\x01"
    header[18:20] = machine.to_bytes(2, "little")
    return bytes(header)


def _thin_macho(cpu_type: int) -> bytes:
    return b"\xcf\xfa\xed\xfe" + cpu_type.to_bytes(4, "little") + bytes(24)


def _fat_macho(*cpu_types: int) -> bytes:
    header = b"\xca\xfe\xba\xbe" + len(cpu_types).to_bytes(4, "big")
    for cpu_type in cpu_types:
        header += cpu_type.to_bytes(4, "big") + bytes(16)
    return header + bytes(64)


def _pe(machine: int) -> bytes:
    header = bytearray(0x80 + 6)
    header[:2] = b"MZ"
    header[0x3C:0x40] = (0x80).to_bytes(4, "little")
    header[0x80:0x84] = b"PE\0\0"
    header[0x84:0x86] = machine.to_bytes(2, "little")
    return bytes(header)


def test_binary_arch_comes_from_the_executable_header():
    build_wheel = load_build_wheel_module()
    x86_64, arm64 = 0x01000007, 0x0100000C
    cases = {
        "elf-x86_64": (_elf(62), "x86_64"),
        "elf-aarch64": (_elf(183), "arm64"),
        "elf-riscv": (_elf(243), None),
        "macho-arm64": (_thin_macho(arm64), "arm64"),
        "macho-x86_64": (_thin_macho(x86_64), "x86_64"),
        "fat-universal": (_fat_macho(x86_64, arm64), "universal"),
        "fat-arm64": (_fat_macho(arm64), "arm64"),
        "pe-amd64": (_pe(0x8664), "x86_64"),
        "pe-arm64": (_pe(0xAA64), "arm64"),
        # A Java class file shares the universal binary magic.
        "java-class": (b"\xca\xfe\xba\xbe\x00\x00\x00\x41" + bytes(64), None),
        "text": (b"cli", None),
    }

    with tempfile.TemporaryDirectory() as tmp:
        for name, (content, expected) in cases.items():
            path = Path(tmp) / name
            path.write_bytes(content)
            assert build_wheel.get_binary_arch(path) == expected, name


def test_platform_tag_follows_the_binary_not_the_host():
    build_wheel = load_build_wheel_module()

    with tempfile.TemporaryDirectory() as tmp:
        binary = Path(tmp) / "cua-driver"
        binary.write_bytes(_elf(183))
        with mock.patch.object(build_wheel.platform, "machine", return_value="x86_64"), \
                mock.patch.object(build_wheel.os, "confstr", return_value="glibc 2.39"):
            assert build_wheel.select_platform_tag("linux", binary) == "manylinux_2_39_aarch64"

        binary.write_bytes(b"not an executable")
        with mock.patch.object(build_wheel.platform, "machine", return_value="AMD64"):
            assert build_wheel.select_platform_tag("windows", binary) == "win_amd64"


def test_plat_name_overrides_detection():
    build_wheel = load_build_wheel_module()
    missing = Path("/nonexistent/cua-driver")

    assert (
        build_wheel.select_platform_tag("linux", missing, "manylinux_2_31_x86_64")
        == "manylinux_2_31_x86_64"
    )
    assert (
        build_wheel.select_platform_tag("darwin", missing, "macosx-13.0-universal2")
        == "macosx_13_0_universal2"
    )
    try:
        build_wheel.select_platform_tag("linux", missing)
    except FileNotFoundError as error:
        assert build_wheel.CARGO_BUILD_HINT in str(error)
    else:
        raise AssertionError("detected a tag without a binary")


def test_binary_names_match_cargo_outputs():
    build_wheel = load_build_wheel_module()

    assert build_wheel.get_binary_names("darwin") == ("cua-driver", "libcua_driver_sdk.dylib")
    assert build_wheel.get_binary_names("linux") == ("cua-driver", "libcua_driver_sdk.so")
    assert build_wheel.get_binary_names("windows") == ("cua-driver.exe", "cua_driver_sdk.dll")


def test_default_target_dir_follows_cargo_target_dir():
    build_wheel = load_build_wheel_module()
    script_dir = BUILD_WHEEL_PATH.parent

    with mock.patch.dict(os.environ, {"CARGO_TARGET_DIR": "/elsewhere/target"}):
        assert build_wheel.get_default_target_dir(script_dir) == Path("/elsewhere/target")
    with mock.patch.dict(os.environ):
        os.environ.pop("CARGO_TARGET_DIR", None)
        assert build_wheel.get_default_target_dir(script_dir) == (
            script_dir.parent / "rust" / "target"
        )


def test_stage_artifacts_copies_the_local_build_into_the_package():
    build_wheel = load_build_wheel_module()

    with tempfile.TemporaryDirectory() as tmp:
        release = Path(tmp) / "target" / "release"
        release.mkdir(parents=True)
        (release / "cua-driver").write_bytes(b"cli")
        (release / "libcua_driver_sdk.so").write_bytes(b"sdk")
        package = Path(tmp) / "cua_driver"

        staged = build_wheel.stage_artifacts(
            release / "cua-driver", release / "libcua_driver_sdk.so", package, "linux"
        )

        assert staged == [package / "bin" / "cua-driver", package / "libcua_driver_sdk.so"]
        assert (package / "bin" / "cua-driver").read_bytes() == b"cli"
        assert (package / "libcua_driver_sdk.so").read_bytes() == b"sdk"


def test_stage_artifacts_bundles_the_windows_uia_worker_when_built():
    build_wheel = load_build_wheel_module()

    with tempfile.TemporaryDirectory() as tmp:
        release = Path(tmp) / "release"
        release.mkdir()
        for name in ("cua-driver.exe", "cua-driver-uia.exe", "cua_driver_sdk.dll"):
            (release / name).write_bytes(name.encode())
        package = Path(tmp) / "cua_driver"

        staged = build_wheel.stage_artifacts(
            release / "cua-driver.exe", release / "cua_driver_sdk.dll", package, "windows"
        )

        assert staged == [
            package / "bin" / "cua-driver.exe",
            package / "bin" / "cua-driver-uia.exe",
            package / "cua_driver_sdk.dll",
        ]


def test_missing_local_build_names_the_cargo_command():
    build_wheel = load_build_wheel_module()

    with tempfile.TemporaryDirectory() as tmp:
        missing = Path(tmp) / "release" / "cua-driver"
        try:
            build_wheel.stage_artifacts(
                missing, missing.with_name("libcua_driver_sdk.so"), Path(tmp) / "pkg", "linux"
            )
        except FileNotFoundError as error:
            assert build_wheel.CARGO_BUILD_HINT in str(error)
        else:
            raise AssertionError("stage_artifacts accepted a missing binary")
        assert not (Path(tmp) / "pkg").exists()


def test_build_script_never_downloads():
    source = BUILD_WHEEL_PATH.read_text()

    for needle in ("urllib", "http://", "https://", "trycua/cua", "cua.ai"):
        assert needle not in source


def test_license_metadata_stays_legacy_upload_compatible():
    pyproject = Path(__file__).resolve().parents[1] / "pyproject.toml"
    pyproject_text = pyproject.read_text()

    assert 'license = { text = "MIT" }' in pyproject_text
    assert 'license = "MIT"' not in pyproject_text
