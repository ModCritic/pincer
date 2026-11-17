#!/usr/bin/env python3
"""Write THIRD_PARTY_LICENSES.md: the license of every third-party crate linked
into the Linux `pincer` binary, with each crate's own license text.

What counts as linked: crates reached from the `pincer` package through normal
(not build or dev) dependencies on x86_64-unknown-linux-gnu, as cargo's resolver
sees the workspace. Proc-macro crates and everything only they depend on run at
compile time and are left out. Workspace crates are Pincer's own code (MIT, see
LICENSE) and are walked through, not listed. C sources bundled inside a `-sys`
crate keep their own license files; those are included too.

Where a crate offers a choice ("MIT OR Apache-2.0"), Pincer uses it under the
first of MIT, Apache-2.0, BSD-3-Clause, BSD-2-Clause, ISC, Zlib, Unicode-3.0 that
the expression offers, and the file says which.

usage: third_party_licenses.py [--check]   (run from anywhere; needs cargo and
       the crates already downloaded, since it runs `cargo metadata --offline`)
  --check  exit 1 if the file on disk differs from what would be written
"""
import json, os, re, subprocess, sys
from pathlib import Path

RUST = Path(__file__).resolve().parent.parent
OUT = RUST / "THIRD_PARTY_LICENSES.md"
TARGET = "x86_64-unknown-linux-gnu"
PREFERENCE = ["MIT", "Apache-2.0", "BSD-3-Clause", "BSD-2-Clause", "ISC", "Zlib", "Unicode-3.0",
              "Unicode-DFS-2016", "BSL-1.0", "CC0-1.0", "Unlicense", "MPL-2.0"]
LICENSE_FILE = re.compile(r"^(LICEN[CS]E|COPYING|COPYRIGHT|NOTICE|UNLICENSE)([-._].*)?$", re.I)


def metadata():
    cargo = os.environ.get("CARGO", str(Path.home() / ".cargo/bin/cargo"))
    out = subprocess.run([cargo, "metadata", "--format-version", "1", "--offline",
                          "--filter-platform", TARGET], cwd=RUST, check=True,
                         capture_output=True, text=True).stdout
    return json.loads(out)


def linked_packages(meta):
    """Packages `cargo tree -p pincer` reaches through normal, non-proc-macro edges.
    cargo tree resolves features for building `pincer` alone; `cargo metadata`'s
    graph unifies the whole workspace's features (pincer-bindgen's `cli`, dev-only
    features) and so lists crates the binary never links."""
    cargo = os.environ.get("CARGO", str(Path.home() / ".cargo/bin/cargo"))
    tree = subprocess.run([cargo, "tree", "-p", "pincer", "--offline", "--target", TARGET,
                           "--edges", "normal,no-proc-macro", "--prefix", "none", "--format", "{p}"],
                          cwd=RUST, check=True, capture_output=True, text=True).stdout
    wanted = set()
    for line in tree.splitlines():
        m = re.match(r"^(\S+) v(\S+)(?: \((.*)\))?", line.strip())
        if m and not (m.group(3) or "").startswith("/"):
            wanted.add((m.group(1), m.group(2)))
    return sorted((p for p in meta["packages"]
                   if p["source"] is not None and (p["name"], p["version"]) in wanted),
                  key=lambda p: (p["name"], p["version"]))


def elect(expression):
    if not expression:
        return None
    options = [o.strip(" ()") for o in re.split(r"\s+OR\s+|/", expression)]
    for choice in PREFERENCE:
        if choice in options:
            return choice
    return expression


def license_files(pkg):
    root = Path(pkg["manifest_path"]).parent
    found = []
    for path in sorted(root.iterdir()):
        if path.is_file() and LICENSE_FILE.match(path.name):
            found.append(path)
    if pkg["name"].endswith("-sys"):
        # Bundled C sources (e.g. zstd inside zstd-sys) keep their own license.
        for path in sorted(root.rglob("*")):
            rel = path.relative_to(root)
            if 1 <= len(rel.parts) - 1 <= 3 and path.is_file() and LICENSE_FILE.match(path.name) \
                    and "test" not in str(rel).lower():
                found.append(path)
    if pkg.get("license_file"):
        extra = root / pkg["license_file"]
        if extra.is_file() and extra not in found:
            found.append(extra)
    return found


