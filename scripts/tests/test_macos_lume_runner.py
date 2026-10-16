"""Focused tests for the macOS Lume helpers and the fixture builds they use.

These exercise the SIP-off TCC seed helpers directly, with fake ``lume``,
``ssh`` and ``scp`` commands and a scratch ``TCC.db``, and check the seed guide
and the fixture build scripts, so they need no macOS GUI session or VM.
"""

from __future__ import annotations

import os
import re
import shutil
import sqlite3
import subprocess
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]
SEED_TCC = REPO_ROOT / "tests/runners/macos-lume/seed-tcc.sh"
SEED_TCC_GUEST = REPO_ROOT / "tests/runners/macos-lume/seed-tcc-guest.sh"
HARNESS_GUIDE = REPO_ROOT / "docs/test-harnesses-guide.md"
RUNNER_GUIDE = REPO_ROOT / "tests/runners/macos-lume/README.md"
ELECTRON_BUILD = REPO_ROOT / "tests/fixtures/apps/cross-platform/electron/build.sh"
ELECTRON_LOCK = (
    REPO_ROOT / "tests/fixtures/apps/cross-platform/electron/package-lock.json"
)
TAURI_BUILD = REPO_ROOT / "tests/fixtures/apps/cross-platform/tauri/build.sh"

requires_jq = pytest.mark.skipif(shutil.which("jq") is None, reason="jq is required")


def _write_executable(path: Path, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(f"#!/bin/sh\n{body}", encoding="utf-8")
    path.chmod(0o755)


# --------------------------------------------------------------------------
# Syntax
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "script",
    [SEED_TCC, SEED_TCC_GUEST, ELECTRON_BUILD, TAURI_BUILD],
    ids=lambda path: f"{path.parent.name}/{path.name}",
)
def test_runner_scripts_have_valid_bash_syntax(script: Path) -> None:
    assert subprocess.run(["bash", "-n", str(script)], check=False).returncode == 0


# --------------------------------------------------------------------------
# Seed guide
# --------------------------------------------------------------------------


def test_seed_guide_keeps_the_login_keychain_searchable() -> None:
    text = RUNNER_GUIDE.read_text(encoding="utf-8")
    assert 'security list-keychains -d user -s "$SIGNING_KEYCHAIN"\n' not in text
    assert '"$HOME/Library/Keychains/login.keychain-db"' in text


# --------------------------------------------------------------------------
# SIP-off TCC seed helper
# --------------------------------------------------------------------------


def test_tcc_guest_seed_is_vm_and_sip_gated() -> None:
    text = SEED_TCC_GUEST.read_text(encoding="utf-8")
    assert re.search(r'\[\[ "\$\{MODEL\}" == VirtualMac\* \]\] \|\| fail', text)
    assert "csrutil status" in text
    assert 'fail "system TCC.db is SIP-protected' in text


def test_tcc_guest_seed_refuses_non_virtualmac_before_tcc_access() -> None:
    completed = subprocess.run(
        ["bash", str(SEED_TCC_GUEST)],
        capture_output=True,
        text=True,
        check=False,
    )
    assert completed.returncode == 2
    assert "seed-tcc:" in completed.stderr
    assert "sqlite3" not in completed.stderr


def _guest_seed_assignment(name: str) -> str:
    text = SEED_TCC_GUEST.read_text(encoding="utf-8")
    match = re.search(rf'^{name}="(?P<body>.*?)"', text, re.DOTALL | re.MULTILINE)
    assert match is not None
    return match.group("body")


