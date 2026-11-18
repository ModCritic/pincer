//! Which web engine a program or process runs.
//!
//! On a Chromium or Gecko surface an AT-SPI write can change a field's text,
//! which is all an accessibility read-back sees, without the page's scripts
//! ever getting the event a typed character fires. The tools ask this module
//! so they never report such a read-back as a confirmed change.

use std::path::{Path, PathBuf};

/// A browser engine whose page content Pincer reads through AT-SPI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WebEngine {
    /// Chromium, its browsers, and Electron or CEF apps.
    Chromium,
    /// Gecko: Firefox, Thunderbird and their forks.
    Gecko,
}

impl WebEngine {
    /// The engine's name in a structured result.
    pub(crate) fn name(self) -> &'static str {
        match self {
            WebEngine::Chromium => "chromium",
            WebEngine::Gecko => "gecko",
        }
    }
}

/// The executable `/proc/<pid>/exe` names, without the " (deleted)" the
/// kernel appends once a package update replaced the file.
pub(crate) fn process_executable(pid: u32) -> Option<PathBuf> {
    let path = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let text = path.to_str()?;
    Some(PathBuf::from(
        text.strip_suffix(" (deleted)").unwrap_or(text),
    ))
}

/// Whether `executable` is a Gecko application binary. Every Gecko app
/// (Firefox, Firefox ESR, Thunderbird, LibreWolf and the other forks, as
/// distribution packages, tarballs, Flatpaks or snaps) ships its engine as
/// `libxul.so` next to the binary, and nothing else does. Content processes
/// run the same binary, so this holds for them too.
pub(crate) fn is_gecko_executable(executable: &Path) -> bool {
    executable
        .parent()
        .is_some_and(|dir| dir.join("libxul.so").is_file())
}

/// Whether process `pid` runs a Gecko application ([`is_gecko_executable`]).
pub(crate) fn is_gecko_process(pid: u32) -> bool {
    process_executable(pid).is_some_and(|exe| is_gecko_executable(&exe))
}

// ── Chromium ─────────────────────────────────────────────────────────────────
//
// One detection for every question "is this Chromium?": which programs
// launch_app gives --force-renderer-accessibility, which processes the
// browser tools classify as Chromium, and which processes the input tools
// treat as Chromium embedders (tools/impl_/toolkit.rs adds the helper-process fingerprint
// only a running process has).

/// Words of a program name that name a Chromium-family product.
const CHROMIUM_NAME_WORDS: &[&str] = &[
    "chrome", "chromium", "electron", "brave", "msedge", "vivaldi", "opera", "thorium", "iridium",
];

/// Whether a program or process name names a Chromium-family program. The
/// name is split at every character that is not an ASCII letter or digit and
/// matches when a word is a Chromium product (`google-chrome-stable`,
/// `chromium-browser`, `brave-browser`), "electron" with a version
/// (`electron33`, Arch's per-version Electron), "edge" after "microsoft"
/// (`microsoft-edge-stable`), or "yandex" with "browser". Words, not
/// substrings: `hedgewars` and `knowledge` hold "edge" but are not Edge.
/// Callers pass a file name, not a whole path, so a folder called "chrome"
/// does not make every program in it Chromium.
pub(crate) fn is_chromium_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let words: Vec<&str> = lower
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let has = |wanted: &str| words.contains(&wanted);
    words.iter().any(|word| {
        CHROMIUM_NAME_WORDS.contains(word)
            || word.strip_prefix("electron").is_some_and(|version| {
                !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit())
            })
    }) || (has("edge") && has("microsoft"))
        || (has("yandex") && has("browser"))
}

/// Files a Chromium build ships beside its binary: the UI resource packs
/// every Chromium, Chrome, Brave and Electron build has, CEF's library, and
/// an Electron app's bundle. Checked by name only (a few `stat` calls), so it
/// is cheap enough for the per-input-call embedder check.
const CHROMIUM_PAYLOAD: &[&str] = &[
    "chrome_100_percent.pak",
    "resources.pak",
    "libcef.so",
    "resources/app.asar",
];

/// Whether `dir` holds a Chromium build's payload ([`CHROMIUM_PAYLOAD`]).
fn has_chromium_payload(dir: &Path) -> bool {
    CHROMIUM_PAYLOAD.iter().any(|file| dir.join(file).is_file())
}

