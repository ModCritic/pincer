//! Linux tool implementations over the x11/atspi/input/capture backends.

use pincer_core::tool::ToolRegistry;

mod impl_;

pub fn build_registry(compat: bool) -> ToolRegistry {
    build_registry_with_provider(compat, None)
}

pub fn build_registry_with_provider(
    compat: bool,
    provider: Option<std::sync::Arc<dyn pincer_core::consent::ProtectedConsentProvider>>,
) -> ToolRegistry {
    // Visibly targeted pointer actions push cursor-hook events (see
    // `impl_::cursor_reveal::emit_cursor_hook`), so an embedder such as cua-spacesd can
    // publish the agent's cursor as presence without polling.
    pincer_core::cursor_hook::declare_cursor_hook_emitter();
    impl_::build_registry_with_provider(compat, provider)
}