def test_tcc_guest_seed_sql_executes_against_modern_tcc_schema(tmp_path: Path) -> None:
    sql_body = _guest_seed_assignment("SQL")
    verify_sql = _guest_seed_assignment("VERIFY_SQL")
    # The seed targets the local developer build unless told otherwise.
    assert re.search(
        r'^EXPECTED_CLIENT="\$\{CUA_TCC_EXPECTED_CLIENT:-com\.trycua\.driver\.local\}"$',
        SEED_TCC_GUEST.read_text(encoding="utf-8"),
        re.MULTILINE,
    )
    client = "com.trycua.driver.local"
    client_type = "0"
    csreq_hex = "01020304"
    substitutions = {
        "${CLIENT_SQL}": client,
        "${CLIENT_TYPE}": client_type,
        "${CSREQ_HEX}": csreq_hex,
    }
    for old, new in substitutions.items():
        sql_body = sql_body.replace(old, new)
        verify_sql = verify_sql.replace(old, new)

    db = tmp_path / "TCC.db"
    with sqlite3.connect(db) as conn:
        conn.executescript(
            """
            CREATE TABLE access(
              service TEXT NOT NULL,
              client TEXT NOT NULL,
              client_type INTEGER NOT NULL,
              auth_value INTEGER,
              auth_reason INTEGER,
              auth_version INTEGER,
              csreq BLOB,
              flags INTEGER,
              indirect_object_identifier_type INTEGER,
              indirect_object_identifier TEXT DEFAULT 'UNUSED',
              indirect_object_code_identity BLOB,
              last_modified INTEGER DEFAULT 0
            );
            """
        )
        conn.executescript(sql_body)
        row_count = conn.execute(verify_sql).fetchone()[0]
        rows = conn.execute(
            """
            SELECT service, client, client_type, auth_value, auth_reason,
                   auth_version, hex(csreq), indirect_object_identifier
              FROM access
             ORDER BY service
            """
        ).fetchall()
        # Verification counts only allowed grants, not rows that merely exist.
        conn.execute("UPDATE access SET auth_value=0 WHERE service='kTCCServiceScreenCapture'")
        denied_count = conn.execute(verify_sql).fetchone()[0]

    assert row_count == 2
    assert denied_count == 1
    assert rows == [
        (
            "kTCCServiceAccessibility",
            client,
            0,
            2,
            2,
            1,
            csreq_hex.upper(),
            "UNUSED",
        ),
        (
            "kTCCServiceScreenCapture",
            client,
            0,
            2,
            2,
            1,
            csreq_hex.upper(),
            "UNUSED",
        ),
    ]


def test_tcc_guest_seed_requires_certificate_backed_requirement_by_default() -> None:
    text = SEED_TCC_GUEST.read_text(encoding="utf-8")
    assert '[[ "${ALLOW_ADHOC}" != 1 ]]' in text
    assert "^Signature=adhoc$" in text
    assert 'certificate (leaf|root) = H"[[:xdigit:]]{40}"' in text
    assert "is not signed with a certificate-backed identity" in text
    assert 'classify_designated_requirement "${REQUIREMENT}" "${SIGNING_IDENTIFIER}"' in text


# The designated requirement codesign derives for the released, notarized
# CuaDriver.app (Developer ID Application: Cua AI, Inc.).
RELEASED_DEVELOPER_ID_DR = (
    'identifier "com.trycua.driver" and anchor apple generic and '
    "certificate 1[field.1.2.840.113635.100.6.2.6] /* exists */ and "
    "certificate leaf[field.1.2.840.113635.100.6.1.13] /* exists */ and "
    "certificate leaf[subject.OU] = YCK386LBJ7"
)
LOCAL_LEAF_HASH_DR = (
    'identifier "com.trycua.driver.local" and '
    'certificate leaf = H"0123456789abcdef0123456789abcdef01234567"'
)


def _classify_requirement(requirement: str, identifier: str = "") -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            "bash",
            "-c",
            'script="$1" requirement="$2" identifier="$3"; set --; '
            'source "$script"; classify_designated_requirement "$requirement" "$identifier"',
            "classify-test",
            str(SEED_TCC_GUEST),
            requirement,
            identifier,
        ],
        capture_output=True,
        text=True,
        env={**os.environ, "CUA_TCC_SEED_LIB_ONLY": "1"},
        check=False,
    )


@pytest.mark.parametrize(
    ("requirement", "identifier", "kind"),
    [
        pytest.param(RELEASED_DEVELOPER_ID_DR, "com.trycua.driver", "developer-id", id="released-developer-id"),
        pytest.param(RELEASED_DEVELOPER_ID_DR, "", "developer-id", id="released-developer-id-no-identifier"),
        pytest.param(
            RELEASED_DEVELOPER_ID_DR.replace("= YCK386LBJ7", '= "YCK386LBJ7"'),
            "com.trycua.driver",
            "developer-id",
            id="quoted-team-id",
        ),
        pytest.param(LOCAL_LEAF_HASH_DR, "com.trycua.driver.local", "leaf-hash", id="local-leaf-hash"),
        pytest.param(
            'identifier "com.trycua.driver.local" and anchor apple generic and '
            'certificate root = H"89abcdef0123456789abcdef0123456789abcdef"',
            "com.trycua.driver.local",
            "leaf-hash",
            id="root-hash",
        ),
    ],
)
def test_tcc_guest_seed_accepts_certificate_backed_requirements(
    requirement: str, identifier: str, kind: str
) -> None:
    completed = _classify_requirement(requirement, identifier)
    assert completed.returncode == 0, completed.stderr
    assert completed.stdout.strip() == kind


