//! The App's intake for an ACP connection.
//!
//! An ACP connection pumps frames on its own thread: the transcript
//! folds straight into the [`SharedBuffer`] the App draws, and the
//! signals the connection fabricates reach the App through the engine
//! event channel. What that thread cannot apply itself, because it
//! takes `&mut App`, waits in an [`AcpFeed`] the loop drains each
//! pass: plugin commands for the `:` palette, the keymap the snapshot
//! offers, status entries plugins set, and the root buffer of a
//! session the connection moved onto or replaced, which waits for that
//! session's `SessionAdopted` event.

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
            self.adopted_buffer = Some(buffer);
        }
        changed
    }

    /// Show the session the connection's store moved the main view
    /// onto, or the transcript it replaced in place. The store already
    /// holds the blocks, handed over through the feed, so only the
    /// view's session state resets: agent views, focus, search and, on
    /// a switch, the pending prompts and run indicators of the session
    /// left. The composer keeps what is being typed.
    pub(crate) fn session_adopted(&mut self, session: kage_core::SessionId) {
        let switched = self.active_session != Some(session);
        self.set_focus(None);
        self.reset_session_ui();
        self.active_session = Some(session);
        self.status_session_id = Some(session.to_string().chars().take(8).collect());
        if let Some(buffer) = self.adopted_buffer.take() {
            self.root_buffer = buffer;
        }
        self.buffer = Arc::clone(&self.root_buffer);
        self.draw_snapshot = None;
        lock(&self.buffer).invalidate_all_heights();
        self.agents.clear();
        self.agent_buffers.clear();
        self.evicted_agents.clear();
        self.drafts.clear();
        if switched {
            self.pending.clear();
            self.swarm_oneshot = None;
            self.compacting = None;
            self.pending_compaction_before = None;
        }
        self.plugin_sessions_stale = true;
        self.on_session_changed();
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
            !Arc::ptr_eq(&app.root_buffer, &sent_buffer),
            "the buffer waits for its session's adoption"
        );
        app.session_adopted(kage_core::SessionId::new());
        assert!(
            Arc::ptr_eq(&app.root_buffer, &sent_buffer),
            "the app draws the feed's buffer once adopted"
        );
        assert!(!app.drain_acp_feed(), "a second drain takes nothing");
    }

    #[test]
    fn an_empty_feed_changes_nothing() {
        let mut app = App::new(crate::events::shared_buffer(), std::sync::mpsc::channel().0);
        app.set_acp_feed(std::sync::Arc::new(Fixed(std::sync::Mutex::new(Some(
            AcpUpdate::default(),
        )))));
        assert!(!app.drain_acp_feed());
    }

    /// Adopting another session leaves the agent view and the agents
    /// of the session left, shows the handed buffer, and takes the new
    /// session's id for the status line and the session picker.
    #[test]
    fn adopting_a_session_resets_the_view_onto_its_buffer() {
        let fresh = crate::events::shared_buffer();
        let mut app = App::new(crate::events::shared_buffer(), std::sync::mpsc::channel().0);
        let old = kage_core::SessionId::new();
        app.active_session = Some(old);
        app.focus = Some(kage_core::SessionId::new());
        app.pending.push((
            None,
            crate::view::PendingPrompt {
                text: "hi".into(),
                queued: true,
            },
        ));
        app.adopted_buffer = Some(Arc::clone(&fresh));
        let new = kage_core::SessionId::new();
        app.session_adopted(new);
        assert_eq!(app.focus, None);
        assert!(Arc::ptr_eq(&app.buffer, &fresh));
        assert!(Arc::ptr_eq(&app.root_buffer, &fresh));
        assert_eq!(app.active_session, Some(new));
        assert_eq!(
            app.status_session_id.as_deref(),
            Some(&new.to_string()[..8])
        );
        assert!(app.pending.is_empty(), "the old session's pending rows go");
    }

    /// Events the session left published before the switch still land
    /// in its buffer: the new buffer is drawn only once the adoption
    /// arrives, in order.
    #[test]
    fn events_before_the_adoption_stay_with_the_session_left() {
        let old_buffer = crate::events::shared_buffer();
        let fresh = crate::events::shared_buffer();
        let mut app = App::new(Arc::clone(&old_buffer), std::sync::mpsc::channel().0);
        let old = kage_core::SessionId::new();
        app.active_session = Some(old);
        app.adopted_buffer = Some(Arc::clone(&fresh));
        let notice = |session| kage_core::protocol::Envelope {
            session,
            seq: 0,
            event: kage_core::protocol::HostEvent::Notice {
                level: kage_core::protocol::NoticeLevel::Info,
                text: "late".into(),
                transient: false,
            }
            .into(),
        };
        app.apply_envelope(notice(old));
        let new = kage_core::SessionId::new();
        app.apply_envelope(kage_core::protocol::Envelope {
            session: new,
            seq: 0,
            event: kage_core::protocol::HostEvent::SessionAdopted.into(),
        });
        assert_eq!(lock(&old_buffer).blocks().len(), 1);
        assert!(lock(&fresh).blocks().is_empty());
    }
}
