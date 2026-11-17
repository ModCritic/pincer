// Bake Swift runtime rpaths into the pincer binary on macOS.
//
// The `screencapturekit` dep ships a small Swift-bridge shim that links
// against the Swift Concurrency runtime (`@rpath/libswift_Concurrency.dylib`
// and friends). Its own build.rs emits `cargo:rustc-link-arg=-Wl,-rpath,…`
// directives, but those only flow through to the binary linker when the
// emitting crate is the final binary crate — for transitive deps Cargo
// silently drops them. So we re-emit the same rpaths from here.
//
// On Windows, embed the Per-Monitor V2 DPI-awareness manifest so the
// process sees physical pixels (no DWM coordinate virtualization) at
// 125%/150%/200% scaling and clicks land where screenshots say they do.

fn main() {
    embed_git_sha();

    #[cfg(target_os = "windows")]
    {
        embed_resource::compile("pincer.rc", embed_resource::NONE);
    }

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    emit_sdk_framework_search_path();
    emit_swift_runtime_link_args();
}

/// Embeds the source revision as `PINCER_GIT_SHA` (read by `main` and
/// handed to `pincer_core::build_info::init`): the `PINCER_GIT_SHA`
/// build environment wins (release builds in containers without `.git`),
/// else `git rev-parse HEAD`, else empty. Reruns when HEAD or its ref moves.
///
/// Also embeds `PINCER_GIT_REVISION` for `pincer --version`: the short SHA,
/// with `-dirty` when tracked files differ from that commit (the meaning of
/// `git describe --dirty`). It is empty when git is unavailable, as in a
/// build from a source tarball. With the `PINCER_GIT_SHA` override it is that
/// SHA shortened (empty unless it is hex), since there is no tree to compare.
fn embed_git_sha() {
    println!("cargo:rerun-if-env-changed=PINCER_GIT_SHA");
    let from_env = std::env::var("PINCER_GIT_SHA").unwrap_or_default();
    let from_env = from_env.trim();
    let (sha, revision) = if !from_env.is_empty() {
        let short = if from_env.len() >= 7 && from_env.bytes().all(|b| b.is_ascii_hexdigit()) {
            from_env[..7].to_ascii_lowercase()
        } else {
            String::new()
        };
        (from_env.to_owned(), short)
    } else {
        let sha = git(&["rev-parse", "HEAD"]).unwrap_or_default();
        let revision = match git(&["rev-parse", "--short", "HEAD"]) {
            Some(short) if worktree_is_dirty() => format!("{short}-dirty"),
            Some(short) => short,
            None => String::new(),
        };
        (sha, revision)
    };
    // Rebuild when the checkout moves: HEAD itself (worktrees keep it in
    // their own git dir), the branch ref it names, and packed refs.
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        let git_dir = std::path::PathBuf::from(git_dir);
        println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
        if let Some(common) = git_common_dir() {
            println!(
                "cargo:rerun-if-changed={}",
                common.join("packed-refs").display()
            );
            if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
                println!(
                    "cargo:rerun-if-changed={}",
                    common.join(reference).display()
                );
            }
        }
        // Keep the `-dirty` mark current: rerun when the sources Cargo builds
        // the binary from change. Assumption: an edit to a tracked file
        // outside these paths (docs, scripts) only shows up at the next
        // rerun, which is fine because it cannot change the binary.
        if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
            let workspace = std::path::Path::new(&manifest_dir).join("../..");
            let workspace = workspace.canonicalize().unwrap_or(workspace);
            for path in ["crates", "Skills", "include", "Cargo.toml", "Cargo.lock"] {
                println!("cargo:rerun-if-changed={}", workspace.join(path).display());
            }
        }
    }
    println!("cargo:rustc-env=PINCER_GIT_SHA={sha}");
    println!("cargo:rustc-env=PINCER_GIT_REVISION={revision}");
}

/// The common git dir (where branch refs and packed-refs live, shared by
/// every worktree) as an absolute path. Plain `--git-common-dir` answers
/// relative to the current directory in a main checkout, for example
/// `../../../.git`, so resolve it against the directory git ran in.
fn git_common_dir() -> Option<std::path::PathBuf> {
    if let Some(common) = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]) {
        return Some(common.into());
    }
    // git before 2.31 has no --path-format.
    let common = std::path::PathBuf::from(git(&["rev-parse", "--git-common-dir"])?);
    if common.is_absolute() {
        Some(common)
    } else {
        Some(std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").ok()?).join(common))
    }
}

/// Whether tracked files differ from HEAD, staged or not. Untracked files do
/// not count. `--no-optional-locks` keeps `git status` from rewriting the
/// index, so a build never races a `git commit` running in the same tree.
fn worktree_is_dirty() -> bool {
    let Ok(dir) = std::env::var("CARGO_MANIFEST_DIR") else {
        return false;
    };
    let output = std::process::Command::new("git")
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--untracked-files=no",
        ])
        .current_dir(dir)
        .output();
    match output {
        Ok(output) if output.status.success() => !output.stdout.is_empty(),
        _ => false,
    }
}

fn git(args: &[&str]) -> Option<String> {
    let dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

fn emit_sdk_framework_search_path() {
    if let Some(sdk_root) = active_macos_sdk_root() {
        println!(
            "cargo:rustc-link-search=framework={}",
            sdk_root.join("System/Library/Frameworks").display()
        );
    }
}

fn emit_swift_runtime_link_args() {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    let mut dirs = BTreeSet::new();

    // `swiftc` links against the SDK's Swift TBDs on current Xcode releases.
    // The toolchain runtime directories below contain compatibility libraries,
    // but are not guaranteed to contain the full Swift runtime surface.
    if let Some(sdk_root) = active_macos_sdk_root() {
        let sdk_swift_dir = sdk_root.join("usr/lib/swift");
        if sdk_swift_dir.is_dir() {
            println!("cargo:rustc-link-search=native={}", sdk_swift_dir.display());
        }
    }

    if let Ok(out) = std::process::Command::new("xcode-select")
        .arg("-p")
        .output()
    {
        if out.status.success() {
            let xcode_path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let developer_dir = PathBuf::from(xcode_path);
            for sub in [
                "Toolchains/XcodeDefault.xctoolchain/usr/lib/swift/macosx",
                "Toolchains/XcodeDefault.xctoolchain/usr/lib/swift-5.5/macosx",
                "Toolchains/XcodeDefault.xctoolchain/usr/lib/swift-6.2/macosx",
                "usr/lib/swift/macosx",
                "usr/lib/swift-5.5/macosx",
                "usr/lib/swift-6.2/macosx",
            ] {
                dirs.insert(developer_dir.join(sub));
            }
        }
    }

    if let Ok(out) = Command::new("xcrun").args(["--find", "swiftc"]).output() {
        if out.status.success() {
            let swiftc = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
            if let Some(usr_dir) = swiftc.parent().and_then(Path::parent) {
                for sub in [
                    "lib/swift/macosx",
                    "lib/swift-5.5/macosx",
                    "lib/swift-6.2/macosx",
                ] {
                    dirs.insert(usr_dir.join(sub));
                }
            }
        }
    }

    for dir in dirs {
        if dir.is_dir() {
            println!("cargo:rustc-link-search=native={}", dir.display());
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", dir.display());
        }
    }
}

fn active_macos_sdk_root() -> Option<std::path::PathBuf> {
    std::env::var_os("SDKROOT")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_dir())
        .or_else(|| {
            let out = std::process::Command::new("xcrun")
                .args(["--sdk", "macosx", "--show-sdk-path"])
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
                .filter(|path| path.is_dir())
        })
}