@pytest.mark.parametrize(
    ("requirement", "identifier"),
    [
        pytest.param('cdhash H"0123456789abcdef0123456789abcdef01234567"', "", id="adhoc-cdhash"),
        pytest.param(
            'identifier "com.trycua.driver.local" and cdhash H"0123456789abcdef0123456789abcdef01234567"',
            "com.trycua.driver.local",
            id="adhoc-identifier-cdhash",
        ),
        pytest.param("", "", id="empty"),
        pytest.param(
            RELEASED_DEVELOPER_ID_DR.replace("YCK386LBJ7", "YCK386LBJ"),
            "com.trycua.driver",
            id="short-team-id",
        ),
        pytest.param(
            RELEASED_DEVELOPER_ID_DR.replace("YCK386LBJ7", "yck386lbj7"),
            "com.trycua.driver",
            id="lowercase-team-id",
        ),
        pytest.param(
            RELEASED_DEVELOPER_ID_DR.replace("YCK386LBJ7", "YCK386LBJ7 or cdhash H\"00\""),
            "com.trycua.driver",
            id="trailing-disjunction",
        ),
        pytest.param(
            RELEASED_DEVELOPER_ID_DR.replace("= YCK386LBJ7", '= "YCK386LBJ7'),
            "com.trycua.driver",
            id="unbalanced-quote",
        ),
        pytest.param(
            RELEASED_DEVELOPER_ID_DR.replace("anchor apple generic and ", ""),
            "com.trycua.driver",
            id="missing-apple-anchor",
        ),
        pytest.param(
            RELEASED_DEVELOPER_ID_DR.replace('identifier "com.trycua.driver" and ', ""),
            "",
            id="missing-identifier",
        ),
        pytest.param(RELEASED_DEVELOPER_ID_DR, "com.trycua.driver.local", id="identifier-mismatch"),
    ],
)
def test_tcc_guest_seed_rejects_non_certificate_backed_requirements(
    requirement: str, identifier: str
) -> None:
    completed = _classify_requirement(requirement, identifier)
    assert completed.returncode == 1
    assert completed.stdout.strip() == "rejected"


def test_tcc_guest_seed_library_mode_fails_closed_when_executed() -> None:
    completed = subprocess.run(
        ["bash", str(SEED_TCC_GUEST)],
        capture_output=True,
        text=True,
        env={**os.environ, "CUA_TCC_SEED_LIB_ONLY": "1"},
        check=False,
    )
    assert completed.returncode == 2
    assert "valid only when this script is sourced" in completed.stderr


@pytest.mark.parametrize(
    "document",
    [HARNESS_GUIDE, RUNNER_GUIDE],
    ids=lambda path: path.name,
)
def test_harness_guides_route_automated_tcc_through_guarded_helper(document: Path) -> None:
    text = document.read_text(encoding="utf-8")
    assert "tests/runners/macos-lume/seed-tcc.sh" in text
    assert "VirtualMac" in text
    assert "SIP" in text
    assert "certificate" in text
    assert "TCC.db" in text
    assert "rows alone" in text or "helper exit alone" in text


def test_tcc_host_seed_parses_lume_json_with_jq_not_python() -> None:
    text = SEED_TCC.read_text(encoding="utf-8")
    assert "python3 -c" not in text
    assert "command -v python3" not in text
    assert "command -v jq" in text
    assert "jq -r --arg name" in text


@requires_jq
@pytest.mark.parametrize(
    ("payload", "message"),
    [
        ('[{"status":"stopped","ipAddress":null}]', "expected running VM with an IP"),
        ('[{"status":"running","ipAddress":"192.0.2.10","sshAvailable":false}]', "SSH is not available yet"),
        ("[]", "lume get returned no VM record"),
    ],
)
def test_tcc_host_seed_refuses_unready_vms(tmp_path: Path, payload: str, message: str) -> None:
    fake_bin = tmp_path / "bin"
    for tool in ("scp", "ssh"):
        _write_executable(fake_bin / tool, "exit 99\n")
    _write_executable(fake_bin / "lume", f"printf '%s\\n' '{payload}'\n")
    completed = subprocess.run(
        ["bash", str(SEED_TCC), "--timeout", "5", "worker-a"],
        capture_output=True,
        stdin=subprocess.DEVNULL,
        text=True,
        env={**os.environ, "PATH": f"{fake_bin}:{os.environ['PATH']}"},
        check=False,
    )
    assert completed.returncode != 0
    assert f"worker-a: {message}" in completed.stderr
    assert "installing guest TCC seeder" not in completed.stdout