/// A wrapper script longer than this is not read: launch wrappers are a
/// few lines (claude-desktop's is 1.2 KB, Discord's 1.1 KB, Chrome's 2 KB).
const WRAPPER_MAX_BYTES: u64 = 64 * 1024;
/// Wrappers that exec wrappers are followed this many levels deep.
const WRAPPER_MAX_DEPTH: usize = 3;
/// Expansions of one word that are tried at most.
const WORD_MAX_EXPANSIONS: usize = 16;

/// Whether running `executable` runs Chromium: a Chromium-family browser,
/// or an Electron or CEF app. `executable` is the program as launch_app
/// resolved it, or a process's `/proc/<pid>/exe`. It is, in order:
///
/// 1. named like a Chromium program ([`is_chromium_name`]), as given or
///    after resolving symlinks (`/usr/bin/google-chrome-stable`);
/// 2. a file beside a Chromium payload ([`CHROMIUM_PAYLOAD`]): Chromium's
///    own binary, an Electron app's (`/usr/lib/claude-desktop/claude-desktop`,
///    `~/.config/discord/app-1.0.169/Discord`), or a wrapper script kept in
///    the install folder (`/opt/google/chrome/google-chrome`);
/// 3. a shell script of at most [`WRAPPER_MAX_BYTES`] whose every `exec`
///    runs one of these (`/usr/bin/claude-desktop`, `/usr/bin/discord`),
///    followed [`WRAPPER_MAX_DEPTH`] levels deep. Exec targets are expanded
///    with the script's simple variable assignments and the environment; a
///    target built from a command's output, or one that is not there, is
///    left out, and a script with no target left is not Chromium.
///
/// A binary named otherwise with no payload beside it (an AppImage, a
/// Flatpak's `flatpak run`) is not recognized.
pub(crate) fn is_chromium_executable(executable: &Path) -> bool {
    program_runs_chromium(executable, &|name| std::env::var(name).ok(), 0)
}

/// Whether process `pid`'s executable is Chromium ([`is_chromium_executable`]).
pub(crate) fn is_chromium_process(pid: u32) -> bool {
    process_executable(pid).is_some_and(|exe| is_chromium_executable(&exe))
}

fn file_name_is_chromium(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_chromium_name)
}

fn program_runs_chromium(path: &Path, env: &dyn Fn(&str) -> Option<String>, depth: usize) -> bool {
    if file_name_is_chromium(path) {
        return true;
    }
    let Ok(real) = std::fs::canonicalize(path) else {
        return false;
    };
    if file_name_is_chromium(&real) || real.parent().is_some_and(has_chromium_payload) {
        return true;
    }
    if depth >= WRAPPER_MAX_DEPTH {
        return false;
    }
    let Some(script) = read_shell_script(&real) else {
        return false;
    };
    // Every program the wrapper can exec must be Chromium: a script that
    // runs Chromium in one branch and something else in another gets no
    // Chromium flags.
    let dir = real.parent().map(Path::to_path_buf);
    let targets: Vec<PathBuf> = wrapper_exec_targets(&script, env)
        .into_iter()
        .filter_map(|target| resolve_target(&target, dir.as_deref(), env))
        .collect();
    !targets.is_empty()
        && targets
            .iter()
            .all(|target| program_runs_chromium(target, env, depth + 1))
}

/// The text of `path` when it is a short POSIX-shell script: at most
/// [`WRAPPER_MAX_BYTES`], UTF-8, and a `#!` line naming sh, bash, dash, ksh
/// or zsh (directly or through `env`).
fn read_shell_script(path: &Path) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() || file.metadata().ok()?.len() > WRAPPER_MAX_BYTES {
        return None;
    }
    let mut text = String::new();
    file.take(WRAPPER_MAX_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    let shebang = text.lines().next()?.strip_prefix("#!")?;
    let mut words = shebang.split_whitespace();
    let mut interpreter = words.next()?.rsplit('/').next()?;
    if interpreter == "env" {
        interpreter = words.find(|word| !word.starts_with('-'))?;
    }
    matches!(interpreter, "sh" | "bash" | "dash" | "ksh" | "zsh").then_some(text)
}

