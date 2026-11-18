//! Windows tool implementations.
//!
//! On Windows, delegates to real win32/uia/input/capture implementations.
//! On other platforms, returns "not implemented" stubs so the crate still compiles.

use pincer_core::tool::ToolRegistry;

#[cfg(target_os = "windows")]
mod impl_;

#[cfg(not(target_os = "windows"))]
mod stubs;

pub fn build_registry(compat: bool) -> ToolRegistry {
    build_registry_with_provider(compat, None)
}

pub fn build_registry_with_provider(
    compat: bool,
    provider: Option<std::sync::Arc<dyn pincer_core::consent::ProtectedConsentProvider>>,
) -> ToolRegistry {
    #[cfg(target_os = "windows")]
    return impl_::build_registry_with_provider(compat, provider);

    #[cfg(not(target_os = "windows"))]
    {
        let _ = compat;
        let _ = provider;
        stubs::build_registry()
    }
}

/// The first line of list_windows' text. Kept outside `impl_` so it is
/// tested on every host.
pub(crate) fn list_windows_header(total: usize, apps: usize, on_screen: usize) -> String {
    format!("✅ Found {total} window(s) across {apps} app(s); {on_screen} on-screen.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_windows_header_names_no_macos_api() {
        // The header ended with macOS wording copied from the Swift driver:
        // "(SkyLight Space SPIs unavailable ...)". SkyLight and Spaces are
        // macOS concepts, so on Windows the note said nothing true.
        let header = list_windows_header(5, 3, 4);
        assert_eq!(header, "✅ Found 5 window(s) across 3 app(s); 4 on-screen.");
    }
}