@requires_jq
def test_tcc_host_seed_runs_the_guest_helper_once_per_vm(tmp_path: Path) -> None:
    fake_bin = tmp_path / "bin"
    fake_lume = fake_bin / "lume"
    fake_scp = fake_bin / "scp"
    fake_ssh = fake_bin / "ssh"
    log = tmp_path / "transport.log"
    _write_executable(
        fake_lume,
        """printf 'lume %s\n' "$*" >> "$CUA_TEST_TRANSPORT_LOG"
if [ "$1" = "get" ]; then
  case "$2" in
    worker-a) ip="192.0.2.10" ;;
    worker-b) ip="192.0.2.11" ;;
    *) exit 2 ;;
  esac
  printf '[{"name":"%s","status":"running","sshAvailable":true,"ipAddress":"%s"}]\n' "$2" "$ip"
fi
""",
    )
    _write_executable(
        fake_scp,
        """printf 'scp askpass=%s pass=%s %s\n' "${SSH_ASKPASS_REQUIRE:-}" "${CUA_TCC_SSH_PASSWORD:-}" "$*" >> "$CUA_TEST_TRANSPORT_LOG"
""",
    )
    _write_executable(
        fake_ssh,
        """stdin="$(cat || true)"
printf 'ssh askpass=%s pass=%s stdin=%s %s\n' "${SSH_ASKPASS_REQUIRE:-}" "${CUA_TCC_SSH_PASSWORD:-}" "$stdin" "$*" >> "$CUA_TEST_TRANSPORT_LOG"
""",
    )

    completed = subprocess.run(
        [
            "bash",
            str(SEED_TCC),
            "--timeout",
            "5",
            "worker-a",
            "worker-b",
        ],
        capture_output=True,
        stdin=subprocess.DEVNULL,
        text=True,
        env={
            **os.environ,
            "PATH": f"{fake_bin}:/usr/bin:/bin",
            "CUA_TEST_TRANSPORT_LOG": str(log),
            "CUA_TCC_SUDO_PASSWORD": "fixture-password",
            "LUME_SSH_USER": "lume",
            "LUME_STORAGE": "",
        },
        check=False,
    )

    assert completed.returncode == 0, completed.stderr
    calls = log.read_text(encoding="utf-8").splitlines()
    assert len(calls) == 10
    assert calls[0] == "lume get worker-a --format json"
    assert calls[1].startswith("scp askpass=force pass=lume -o StrictHostKeyChecking=no ")
    assert "seed-tcc-guest.sh" in calls[1]
    assert calls[1].endswith(" lume@192.0.2.10:/tmp/cua-driver-seed-tcc-guest.sh")
    assert calls[2].endswith(" lume@192.0.2.10 chmod 700 /tmp/cua-driver-seed-tcc-guest.sh")
    assert "stdin=fixture-password" in calls[3]
    assert "CUA_TCC_READ_SUDO_PASSWORD=1 /tmp/cua-driver-seed-tcc-guest.sh" in calls[3]
    assert calls[4].endswith(" lume@192.0.2.10 /bin/rm -f /tmp/cua-driver-seed-tcc-guest.sh")
    assert calls[5] == "lume get worker-b --format json"
    assert calls[6].endswith(" lume@192.0.2.11:/tmp/cua-driver-seed-tcc-guest.sh")
    assert calls[7].endswith(" lume@192.0.2.11 chmod 700 /tmp/cua-driver-seed-tcc-guest.sh")
    assert "stdin=fixture-password" in calls[8]
    assert "CUA_TCC_READ_SUDO_PASSWORD=1 /tmp/cua-driver-seed-tcc-guest.sh" in calls[8]
    assert calls[9].endswith(" lume@192.0.2.11 /bin/rm -f /tmp/cua-driver-seed-tcc-guest.sh")


# --------------------------------------------------------------------------
# Fixture builds
# --------------------------------------------------------------------------


def test_fixture_builds_use_fresh_run_owned_state() -> None:
    tauri = TAURI_BUILD.read_text(encoding="utf-8")
    electron = ELECTRON_BUILD.read_text(encoding="utf-8")
    assert ELECTRON_LOCK.is_file()
    assert 'tauriTargetDir="${CARGO_TARGET_DIR:-' in tauri
    assert 'srcBin="$tauriTargetDir/release/' in tauri
    assert 'CUA_E2E_FRESH_FIXTURE_STATE:-0}" == 1' in electron
    assert "npm ci --silent" in electron
