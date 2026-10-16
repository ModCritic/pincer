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

    assert build_wheel.get_wheel_tag("darwin", "universal") == "py3-none-macosx_13_0_universal2"
    assert build_wheel.get_wheel_tag("linux", "x86_64") == "py3-none-manylinux_2_31_x86_64"
    assert build_wheel.get_wheel_tag("linux", "arm64") == "py3-none-manylinux_2_31_aarch64"
    assert build_wheel.get_wheel_tag("windows", "x86_64") == "py3-none-win_amd64"
    assert build_wheel.get_wheel_tag("windows", "arm64") == "py3-none-win_arm64"


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
