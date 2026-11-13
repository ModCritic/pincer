//! Approval-gated downloads for exact browser tab and element capabilities.
//!
//! Chromium's download behavior is browser-wide, so this tool enables it only
//! for the bounded interval around one exact ref mutation and restores the
//! default before returning.
//!
//! Chromium saves the file as `<destination_root>/<download guid>`
//! (`allowAndName`), which lets the tool prove the exact file it waited for is
//! a regular file directly inside the approved directory. Once that proof
//! holds, the file is renamed to the page's suggested filename, sanitized
//! here and never replacing an existing file, and the result returns that
//! bare name so the caller can find it. Output still omits the source URL
//! (download URLs often carry access tokens) and every local path: the
//! caller already holds `destination_root`, and session recordings redact it
//! from the arguments, so the result text names no path or file either.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::tool::{Tool, ToolDef};
use crate::tool_args::ArgsExt;

use super::cdp_ws::CdpConnection;
use super::engine::BrowserEngine;
use super::refusal::{BrowserRefusal, BrowserRefusalCode};
use super::required_session_schema;
use super::store::BrowserActionKind;

/// Injected by an MCP host only after its destructive-tool approval flow.
/// It is intentionally absent from the public input schema.
pub const MCP_HOST_DOWNLOAD_APPROVAL_ARG: &str = "_pincer_browser_download_mcp_host_approved";

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);

fn browser_download_gate() -> &'static tokio::sync::Mutex<()> {
    static GATE: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    GATE.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub struct BrowserDownloadTool {
    def: ToolDef,
    engine: Arc<BrowserEngine>,
}

impl BrowserDownloadTool {
    pub fn new(engine: Arc<BrowserEngine>) -> Self {
        Self {
            def: ToolDef {
                name: "browser_download".into(),
                description: "Click one live page ref that starts a download (a ref with the click action, such as a link) and save the file in destination_root. The file keeps the page's suggested filename with path separators, control characters and characters Windows forbids replaced by _ and leading dots removed; if that name is taken it gets a numbered suffix (report (1).txt), so no file is overwritten. The result carries file_name (the file is destination_root/file_name), bytes and download_id, never the source URL or a local path. Waits up to 30 s for the download to finish and deletes a partial file when it fails. The tool is marked destructive, so the MCP client may ask the user before each call; calls from outside an MCP client are refused with browser_consent_required.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "session": required_session_schema(),
                        "target_id": {
                            "type": "string",
                            "description": "Opaque exact browser target id from get_browser_state."
                        },
                        "tab_id": {
                            "type": "string",
                            "description": "Opaque exact tab id from get_browser_state."
                        },
                        "ref": {
                            "type": "string",
                            "description": "Live page ref whose activation initiates the download."
                        },
                        "destination_root": {
                            "type": "string",
                            "description": "Existing directory that receives the file, as an absolute canonical path (no symlink or .. anywhere in it)."
                        }
                    },
                    "required": ["session", "target_id", "tab_id", "ref", "destination_root"],
                    "additionalProperties": false
                }),
                read_only: false,
                destructive: true,
                idempotent: false,
                open_world: true,
            },
            engine,
        }
    }
}

fn consent_required() -> ToolResult {
    BrowserRefusal::new(
        BrowserRefusalCode::BrowserConsentRequired,
        "browser_download requires approval through the MCP host's destructive-tool confirmation flow",
    )
    .to_tool_result()
}

fn explicit_session(args: &Value) -> Result<String, ToolResult> {
    let session = args.require_str("session")?;
    if session.trim().is_empty() || session == "default" {
        return Err(ToolResult::error(
            "browser_download requires an explicit non-default session",
        ));
    }
    Ok(session)
}

fn validate_destination_root(raw: &str) -> Result<PathBuf, ToolResult> {
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(ToolResult::error("destination_root must be absolute"));
    }

    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| ToolResult::error("destination_root must be an existing directory"))?;
    if metadata.file_type().is_symlink() {
        return Err(ToolResult::error(
            "destination_root must name the directory directly, not a symlink",
        ));
    }
    if !metadata.is_dir() {
        return Err(ToolResult::error(
            "destination_root must be an existing directory",
        ));
    }

    let canonical = std::fs::canonicalize(path)
        .map_err(|_| ToolResult::error("destination_root could not be canonicalized"))?;
    if canonical != path {
        return Err(ToolResult::error(
            "destination_root must already be in canonical form",
        ));
    }
    Ok(canonical)
}

