use std::{
    path::Path,
    sync::{Mutex, OnceLock},
};

use clipboard_rs::{common::RustImage, Clipboard, ClipboardContext, ContentFormat};
use pincer_core::clipboard::ClipboardBackend;
use x11rb::protocol::xproto::ConnectionExt as _;

pub struct LinuxClipboard {
    context: OnceLock<Result<Mutex<ClipboardContext>, String>>,
}

impl LinuxClipboard {
    pub fn new() -> Self {
        Self {
            context: OnceLock::new(),
        }
    }

    fn context(&self) -> Result<std::sync::MutexGuard<'_, ClipboardContext>, String> {
        self.context
            .get_or_init(|| {
                ClipboardContext::new()
                    .map(Mutex::new)
                    .map_err(|error| error.to_string())
            })
            .as_ref()
            .map_err(Clone::clone)?
            .lock()
            .map_err(|_| "clipboard lock was poisoned".to_owned())
    }
}

fn absolute_existing_file(path: &str) -> Result<String, String> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err("clipboard file paths must be absolute".into());
    }
    let canonical = path.canonicalize().map_err(|error| error.to_string())?;
    if !canonical.is_file() {
        return Err("clipboard path must identify an existing file".into());
    }
    Ok(canonical.to_string_lossy().into_owned())
}

/// Whether nothing owns the X11 `CLIPBOARD` selection (the last owner exited
/// and no clipboard manager took it over). `None` for a Wayland context or
/// when the server cannot be asked.
///
/// clipboard-rs 0.3.5 reads by ConvertSelection and then GetProperty on the
/// property the SelectionNotify names (src/platform/x11.rs,
/// `process_event`). With no owner the server answers with property None,
/// and GetProperty on None fails with BadAtom, so an empty clipboard read
/// as "clipboard unavailable" with a raw X11 error.
fn x11_clipboard_unowned(context: &ClipboardContext) -> Option<bool> {
    if !matches!(context, ClipboardContext::X11(_)) {
        return None;
    }
    let (conn, _) = crate::x11::query_connection().ok()?;
    // Only if it exists: a server where nobody ever used CLIPBOARD lacks it.
    let atom = conn
        .intern_atom(true, b"CLIPBOARD")
        .ok()?
        .reply()
        .ok()?
        .atom;
    if atom == x11rb::NONE {
        return Some(true);
    }
    let owner = conn.get_selection_owner(atom).ok()?.reply().ok()?.owner;
    Some(owner == x11rb::NONE)
}

impl ClipboardBackend for LinuxClipboard {
    /// An unowned X11 clipboard is empty: no formats. Checked again after a
    /// failed read, since the owner can exit between the check and the read.
    fn available_formats(&self) -> Result<Vec<String>, String> {
        let context = self.context()?;
        if x11_clipboard_unowned(&context) == Some(true) {
            return Ok(Vec::new());
        }
        match context.available_formats() {
            Ok(formats) => Ok(formats),
            Err(_) if x11_clipboard_unowned(&context) == Some(true) => Ok(Vec::new()),
            Err(error) => Err(error.to_string()),
        }
    }

    fn read_text(&self) -> Result<Option<String>, String> {
        let context = self.context()?;
        if x11_clipboard_unowned(&context) == Some(true) {
            return Ok(None);
        }
        if context.has(ContentFormat::Text) {
            context.get_text().map(Some).map_err(|e| e.to_string())
        } else {
            Ok(None)
        }
    }

    fn write_text(&self, text: String) -> Result<(), String> {
        self.context()?.set_text(text).map_err(|e| e.to_string())
    }

    fn write_image(&self, absolute_path: &str) -> Result<(), String> {
        let path = absolute_existing_file(absolute_path)?;
        let image = clipboard_rs::RustImageData::from_path(&path).map_err(|e| e.to_string())?;
        self.context()?.set_image(image).map_err(|e| e.to_string())
    }

    fn write_file_url(&self, absolute_path: &str) -> Result<(), String> {
        let path = absolute_existing_file(absolute_path)?;
        self.context()?
            .set_files(vec![path])
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construction_does_not_start_native_clipboard() {
        let backend = LinuxClipboard::new();
        assert!(backend.context.get().is_none());
    }

    #[test]
    fn rejects_relative_local_paths_before_clipboard_access() {
        let backend = LinuxClipboard::new();
        assert!(backend
            .write_file_url("relative.txt")
            .unwrap_err()
            .contains("absolute"));
        assert!(backend
            .write_image("relative.png")
            .unwrap_err()
            .contains("absolute"));
    }

    /// The live clipboard tests below share the display's one clipboard.
    static LIVE_CLIPBOARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Needs an X server; run it under `xvfb-run`.
    #[test]
    #[ignore = "requires an X11 display (run under xvfb-run)"]
    fn native_clipboard_round_trips_text_on_x11() {
        let _clipboard = LIVE_CLIPBOARD.lock().unwrap_or_else(|p| p.into_inner());
        let backend = LinuxClipboard::new();
        backend.write_text("pincer clipboard test".into()).unwrap();
        assert_eq!(
            backend.read_text().unwrap().as_deref(),
            Some("pincer clipboard test")
        );
    }

    /// A clipboard whose owner exited reads as empty instead of failing with
    /// BadAtom, and one with an owner still reads. Needs an X server, like
    /// the round trip above.
    #[test]
    #[ignore = "requires an X11 display (run under xvfb-run)"]
    fn an_unowned_x11_clipboard_reads_as_empty() {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::{CreateWindowAux, WindowClass};
        let _clipboard = LIVE_CLIPBOARD.lock().unwrap_or_else(|p| p.into_inner());

        // Take the selection with a client of our own, then disconnect it:
        // the server drops an exiting client's selections.
        let (owner, screen) = x11rb::rust_connection::RustConnection::connect(None).unwrap();
        let root = owner.setup().roots[screen].root;
        let window = owner.generate_id().unwrap();
        owner
            .create_window(
                0,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new(),
            )
            .unwrap();
        let clipboard = owner
            .intern_atom(false, b"CLIPBOARD")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        owner
            .set_selection_owner(window, clipboard, x11rb::CURRENT_TIME)
            .unwrap();
        owner.get_input_focus().unwrap().reply().unwrap();
        drop(owner);

        let backend = LinuxClipboard::new();
        let mut unowned = None;
        for _ in 0..100 {
            unowned = x11_clipboard_unowned(&backend.context().unwrap());
            if unowned == Some(true) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(unowned, Some(true));
        assert_eq!(backend.available_formats(), Ok(Vec::new()));
        assert_eq!(backend.read_text(), Ok(None));

        backend.write_text("pincer owned text".into()).unwrap();
        assert_eq!(
            x11_clipboard_unowned(&backend.context().unwrap()),
            Some(false)
        );
        assert!(backend
            .available_formats()
            .unwrap()
            .contains(&"UTF8_STRING".to_owned()));
        assert_eq!(
            backend.read_text().unwrap().as_deref(),
            Some("pincer owned text")
        );
    }
}
