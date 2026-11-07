//! The App's intake for an ACP connection.
//!
//! An ACP connection pumps frames on its own thread: the transcript
//! folds straight into the [`SharedBuffer`] the App draws, and the
//! signals the connection fabricates reach the App through the engine
//! event channel. What that thread cannot apply itself, because it
//! takes `&mut App`, waits in an [`AcpFeed`] the loop drains each
//! pass: plugin commands for the `:` palette, the keymap the snapshot
//! offers, status entries plugins set, and a fresh root buffer after
//! the session was replaced.

use super::*;

use kage_core::keymap::Keymap;

/// What an [`AcpFeed`] hands over, one drain at a time. Every field is
/// delivered only when it changed, so an empty update is the norm.
#[derive(Default)]
pub struct AcpUpdate {
    /// The plugin commands the attached `_kage/ui` offers, for
    /// [`App::set_plugin_commands`].
    pub commands: Option<Vec<crate::command::PluginCommand>>,
    /// The keymap the attached `_kage/ui` offers, rebuilt. `None` when
    /// the snapshot does not rebuild, which keeps what the App routes
    /// with.
    pub keymap: Option<Keymap>,
    /// The buffer to draw from now on: a replaced session replays over
    /// a clean slate, so the App must adopt the connection's fresh one.
    pub root_buffer: Option<SharedBuffer>,
    /// Status entries plugins set, by key, for the status bar.
    pub status: Option<Vec<(String, String)>>,
}

/// A source of [`AcpUpdate`]s the App drains between event polls.
/// Shared with the connection's pump thread, which fills it; `take`
/// hands over and clears what is waiting.
pub trait AcpFeed: Send + Sync {
    /// Hand over what is waiting, leaving nothing behind.
    fn take(&self) -> AcpUpdate;
}

impl App {
    /// Wire the feed an ACP connection fills. Without one, draining is
    /// a no-op and the App behaves exactly as on the engine path.
    pub fn set_acp_feed(&mut self, feed: std::sync::Arc<dyn AcpFeed>) {
        self.acp_feed = Some(feed);
    }

    /// Take the feed's update and apply it. Returns whether anything
    /// changed on screen.
    pub(crate) fn drain_acp_feed(&mut self) -> bool {
        let Some(feed) = self.acp_feed.as_ref() else {
            return false;
        };
        let update = feed.take();
        let mut changed = false;
        if let Some(commands) = update.commands {
            self.set_plugin_commands(commands);
            changed = true;
        }
        if let Some(keymap) = update.keymap {
            *lock(&self.keymap) = keymap;
            self.sequencer.clear();
            changed = true;
        }
        if let Some(status) = update.status {
            self.plugin_status_cache = status;
            changed = true;
        }
        if let Some(buffer) = update.root_buffer {
            self.set_root_buffer(buffer);
            changed = true;
        }
        changed
    }

    /// Draw `buffer` as the main session's transcript from now on: the
    /// conversation was replaced in place, so the connection handed
    /// over a fresh one its replay folds into.
    pub(crate) fn set_root_buffer(&mut self, buffer: SharedBuffer) {
        self.root_buffer = Arc::clone(&buffer);
        if self.focus.is_none() {
            self.buffer = buffer;
        }
        self.draw_snapshot = None;
        lock(&self.buffer).invalidate_all_heights();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::PluginCommand;

    /// A feed that hands out its update once, the way a real pump
    /// fills and clears it.
    struct Fixed(std::sync::Mutex<Option<AcpUpdate>>);

    impl AcpFeed for Fixed {
        fn take(&self) -> AcpUpdate {
            self.0.lock().unwrap().take().unwrap_or_default()
        }
    }

    /// A one-mapping keymap, rebuilt the way the snapshot's mappings
    /// rebuild.
    fn one_mapping_keymap() -> Keymap {
        let mut keymap = Keymap::new();
        keymap
            .replace_with(&[kage_core::keymap::MappingData {
                mode: "n".to_owned(),
                lhs: "<C-x>g".to_owned(),
                rhs: kage_core::keymap::RhsData::Command {
                    line: ":echo hi".to_owned(),
                },
                desc: Some("say hi".to_owned()),
                group: None,
                owner: "test".to_owned(),
            }])
            .unwrap();
        keymap
    }

    #[test]
    fn a_drain_applies_every_delivered_field() {
        let sent_buffer = crate::events::shared_buffer();
        let mut app = App::new(crate::events::shared_buffer(), std::sync::mpsc::channel().0);
        app.set_acp_feed(std::sync::Arc::new(Fixed(std::sync::Mutex::new(Some(
            AcpUpdate {
                commands: Some(vec![PluginCommand {
                    name: "greet".to_owned(),
                    aliases: Vec::new(),
                    is_override: false,
                    description: "say hi".to_owned(),
                    args: Vec::new(),
                }]),
                keymap: Some(one_mapping_keymap()),
                root_buffer: Some(Arc::clone(&sent_buffer)),
                status: Some(vec![("mood".to_owned(), "fine".to_owned())]),
            },
        )))));
        assert!(app.drain_acp_feed());
        assert!(
            app.plugin_command_specs
                .iter()
                .any(|spec| spec.name == "greet")
        );
        assert_eq!(lock(&app.keymap).to_data().len(), 1);
        assert_eq!(
            app.plugin_status_cache,
            [("mood".to_owned(), "fine".to_owned())]
        );
        assert!(
            Arc::ptr_eq(&app.root_buffer, &sent_buffer),
            "the app draws the feed's buffer"
        );
        // A second drain takes nothing: the feed was emptied.
        assert!(!app.drain_acp_feed());
    }

    #[test]
    fn an_empty_feed_changes_nothing() {
        let mut app = App::new(crate::events::shared_buffer(), std::sync::mpsc::channel().0);
        app.set_acp_feed(std::sync::Arc::new(Fixed(std::sync::Mutex::new(Some(
            AcpUpdate::default(),
        )))));
        assert!(!app.drain_acp_feed());
    }

    #[test]
    fn a_root_buffer_swap_keeps_a_focused_agent_view() {
        let fresh = crate::events::shared_buffer();
        let mut app = App::new(crate::events::shared_buffer(), std::sync::mpsc::channel().0);
        // A focused agent view (no agent present, so just the focus
        // flag) must not be yanked to the new root.
        app.focus = Some(kage_core::SessionId::new());
        app.set_root_buffer(Arc::clone(&fresh));
        assert!(!Arc::ptr_eq(&app.buffer, &fresh));
        assert!(Arc::ptr_eq(&app.root_buffer, &fresh));
    }
}
