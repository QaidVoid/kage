//! Overlay registry for built-in and plugin overlays.
//!
//! Mirrors [`crate::view::registry::BlockRenderer`]: built-in overlays
//! are pre-registered by key so a plugin can override them, and
//! plugins add entirely new overlays via [`OverlayRegistry::set_custom`].
//! Both paths are consumed later by PE.B's `kage.ui.custom(name, options)`
//! Lua surface, which looks up `name` in the registry and instantiates
//! the resulting factory with `options`.
//!
//! Built-in factories cover the option-only primitives that PO.4
//! defines: `confirm`, `input`, `editor`. The two stateful built-ins
//! ([`crate::overlay::OverlayPicker`], [`crate::overlay::SlashPalette`])
//! are constructed directly by [`crate::App`] because their inputs
//! (`Vec<PickItem>`, command-spec refs) do not round-trip through JSON;
//! they live outside the registry on purpose.

use std::collections::HashMap;
use std::sync::Arc;

use super::confirm::ConfirmOverlay;
use super::editor::EditorOverlay;
use super::input::InputOverlay;
use super::widget::OverlayWidget;

/// Constructs a fresh overlay from a JSON options payload.
///
/// `options` is the value `ui.custom(name, options)` passed in Lua.
/// Factories pull whatever fields they need (`title`, `message`,
/// `prefill`) and ignore the rest. Returns a boxed
/// [`OverlayWidget`] the host can store and dispatch through the
/// usual modal stack.
pub trait OverlayFactory: Send + Sync {
    /// Build an overlay from `options`. Implementations should treat
    /// missing fields permissively (default to empty strings, etc.)
    /// so a stray Lua call doesn't crash the TUI.
    fn make(&self, options: &serde_json::Value) -> Box<dyn OverlayWidget>;
}

/// Registry mapping overlay keys to factories.
#[derive(Default)]
pub struct OverlayRegistry {
    by_key: HashMap<String, Arc<dyn OverlayFactory>>,
}

impl OverlayRegistry {
    /// Empty registry; plugins build their own with
    /// [`OverlayRegistry::set_custom`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry pre-populated with the bundled factories for
    /// `confirm`, `input`, and `editor`.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut r = Self::default();
        r.by_key
            .insert("confirm".into(), Arc::new(BuiltinConfirmFactory));
        r.by_key
            .insert("input".into(), Arc::new(BuiltinInputFactory));
        r.by_key
            .insert("editor".into(), Arc::new(BuiltinEditorFactory));
        r
    }

    /// Register a factory under `key`. Overrides any existing entry,
    /// so plugins can swap the bundled `confirm` for their own.
    pub fn set(&mut self, key: impl Into<String>, factory: Arc<dyn OverlayFactory>) {
        self.by_key.insert(key.into(), factory);
    }

    /// Convenience alias for [`Self::set`] that documents intent when
    /// a plugin adds a brand-new overlay kind rather than overriding a
    /// built-in. The implementation is identical.
    pub fn set_custom(&mut self, name: impl Into<String>, factory: Arc<dyn OverlayFactory>) {
        self.set(name, factory);
    }

    /// Construct a new overlay for `key`. Returns `None` when nothing
    /// is registered, which lets the host surface "unknown overlay" to
    /// the caller (e.g. a Lua error from `ui.custom`).
    #[must_use]
    pub fn open(&self, key: &str, options: &serde_json::Value) -> Option<Box<dyn OverlayWidget>> {
        self.by_key.get(key).map(|f| f.make(options))
    }

    /// Iterate the registered keys. Useful for diagnostics and for the
    /// future `ui.list_overlays()` introspection API.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.by_key.keys().map(String::as_str)
    }
}

fn str_field<'a>(options: &'a serde_json::Value, key: &str) -> &'a str {
    options
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

/// Built-in factory producing a [`ConfirmOverlay`]. Accepts
/// `{"title": "...", "message": "..."}`.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinConfirmFactory;

impl OverlayFactory for BuiltinConfirmFactory {
    fn make(&self, options: &serde_json::Value) -> Box<dyn OverlayWidget> {
        let title = str_field(options, "title");
        let message = str_field(options, "message");
        Box::new(ConfirmOverlay::new(title, message))
    }
}