fn download_path_for_cdp(path: &Path) -> String {
    let raw = path.to_string_lossy();
    #[cfg(target_os = "windows")]
    {
        // `canonicalize` deliberately gives containment checks a verbatim
        // Windows path. Chromium's download API does not accept that transport
        // spelling, so remove only the namespace prefix after validation.
        if let Some(unc) = raw.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{unc}");
        }
        if let Some(local) = raw.strip_prefix(r"\\?\") {
            return local.to_owned();
        }
    }
    raw.into_owned()
}

fn main_frame_id(frame_tree: &Value) -> Option<&str> {
    frame_tree
        .get("frameTree")?
        .get("frame")?
        .get("id")?
        .as_str()
}

fn is_safe_guid(guid: &str) -> bool {
    let mut components = Path::new(guid).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

fn completed_download_size(root: &Path, guid: &str) -> Result<u64, BrowserRefusal> {
    if !is_safe_guid(guid) {
        return Err(BrowserRefusal::new(
            BrowserRefusalCode::BrowserActionUnavailable,
            "the browser returned an invalid opaque download id",
        ));
    }
    let file = root.join(guid);
    let metadata = std::fs::symlink_metadata(&file).map_err(|_| {
        BrowserRefusal::new(
            BrowserRefusalCode::BrowserActionUnavailable,
            "the completed download could not be proven inside the approved directory",
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(BrowserRefusal::new(
            BrowserRefusalCode::BrowserActionUnavailable,
            "the completed download is not a direct regular file",
        ));
    }
    let canonical = std::fs::canonicalize(&file).map_err(|_| {
        BrowserRefusal::new(
            BrowserRefusalCode::BrowserActionUnavailable,
            "the completed download could not be canonicalized",
        )
    })?;
    if canonical.parent() != Some(root) {
        return Err(BrowserRefusal::new(
            BrowserRefusalCode::BrowserActionUnavailable,
            "the completed download escaped the approved directory",
        ));
    }
    Ok(metadata.len())
}

/// Longest saved filename in UTF-8 bytes. Linux and macOS allow 255; this
/// leaves room for a " (99)" suffix.
const MAX_FILE_NAME_BYTES: usize = 200;
/// Highest numbered suffix tried before the download keeps its id name.
const MAX_NAME_SUFFIX: u32 = 99;
/// Used when the page suggests no usable name.
const FALLBACK_FILE_NAME: &str = "download";

/// The page's suggested filename made safe to create in `destination_root`,
/// or `None` when nothing usable is left. The page controls this string, so:
/// only its last path component is kept; control, bidirectional-override and
/// zero-width characters and the characters Windows forbids in names
/// (`<>:"/\|?*`) become `_`; leading and trailing dots and spaces are trimmed
/// (a leading dot would make a hidden file such as `.bashrc` when the
/// destination is a home directory; Windows drops trailing ones); a Windows
/// device name such as `CON` or `nul.txt` gets a `_` prefix; and the name is
/// cut to [`MAX_FILE_NAME_BYTES`], keeping a short extension. The browser
/// computes `suggestedFilename` itself (Chromium 156 reported `report.txt`
/// for `<a download="report.txt">`), but this does not count on the browser
/// having made it safe.
fn safe_download_file_name(suggested: &str) -> Option<String> {
    let last = suggested.rsplit(['/', '\\']).next().unwrap_or_default();
    let replaced = last
        .chars()
        .map(|ch| {
            let unsafe_char = ch.is_control()
                || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
                || matches!(ch, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}')
                || matches!(ch, '\u{2066}'..='\u{2069}' | '\u{feff}');
            if unsafe_char {
                '_'
            } else {
                ch
            }
        })
        .collect::<String>();
    let trimmed = replaced.trim_matches(|ch: char| ch == '.' || ch.is_whitespace());
    if trimmed.is_empty() || trimmed.chars().all(|ch| ch == '_') {
        return None;
    }
    let mut name = trimmed.to_owned();
    let device_stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let is_device = matches!(device_stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((device_stem.starts_with("COM") || device_stem.starts_with("LPT"))
            && device_stem.len() == 4
            && device_stem.as_bytes()[3].is_ascii_digit());
    if is_device {
        name.insert(0, '_');
    }
    if name.len() > MAX_FILE_NAME_BYTES {
        let (stem, extension) = match split_extension(&name) {
            (stem, extension) if extension.len() <= 20 => (stem, extension),
            _ => (name.as_str(), ""),
        };
        // `stem` is longer than the budget here, so the cut is inside it.
        let mut stem_budget = MAX_FILE_NAME_BYTES - extension.len();
        while !stem.is_char_boundary(stem_budget) {
            stem_budget -= 1;
        }
        name = format!("{}{extension}", stem[..stem_budget].trim_end());
    }
    Some(name)
}

/// `("report", ".txt")` for `report.txt`; no extension for `README` or a
/// name whose only dot is the first character.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    }
}

fn numbered_name(name: &str, number: u32) -> String {
    let (stem, extension) = split_extension(name);
    format!("{stem} ({number}){extension}")
}

/// Rename the proven `<root>/<guid>` file to the sanitized suggested name,
/// adding " (N)" until the name is free, and return the name it ended up
/// with. A name is claimed by creating it with `create_new` (fails if
/// anything, even a dangling symlink, already has that name), then the
/// download is renamed over that empty placeholder, so an existing file is
/// never replaced. If no name can be claimed or the rename fails, the file
/// keeps its id name, which is still a correct answer.
fn give_download_its_name(root: &Path, guid: &str, suggested: Option<&str>) -> String {
    let source = root.join(guid);
    let base = suggested
        .and_then(safe_download_file_name)
        .unwrap_or_else(|| FALLBACK_FILE_NAME.to_owned());
    if base == guid {
        return base;
    }
    let candidates = std::iter::once(base.clone())
        .chain((1..=MAX_NAME_SUFFIX).map(|number| numbered_name(&base, number)));
    for candidate in candidates {
        let target = root.join(&candidate);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(placeholder) => {
                drop(placeholder);
                if std::fs::rename(&source, &target).is_ok() {
                    return candidate;
                }
                let _ = std::fs::remove_file(&target);
                return guid.to_owned();
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return guid.to_owned(),
        }
    }
    guid.to_owned()
}

async fn reset_download_behavior(conn: &CdpConnection) {
    let _ = conn
        .call(
            None,
            "Browser.setDownloadBehavior",
            json!({ "behavior": "default", "eventsEnabled": false }),
        )
        .await;
}

enum DownloadEventOutcome {
    Completed { guid: String },
    Canceled { guid: String },
    EventStreamClosed,
}

async fn await_download(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<super::cdp_ws::CdpEvent>,
    expected_frame_id: &str,
    observed_guid: &mut Option<String>,
    suggested_filename: &mut Option<String>,
) -> DownloadEventOutcome {
    while let Some(event) = events.recv().await {
        match event.method.as_str() {
            "Browser.downloadWillBegin" if observed_guid.is_none() => {
                if event.params.get("frameId").and_then(Value::as_str) != Some(expected_frame_id) {
                    continue;
                }
                *observed_guid = event
                    .params
                    .get("guid")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                *suggested_filename = event
                    .params
                    .get("suggestedFilename")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            "Browser.downloadProgress" => {
                let Some(guid) = observed_guid.as_deref() else {
                    continue;
                };
                if event.params.get("guid").and_then(Value::as_str) != Some(guid) {
                    continue;
                }
                match event.params.get("state").and_then(Value::as_str) {
                    Some("completed") => {
                        return DownloadEventOutcome::Completed {
                            guid: guid.to_owned(),
                        }
                    }
                    Some("canceled") => {
                        return DownloadEventOutcome::Canceled {
                            guid: guid.to_owned(),
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    DownloadEventOutcome::EventStreamClosed
}

fn remove_proven_partial(root: &Path, guid: Option<&str>) {
    let Some(guid) = guid.filter(|guid| is_safe_guid(guid)) else {
        return;
    };
    let path = root.join(guid);
    let Ok(metadata) = std::fs::symlink_metadata(&path) else {
        return;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return;
    }
    let Ok(canonical) = std::fs::canonicalize(&path) else {
        return;
    };
    if canonical.parent() == Some(root) {
        let _ = std::fs::remove_file(canonical);
    }
}

#[async_trait]
impl Tool for BrowserDownloadTool {
    fn def(&self) -> &ToolDef {
        &self.def
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let (session, target_id, tab_id, ext_ref, destination_raw) = match (
            explicit_session(&args),
            args.require_str("target_id"),
            args.require_str("tab_id"),
            args.require_str("ref"),
            args.require_str("destination_root"),
        ) {
            (Ok(session), Ok(target), Ok(tab), Ok(ext_ref), Ok(destination)) => {
                (session, target, tab, ext_ref, destination)
            }
            (Err(error), _, _, _, _)
            | (_, Err(error), _, _, _)
            | (_, _, Err(error), _, _)
            | (_, _, _, Err(error), _)
            | (_, _, _, _, Err(error)) => return error,
        };

        if args
            .get(MCP_HOST_DOWNLOAD_APPROVAL_ARG)
            .and_then(Value::as_bool)
            != Some(true)
        {
            return consent_required();
        }
        let destination_root = match validate_destination_root(&destination_raw) {
            Ok(root) => root,
            Err(error) => return error,
        };

        let _mutation = match self
            .engine
            .lock_mutation(&session, &target_id, &tab_id)
            .await
        {
            Ok(guard) => guard,
            Err(refusal) => return refusal.to_tool_result(),
        };
        // Download routing is browser-wide rather than tab-scoped. Serialize
        // these bounded mutations process-wide so two exact tabs cannot race
        // to replace each other's approved destination.
        let _download_mutation = browser_download_gate().lock().await;
        let validated = match self
            .engine
            .revalidate_for_mutation(&session, &target_id, Some(&tab_id))
            .await
        {
            Ok(validated) => validated,
            Err(refusal) => return refusal.to_tool_result(),
        };
        let entry = match self
            .engine
            .store
            .resolve_ref(&session, &target_id, &tab_id, &ext_ref)
        {
            Ok(entry) => entry,
            Err(refusal) => return refusal.to_tool_result(),
        };
        if entry.semantic && !entry.actions.contains(&BrowserActionKind::Click) {
            return BrowserRefusal::new(
                BrowserRefusalCode::BrowserActionUnavailable,
                "the semantic ref does not declare an activation action",
            )
            .to_tool_result();
        }
        let ref_session = match self
            .engine
            .frame_session_for_mutation(&session, &target_id, &tab_id, &validated, &entry.frame)
            .await
        {
            Ok(session) => session,
            Err(refusal) => return refusal.to_tool_result(),
        };

        let frame_tree = match validated
            .conn
            .call(Some(&validated.cdp_session), "Page.getFrameTree", json!({}))
            .await
        {
            Ok(tree) => tree,
            Err(_) => {
                return BrowserRefusal::new(
                    BrowserRefusalCode::BrowserRouteUnavailable,
                    "the exact tab's main frame could not be proven",
                )
                .to_tool_result()
            }
        };
        let Some(main_frame_id) = main_frame_id(&frame_tree).map(str::to_owned) else {
            return BrowserRefusal::new(
                BrowserRefusalCode::BrowserRouteUnavailable,
                "Page.getFrameTree did not identify the exact tab's main frame",
            )
            .to_tool_result();
        };
        let expected_frame_id = entry
            .frame
            .identity
            .as_ref()
            .map(|identity| identity.frame_id.clone())
            .unwrap_or(main_frame_id);

        let resolved = match validated
            .conn
            .call(
                Some(&ref_session),
                "DOM.resolveNode",
                json!({ "backendNodeId": entry.backend_node_id }),
            )
            .await
        {
            Ok(resolved) => resolved,
            Err(_) => {
                return BrowserRefusal::new(
                    BrowserRefusalCode::BrowserRefStale,
                    "the download ref no longer resolves in the live page",
                )
                .to_tool_result()
            }
        };
        let Some(object_id) = resolved
            .get("object")
            .and_then(|object| object.get("objectId"))
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return BrowserRefusal::new(
                BrowserRefusalCode::BrowserRefStale,
                "the download ref has no live page object",
            )
            .to_tool_result();
        };

        // Browser events are endpoint-wide. Subscribe before enabling and
        // triggering so no matching event can be lost between command replies.
        let mut events = validated.conn.subscribe();
        if validated
            .conn
            .call(
                None,
                "Browser.setDownloadBehavior",
                json!({
                    "behavior": "allowAndName",
                    "downloadPath": download_path_for_cdp(&destination_root),
                    "eventsEnabled": true
                }),
            )
            .await
            .is_err()
        {
            reset_download_behavior(&validated.conn).await;
            return BrowserRefusal::new(
                BrowserRefusalCode::BrowserActionUnavailable,
                "the browser could not configure the approved download",
            )
            .to_tool_result();
        }

        let trigger = validated
            .conn
            .call(
                Some(&ref_session),
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": "function() { this.click(); }",
                    "userGesture": true,
                    "awaitPromise": false
                }),
            )
            .await;
        let trigger_failed = trigger
            .as_ref()
            .map_or(true, |result| result.get("exceptionDetails").is_some());
        if trigger_failed {
            reset_download_behavior(&validated.conn).await;
            return BrowserRefusal::new(
                BrowserRefusalCode::BrowserRefStale,
                "the exact download ref could not be activated",
            )
            .to_tool_result();
        }

        let mut observed_guid = None;
        let mut suggested_filename = None;
        let outcome = tokio::time::timeout(
            DOWNLOAD_TIMEOUT,
            await_download(
                &mut events,
                &expected_frame_id,
                &mut observed_guid,
                &mut suggested_filename,
            ),
        )
        .await;
        reset_download_behavior(&validated.conn).await;

        match outcome {
            Ok(DownloadEventOutcome::Completed { guid }) => {
                match completed_download_size(&destination_root, &guid) {
                    Ok(bytes) => {
                        let file_name = give_download_its_name(
                            &destination_root,
                            &guid,
                            suggested_filename.as_deref(),
                        );
                        // The text stays free of the name: recordings keep
                        // it as the result summary (see the module comment).
                        ToolResult::text("browser download completed").with_structured(json!({
                            "status": "completed",
                            "download_id": guid,
                            "bytes": bytes,
                            "file_name": file_name,
                        }))
                    }
                    Err(refusal) => refusal.to_tool_result(),
                }
            }
            Ok(DownloadEventOutcome::Canceled { guid }) => {
                remove_proven_partial(&destination_root, Some(&guid));
                BrowserRefusal::new(
                    BrowserRefusalCode::BrowserActionUnavailable,
                    "the approved browser download was canceled",
                )
                .to_tool_result()
            }
            Ok(DownloadEventOutcome::EventStreamClosed) => {
                remove_proven_partial(&destination_root, observed_guid.as_deref());
                BrowserRefusal::new(
                    BrowserRefusalCode::BrowserActionUnavailable,
                    "the browser connection closed before the approved download completed",
                )
                .to_tool_result()
            }
            Err(_) => {
                remove_proven_partial(&destination_root, observed_guid.as_deref());
                BrowserRefusal::new(
                    BrowserRefusalCode::BrowserActionUnavailable,
                    "the approved browser download did not complete within 30 seconds",
                )
                .to_tool_result()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_must_be_absolute_existing_canonical_directory() {
        assert!(validate_destination_root("relative").is_err());
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("does-not-exist");
        assert!(validate_destination_root(missing.to_str().unwrap()).is_err());

        let root = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        assert_eq!(
            validate_destination_root(canonical.to_str().unwrap()).unwrap(),
            canonical
        );
    }

    #[test]
    fn destination_rejects_regular_files() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("not-a-directory");
        std::fs::write(&file, b"x").unwrap();
        assert!(validate_destination_root(file.to_str().unwrap()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn destination_rejects_symlink_root() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let real = parent.path().join("real");
        let link = parent.path().join("link");
        std::fs::create_dir(&real).unwrap();
        symlink(&real, &link).unwrap();
        assert!(validate_destination_root(link.to_str().unwrap()).is_err());
    }

    #[test]
    fn completion_proof_accepts_only_direct_regular_guid_file() {
        let root = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        std::fs::write(canonical.join("opaque-guid"), b"abc").unwrap();
        assert_eq!(
            completed_download_size(&canonical, "opaque-guid").unwrap(),
            3
        );
        assert!(completed_download_size(&canonical, "../escape").is_err());
        assert!(completed_download_size(&canonical, "missing").is_err());
    }

    #[test]
    fn partial_cleanup_is_confined_to_a_direct_guid_file() {
        let root = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        let partial = canonical.join("opaque-guid");
        std::fs::write(&partial, b"partial").unwrap();

        remove_proven_partial(&canonical, Some("../escape"));
        assert!(partial.exists());

        remove_proven_partial(&canonical, Some("opaque-guid"));
        assert!(!partial.exists());
    }

    #[test]
    fn suggested_names_are_made_safe() {
        let safe = |name: &str| safe_download_file_name(name);
        assert_eq!(safe("report.txt").as_deref(), Some("report.txt"));
        assert_eq!(safe("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(safe(r"..\..\boot.ini").as_deref(), Some("boot.ini"));
        assert_eq!(safe(".bashrc").as_deref(), Some("bashrc"));
        assert_eq!(safe(" notes.txt. ").as_deref(), Some("notes.txt"));
        assert_eq!(
            safe("a<b>c:d\"e|f?g*h.txt").as_deref(),
            Some("a_b_c_d_e_f_g_h.txt")
        );
        assert_eq!(
            safe("line\nbreak\u{0}.txt").as_deref(),
            Some("line_break_.txt")
        );
        // A right-to-left override can disguise "exe" as "txt".
        assert_eq!(
            safe("invoice\u{202e}txt.exe").as_deref(),
            Some("invoice_txt.exe")
        );
        assert_eq!(safe("CON").as_deref(), Some("_CON"));
        assert_eq!(safe("nul.txt").as_deref(), Some("_nul.txt"));
        assert_eq!(safe("com1.log").as_deref(), Some("_com1.log"));
        assert_eq!(safe("console.log").as_deref(), Some("console.log"));
        assert_eq!(safe(""), None);
        assert_eq!(safe("..."), None);
        assert_eq!(safe("dir/"), None);
        assert_eq!(safe("???"), None);

        let long = format!("{}.pdf", "é".repeat(150));
        let cut = safe(&long).unwrap();
        assert!(cut.len() <= MAX_FILE_NAME_BYTES, "{}", cut.len());
        assert!(cut.ends_with(".pdf"));
        let long_extension = format!("a.{}", "b".repeat(300));
        let cut = safe(&long_extension).unwrap();
        assert_eq!(cut.len(), MAX_FILE_NAME_BYTES);
        assert!(cut.starts_with("a.bbb"));
    }

    #[test]
    fn numbered_names_go_before_the_extension() {
        assert_eq!(numbered_name("report.txt", 1), "report (1).txt");
        assert_eq!(numbered_name("README", 2), "README (2)");
        assert_eq!(numbered_name("archive.tar.gz", 3), "archive.tar (3).gz");
    }

    #[test]
    fn completed_download_takes_the_suggested_name_without_overwriting() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        std::fs::write(root.join("report.txt"), b"older").unwrap();
        std::fs::write(root.join("report (1).txt"), b"older too").unwrap();
        std::fs::write(root.join("guid-a"), b"new report").unwrap();

        let name = give_download_its_name(&root, "guid-a", Some("report.txt"));
        assert_eq!(name, "report (2).txt");
        assert_eq!(std::fs::read(root.join("report.txt")).unwrap(), b"older");
        assert_eq!(
            std::fs::read(root.join("report (1).txt")).unwrap(),
            b"older too"
        );
        assert_eq!(std::fs::read(root.join(&name)).unwrap(), b"new report");
        assert!(!root.join("guid-a").exists());

        std::fs::write(root.join("guid-b"), b"x").unwrap();
        assert_eq!(give_download_its_name(&root, "guid-b", None), "download");
        std::fs::write(root.join("guid-c"), b"y").unwrap();
        assert_eq!(
            give_download_its_name(&root, "guid-c", Some("../.profile")),
            "profile"
        );
        assert!(!root.join(".profile").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_name_is_not_followed() {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let escape = outside.path().join("written-through-link");
        symlink(&escape, root.join("report.txt")).unwrap();
        std::fs::write(root.join("guid"), b"data").unwrap();

        let name = give_download_its_name(&root, "guid", Some("report.txt"));
        assert_eq!(name, "report (1).txt");
        assert!(!escape.exists());
        assert!(std::fs::symlink_metadata(root.join("report.txt"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn cdp_download_path_preserves_ordinary_paths() {
        let path = Path::new("/tmp/approved-downloads");
        assert_eq!(download_path_for_cdp(path), path.to_string_lossy());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn cdp_download_path_removes_only_windows_verbatim_prefixes() {
        assert_eq!(
            download_path_for_cdp(Path::new(r"\\?\C:\approved\downloads")),
            r"C:\approved\downloads"
        );
        assert_eq!(
            download_path_for_cdp(Path::new(r"\\?\UNC\server\share\downloads")),
            r"\\server\share\downloads"
        );
    }
}