/// The words of one shell line, with quotes kept (expansion needs to know
/// what was single-quoted). A `#` that starts a word ends the line.
fn shell_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match quote {
            Some(q) => {
                word.push(ch);
                if ch == q {
                    quote = None;
                } else if ch == '\\' && q == '"' {
                    word.extend(chars.next());
                }
            }
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                word.push(ch);
            }
            None if ch == '\\' => {
                word.push(ch);
                word.extend(chars.next());
            }
            None if ch == '#' && word.is_empty() => break,
            None if ch.is_whitespace() || ch == ';' => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                if ch == ';' {
                    words.push(";".to_owned());
                }
            }
            None => word.push(ch),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// Words after which a new command starts.
const COMMAND_STARTS: &[&str] = &[";", "&&", "||", "|", "then", "else", "do", "{", "(", "!"];

/// The program each `exec` in `script` runs, expanded with the script's own
/// simple assignments and `env` ([`expand_word`]). Each target is one
/// candidate path or command name; an unexpandable target is left out.
fn wrapper_exec_targets(script: &str, env: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let lines: Vec<Vec<String>> = script.lines().map(shell_words).collect();
    // Every value each variable is assigned anywhere in the script: which
    // branch ran is unknown, so each is a candidate.
    let mut assigned: std::collections::HashMap<String, Vec<String>> = Default::default();
    for words in &lines {
        let mut at_command = true;
        for word in words {
            if at_command {
                if let Some((name, value)) = assignment(word) {
                    assigned
                        .entry(name.to_owned())
                        .or_default()
                        .push(value.to_owned());
                    continue;
                }
                if matches!(word.as_str(), "export" | "local" | "readonly") {
                    continue;
                }
            }
            at_command = COMMAND_STARTS.contains(&word.as_str());
        }
    }
    let mut targets = Vec::new();
    for words in &lines {
        for (at, word) in words.iter().enumerate() {
            let starts_command = at == 0 || COMMAND_STARTS.contains(&words[at - 1].as_str());
            if word != "exec" || !starts_command {
                continue;
            }
            let Some(target) = exec_target(&words[at + 1..]) else {
                continue;
            };
            for expanded in expand_word(target, &assigned, env, 0) {
                if !expanded.is_empty() && !targets.contains(&expanded) {
                    targets.push(expanded);
                }
            }
        }
    }
    targets
}