/// Built-in factory producing an [`InputOverlay`]. Accepts
/// `{"title": "...", "placeholder": "..."}`. `placeholder` is optional.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinInputFactory;

impl OverlayFactory for BuiltinInputFactory {
    fn make(&self, options: &serde_json::Value) -> Box<dyn OverlayWidget> {
        let title = str_field(options, "title");
        let mut overlay = InputOverlay::new(title);
        if let Some(hint) = options
            .get("placeholder")
            .and_then(serde_json::Value::as_str)
        {
            overlay = overlay.with_placeholder(hint);
        }
        Box::new(overlay)
    }
}

/// Built-in factory producing an [`EditorOverlay`]. Accepts
/// `{"title": "...", "prefill": "..."}`. `prefill` is optional.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinEditorFactory;

impl OverlayFactory for BuiltinEditorFactory {
    fn make(&self, options: &serde_json::Value) -> Box<dyn OverlayWidget> {
        let title = str_field(options, "title");
        let mut overlay = EditorOverlay::new(title);
        if let Some(prefill) = options.get("prefill").and_then(serde_json::Value::as_str) {
            overlay = overlay.with_prefill(prefill);
        }
        Box::new(overlay)
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;

    use super::*;

    #[test]
    fn with_builtins_includes_confirm_input_editor() {
        let r = OverlayRegistry::with_builtins();
        let mut keys: Vec<&str> = r.keys().collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["confirm", "editor", "input"]);
    }

    #[test]
    fn empty_registry_returns_none_for_unknown_key() {
        let r = OverlayRegistry::new();
        let opened = r.open("confirm", &serde_json::json!({}));
        assert!(opened.is_none());
    }

    #[test]
    fn open_confirm_constructs_overlay() {
        let r = OverlayRegistry::with_builtins();
        let mut overlay = r
            .open(
                "confirm",
                &serde_json::json!({ "title": "Delete?", "message": "sure?" }),
            )
            .expect("confirm should be registered");
        let measured = overlay.measure(Rect::new(0, 0, 80, 24));
        assert!(measured.width > 0);
        // Construct succeeds and the overlay accepts keys.
        let _ = overlay.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    }

    #[test]
    fn set_custom_adds_new_overlay() {
        struct CustomFactory;
        impl OverlayFactory for CustomFactory {
            fn make(&self, _options: &serde_json::Value) -> Box<dyn OverlayWidget> {
                Box::new(super::super::widget::EmptyOverlayWidget)
            }
        }

        let mut r = OverlayRegistry::new();
        r.set_custom("my-dialog", Arc::new(CustomFactory));
        assert!(r.open("my-dialog", &serde_json::json!({})).is_some());
    }

    #[test]
    fn set_overrides_a_builtin() {
        struct OverrideFactory;
        impl OverlayFactory for OverrideFactory {
            fn make(&self, _options: &serde_json::Value) -> Box<dyn OverlayWidget> {
                Box::new(super::super::widget::EmptyOverlayWidget)
            }
        }

        let mut r = OverlayRegistry::with_builtins();
        r.set("confirm", Arc::new(OverrideFactory));
        let overlay = r
            .open("confirm", &serde_json::json!({}))
            .expect("confirm still registered");
        // The override returns EmptyOverlayWidget which measures zero.
        let measured = overlay.measure(Rect::new(0, 0, 80, 24));
        assert_eq!(measured, Rect::new(0, 0, 0, 0));
    }

    #[test]
    fn factory_trait_is_send_and_sync_for_arc_storage() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arc<dyn OverlayFactory>>();
    }

    #[test]
    fn input_factory_honors_placeholder() {
        let r = OverlayRegistry::with_builtins();
        // Construct succeeds whether placeholder is present or not.
        assert!(
            r.open("input", &serde_json::json!({ "title": "Name" }))
                .is_some()
        );
        assert!(
            r.open(
                "input",
                &serde_json::json!({ "title": "Name", "placeholder": "type" })
            )
            .is_some()
        );
    }
}