def pick_texts(pkg, chosen, files):
    """The files that carry the chosen license (and any NOTICE), relative names."""
    if not files:
        return []
    def wants(path):
        name = path.name.upper()
        if "NOTICE" in name:
            return True
        if chosen == "MIT":
            return "MIT" in name or not re.search(r"APACHE|BSD|ZLIB|ISC|UNICODE|MPL|BOOST|CC0", name)
        if chosen == "Apache-2.0":
            return "APACHE" in name or not re.search(r"MIT|BSD|ZLIB|ISC|UNICODE|MPL|BOOST|CC0", name)
        return True
    chosen_files = [f for f in files if wants(f)]
    return chosen_files or files


def render(pkgs):
    rows, sections, mpl = [], {}, []
    for pkg in pkgs:
        expr = pkg.get("license") or ""
        chosen = elect(expr) or "see the crate"
        root = Path(pkg["manifest_path"]).parent
        files = pick_texts(pkg, chosen, license_files(pkg))
        rows.append((pkg["name"], pkg["version"], expr or "(no license field)", chosen))
        if chosen == "MPL-2.0":
            mpl.append(pkg)
        for f in files:
            text = f.read_text(errors="replace").strip()
            label = f"{pkg['name']} {pkg['version']}" + ("" if f.parent == root else f" ({f.relative_to(root)})")
            sections.setdefault(text, []).append(label)
        if not files:
            sections.setdefault(f"(The published crate ships no license file. License: {expr}. "
                                f"Source: {pkg.get('repository') or 'https://crates.io/crates/' + pkg['name']}.)",
                                []).append(f"{pkg['name']} {pkg['version']}")
    out = ["# Third-party licenses", "",
           "Licenses of the third-party Rust crates linked into the Linux `pincer` binary,",
           "generated by `rust/scripts/third_party_licenses.py` from cargo's resolved",
           f"dependency graph ({TARGET}, normal dependencies, proc-macros left out). Pincer's",
           "own code is MIT (see LICENSE). Source copied into Pincer is credited separately in",
           "THIRD_PARTY_NOTICES.md.", "",
           "Where a crate is offered under a choice of licenses, Pincer uses it under the one",
           "in the **Used under** column.", ""]
    if mpl:
        out += ["## MPL-2.0 crates", "",
                "These crates are covered by the Mozilla Public License 2.0, a file-level",
                "copyleft: their source files stay under MPL-2.0 and are available from",
                "crates.io at the versions listed; Pincer's own files are not affected.", ""]
        out += [f"- {p['name']} {p['version']}: https://crates.io/crates/{p['name']}/{p['version']}"
                for p in mpl]
        out.append("")
    out += ["## Crates", "", "| Crate | Version | License | Used under |", "|---|---|---|---|"]
    out += [f"| {n} | {v} | {e} | {c} |" for n, v, e, c in rows]
    out += ["", "## License texts", "",
            "Each text below is copied from the crate's published package; the crates that",
            "ship an identical text are listed together above it.", ""]
    for text, labels in sorted(sections.items(), key=lambda kv: kv[1][0]):
        out.append("### " + ", ".join(labels))
        out += ["", "```text", text.replace("```", "'''"), "```", ""]
    return "\n".join(out).rstrip() + "\n"


def main():
    text = render(linked_packages(metadata()))
    if "--check" in sys.argv:
        current = OUT.read_text() if OUT.exists() else ""
        if current != text:
            print(f"{OUT} is out of date: run rust/scripts/third_party_licenses.py", file=sys.stderr)
            sys.exit(1)
        return
    OUT.write_text(text)
    print(f"wrote {OUT}")


if __name__ == "__main__":
    main()