/// `NAME=value` as a command-position word: `(NAME, value)`.
fn assignment(word: &str) -> Option<(&str, &str)> {
    let (name, value) = word.split_once('=')?;
    let mut chars = name.chars();
    let first = chars.next()?;
    ((first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_'))
    .then_some((name, value))
}

/// The program word of an `exec` command's arguments: past exec's own
/// options (`-a NAME`, `-c`, `-l`) and an `env` with its assignments and
/// options. `None` for an `exec` that only redirects.
fn exec_target(args: &[String]) -> Option<&str> {
    let mut words = args.iter().map(String::as_str).peekable();
    while let Some(word) = words.next() {
        match word {
            "-a" => {
                words.next();
            }
            "-c" | "-l" | "-cl" | "-lc" | "env" | "/usr/bin/env" | "/bin/env" => {}
            _ if word.starts_with('-') => {}
            _ if assignment(word).is_some() => {}
            _ if word.starts_with(['<', '>'])
                || word
                    .trim_start_matches(|ch: char| ch.is_ascii_digit())
                    .starts_with(['<', '>']) =>
            {
                return None
            }
            ";" | "&&" | "||" | "|" => return None,
            _ => return Some(word),
        }
    }
    None
}

/// The values `word` can take: quotes removed, and `$NAME`, `${NAME}`,
/// `${NAME:-default}` and `${NAME-default}` replaced by each value the
/// script assigns NAME (expanded in turn) or else NAME's value in `env`
/// (empty when unset, as in the shell). A word with a command substitution,
/// a positional parameter or any other expansion has no known value and
/// gives none. At most [`WORD_MAX_EXPANSIONS`] values.
fn expand_word(
    word: &str,
    assigned: &std::collections::HashMap<String, Vec<String>>,
    env: &dyn Fn(&str) -> Option<String>,
    depth: usize,
) -> Vec<String> {
    if depth > 4 || word.contains('`') || word.contains("$(") {
        return Vec::new();
    }
    let mut values = vec![String::new()];
    let mut chars = word.chars().peekable();
    let (mut single, mut double) = (false, false);
    while let Some(ch) = chars.next() {
        let piece: Vec<String> = match ch {
            '\'' if !double => {
                single = !single;
                continue;
            }
            '"' if !single => {
                double = !double;
                continue;
            }
            '\\' if !single => chars
                .next()
                .map(|ch| vec![ch.to_string()])
                .unwrap_or_default(),
            '$' if !single => {
                let (name, default) = if chars.peek() == Some(&'{') {
                    chars.next();
                    let mut inner = String::new();
                    let mut open = 1;
                    for ch in chars.by_ref() {
                        match ch {
                            '{' => open += 1,
                            '}' => {
                                open -= 1;
                                if open == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        inner.push(ch);
                    }
                    if open != 0 {
                        return Vec::new();
                    }
                    let split = inner
                        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                        .unwrap_or(inner.len());
                    let (name, rest) = inner.split_at(split);
                    let default = match rest {
                        "" => None,
                        _ => match rest.strip_prefix(":-").or_else(|| rest.strip_prefix('-')) {
                            Some(default) => Some(default.to_owned()),
                            None => return Vec::new(),
                        },
                    };
                    (name.to_owned(), default)
                } else {
                    let mut name = String::new();
                    while let Some(&ch) = chars.peek() {
                        if !(ch.is_ascii_alphanumeric() || ch == '_') {
                            break;
                        }
                        name.push(ch);
                        chars.next();
                    }
                    (name, None)
                };
                if assignment(&format!("{name}=")).is_none() {
                    // `$1`, `$@`, `$0`, `$$` or a lone `$`: no known value.
                    return Vec::new();
                }
                let mut options: Vec<String> = match assigned.get(&name) {
                    Some(raw) => raw
                        .iter()
                        .flat_map(|value| expand_word(value, assigned, env, depth + 1))
                        .collect(),
                    None => vec![env(&name).unwrap_or_default()],
                };
                if let Some(default) = default {
                    let defaults = expand_word(&default, assigned, env, depth + 1);
                    options = options
                        .into_iter()
                        .flat_map(|value| {
                            if value.is_empty() {
                                defaults.clone()
                            } else {
                                vec![value]
                            }
                        })
                        .collect();
                }
                options
            }
            _ => vec![ch.to_string()],
        };
        values = values
            .iter()
            .flat_map(|head| piece.iter().map(move |tail| format!("{head}{tail}")))
            .take(WORD_MAX_EXPANSIONS)
            .collect();
        if values.is_empty() {
            return values;
        }
    }
    values.dedup();
    values
}

/// A wrapper's exec target as a path to check: an absolute path as it is,
/// a bare command name looked up on `PATH`, a relative path from the
/// script's folder. `None` when nothing runnable is there.
fn resolve_target(
    target: &str,
    script_dir: Option<&Path>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<PathBuf> {
    let path = Path::new(target);
    let found = if path.is_absolute() {
        path.to_path_buf()
    } else if target.contains('/') {
        script_dir?.join(path)
    } else {
        let search = env("PATH")?;
        std::env::split_paths(&search)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(target))
            .find(|candidate| candidate.is_file())?
    };
    found.is_file().then_some(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binary_beside_libxul_is_gecko() {
        let dir = tempfile::tempdir().unwrap();
        let firefox = dir.path().join("firefox");
        std::fs::write(&firefox, b"\x7fELF").unwrap();
        assert!(!is_gecko_executable(&firefox));
        std::fs::write(dir.path().join("libxul.so"), b"\x7fELF").unwrap();
        assert!(is_gecko_executable(&firefox));
        // A folder called libxul.so is not the engine.
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir(other.path().join("libxul.so")).unwrap();
        assert!(!is_gecko_executable(&other.path().join("app")));
    }

    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;

    fn write_exec(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A Chromium-family install: a binary beside its resource pack.
    fn chromium_install(dir: &Path, binary: &str) -> PathBuf {
        let path = dir.join(binary);
        write_exec(&path, "\x7fELF");
        std::fs::write(dir.join("chrome_100_percent.pak"), b"pak").unwrap();
        path
    }

    fn runs_chromium(path: &Path, env: &[(&str, &str)]) -> bool {
        let env: HashMap<String, String> = env
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        program_runs_chromium(path, &|name| env.get(name).cloned(), 0)
    }

    #[test]
    fn chromium_names_are_matched_by_word() {
        for name in [
            "chrome",
            "google-chrome-stable",
            "chromium-browser",
            "brave-browser",
            "msedge",
            "microsoft-edge-stable",
            "vivaldi-bin",
            "electron",
            "electron33",
            "PincerTestHarness.Electron",
            "yandex-browser",
        ] {
            assert!(is_chromium_name(name), "{name}");
        }
        for name in [
            "firefox",
            "gnome-text-editor",
            "hedgewars",
            "knowledge",
            "edge-tts",
            "electronics",
            "arc",
            "claude-desktop",
            "Discord",
            "search-worker",
            "",
        ] {
            assert!(!is_chromium_name(name), "{name}");
        }
    }

    #[test]
    fn a_binary_beside_a_chromium_payload_is_chromium() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        write_exec(&app, "\x7fELF");
        assert!(!runs_chromium(&app, &[]));
        let electron = tempfile::tempdir().unwrap();
        let discord = electron.path().join("Discord");
        write_exec(&discord, "\x7fELF");
        std::fs::create_dir(electron.path().join("resources")).unwrap();
        std::fs::write(electron.path().join("resources/app.asar"), b"asar").unwrap();
        assert!(runs_chromium(&discord, &[]));
        // Through a symlink named otherwise, as ~/.config/discord/Discord is.
        let link = dir.path().join("Discord");
        std::os::unix::fs::symlink(&discord, &link).unwrap();
        assert!(runs_chromium(&link, &[]));
        let chromium = tempfile::tempdir().unwrap();
        assert!(runs_chromium(
            &chromium_install(chromium.path(), "app"),
            &[]
        ));
    }

    #[test]
    fn a_wrapper_script_that_execs_an_electron_binary_is_chromium() {
        let root = tempfile::tempdir().unwrap();
        let install = root.path().join("lib/claude-desktop");
        let binary = chromium_install(&install, "claude-desktop");
        let wrapper = root.path().join("bin/claude-desktop");
        write_exec(
            &wrapper,
            &format!(
                "#!/bin/bash\n# Launcher.\nset -euo pipefail\nflags=()\n\
                 conf=\"${{XDG_CONFIG_HOME:-${{HOME:-}}/.config}}/flags.conf\"\n\
                 platform=()\n\
                 if [[ -n ${{WAYLAND_DISPLAY:-}} ]]; then\n  platform=(--ozone-platform=wayland)\nfi\n\
                 exec {} \"${{platform[@]}}\" \"${{flags[@]}}\" \"$@\"\n",
                binary.display()
            ),
        );
        assert!(runs_chromium(&wrapper, &[]));
        std::fs::remove_file(install.join("chrome_100_percent.pak")).unwrap();
        assert!(!runs_chromium(&wrapper, &[]));
    }

    /// The shape of /usr/bin/discord: the binary's path is built from
    /// variables, one of them defaulted from the environment.
    #[test]
    fn a_wrapper_whose_exec_target_is_built_from_variables_is_followed() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let app = home.join(".config/discord/app-1.0.169");
        let binary = chromium_install(&app, "Discord");
        std::os::unix::fs::symlink(&binary, home.join(".config/discord/Discord")).unwrap();
        let wrapper = root.path().join("bin/discord");
        write_exec(
            &wrapper,
            "#!/bin/sh\n\nCHANNEL=stable\nDIR=discord\nEXE=Discord\n\n\
             config_home=$XDG_CONFIG_HOME\nif [ -z \"$config_home\" ]; then\n    \
             config_home=$HOME/.config\nfi\n\ndiscord_host=$config_home/$DIR/$EXE\n\n\
             if [ ! -x \"$discord_host\" ]; then\n    \
             app_dir=`\"$bootstrap\" $zenity \"$config_home/$DIR\" $CHANNEL`\n    \
             exec \"$config_home/$DIR/$app_dir/$EXE\" \"$@\"\nfi\n\
             exec \"$discord_host\" \"$@\"\n",
        );
        let home = home.to_str().unwrap();
        assert!(runs_chromium(&wrapper, &[("HOME", home)]));
        let config = format!("{home}/.config");
        assert!(runs_chromium(
            &wrapper,
            &[("HOME", home), ("XDG_CONFIG_HOME", config.as_str())]
        ));
        // Not installed for this user: nothing it can exec is there.
        assert!(!runs_chromium(&wrapper, &[("HOME", "/nonexistent")]));
    }

    #[test]
    fn a_wrapper_that_may_run_something_else_is_not_chromium() {
        let root = tempfile::tempdir().unwrap();
        let chromium = chromium_install(&root.path().join("chromium"), "chromium");
        let gecko = root.path().join("firefox/firefox");
        write_exec(&gecko, "\x7fELF");
        std::fs::write(root.path().join("firefox/libxul.so"), b"elf").unwrap();
        let firefox = root.path().join("bin/firefox");
        write_exec(
            &firefox,
            &format!("#!/bin/sh\nexec {} \"$@\"\n", gecko.display()),
        );
        assert!(!runs_chromium(&firefox, &[]));
        let either = root.path().join("bin/web");
        write_exec(
            &either,
            &format!(
                "#!/bin/sh\nif [ -n \"$A\" ]; then exec {} \"$@\"; else exec {} \"$@\"; fi\n",
                chromium.display(),
                gecko.display()
            ),
        );
        assert!(!runs_chromium(&either, &[]));
        // Exec targets nobody can know: a command's output, an argument.
        let unknown = root.path().join("bin/unknown");
        write_exec(
            &unknown,
            "#!/bin/sh\nexec \"$(cat /etc/app)\" \"$@\"\nexec \"$1\"\n",
        );
        assert!(!runs_chromium(&unknown, &[]));
    }

    #[test]
    fn wrapper_forms_of_exec_are_followed() {
        let root = tempfile::tempdir().unwrap();
        let chromium = chromium_install(&root.path().join("opt/app"), "app");
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(&chromium, bin.join("app")).unwrap();
        let path = format!("/nonexistent:{}", bin.display());
        for body in [
            // A redirection-only exec, then the program through env.
            "exec >/dev/null 2>&1\nexec env APP_MODE=1 app --flag \"$@\"".to_owned(),
            // exec -a, a command name found on PATH.
            "cd /tmp && exec -a \"$0\" app \"$@\"".to_owned(),
            // ${NAME:-default} with NAME unset.
            format!("exec \"${{APP_BIN:-{}}}\" \"$@\"", chromium.display()),
            // A relative path from the script's own folder.
            "exec ../opt/app/app \"$@\"".to_owned(),
        ] {
            let wrapper = bin.join("wrapper");
            write_exec(&wrapper, &format!("#!/usr/bin/env bash\n{body}\n"));
            assert!(
                runs_chromium(&wrapper, &[("PATH", path.as_str())]),
                "{body}"
            );
        }
    }

    #[test]
    fn only_short_shell_scripts_are_read() {
        let root = tempfile::tempdir().unwrap();
        let chromium = chromium_install(&root.path().join("chromium"), "chromium-bin");
        let python = root.path().join("bin/python-app");
        write_exec(
            &python,
            &format!(
                "#!/usr/bin/python3\nimport os\nos.execv('{0}', ['{0}'])\nexec {0}\n",
                chromium.display()
            ),
        );
        assert!(!runs_chromium(&python, &[]));
        let long = root.path().join("bin/long");
        let filler = "# filler\n".repeat(WRAPPER_MAX_BYTES as usize / 9 + 1);
        write_exec(
            &long,
            &format!("#!/bin/sh\n{filler}exec {} \"$@\"\n", chromium.display()),
        );
        assert!(!runs_chromium(&long, &[]));
        let short = root.path().join("bin/short");
        write_exec(
            &short,
            &format!("#!/bin/sh\nexec {} \"$@\"\n", chromium.display()),
        );
        assert!(runs_chromium(&short, &[]));
    }

    #[test]
    fn a_script_that_execs_itself_stops() {
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("loop");
        write_exec(
            &script,
            &format!("#!/bin/sh\nexec {} \"$@\"\n", script.display()),
        );
        assert!(!runs_chromium(&script, &[]));
    }

    #[test]
    fn this_test_process_is_not_gecko() {
        assert!(!is_gecko_process(std::process::id()));
        assert_eq!(
            process_executable(std::process::id()),
            std::env::current_exe().ok()
        );
    }
}
