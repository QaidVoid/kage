//! Text writes to an input state, held until that state has been laid out.
//!
//! The input engines read `window.text_style()` once, when the state is
//! built, and keep what they read: `DisplayMap::new` stores the family and
//! only the element corrects it, during its own first prepaint. A window
//! with nothing on its text style stack names GPUI's `.SystemUIFont`, and
//! on the web that family resolves through no installed name, so a write
//! that lands before the element's first prepaint asks the text system to
//! resolve it and the resolve panics, which aborts the frame and leaves
//! the window blank.
//!
//! So a write to a state whose element has not prepainted yet waits. The
//! signal is [`LaidOut`], which the element's own prepaint sets through
//! `ElementExt::on_prepaint`. That is the real boundary and nothing else
//! stands in for it: a render count is not one, because a view can render
//! many times before anything prepaints, and a view whose element is not
//! mounted arms on renders that lay nothing out.
//!
//! A second view writing to the same state takes a copy of the owner's
//! [`LaidOut`] rather than a new one, because only the view that renders
//! the input's element can report that element's layout.
//!
//! Writes that can only follow a key press in a field the user is already
//! typing in need none of this, because focusing a field requires its
//! element to be on screen.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// The signal that an input state's element has prepainted once.
///
/// Shareable, so a view that renders the element owns it and every other
/// writer of that state holds a copy of the same signal.
#[derive(Clone, Default)]
pub struct LaidOut(Rc<Cell<bool>>);

impl LaidOut {
    /// A signal nothing has set yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The cell to hand to a prepaint hook, which sets it when the element
    /// lays out.
    #[must_use]
    pub fn flag(&self) -> Rc<Cell<bool>> {
        Rc::clone(&self.0)
    }

    /// Sets the signal, releasing every hold on it.
    pub fn mark(&self) {
        self.0.set(true);
    }

    /// Whether the element has prepainted, so a write would land.
    #[must_use]
    pub fn is_laid_out(&self) -> bool {
        self.0.get()
    }
}

/// One pending text write, released once the target's element pre-paints.
///
/// The pending text sits behind a cell because a hold is written from
/// `&self`: the store observer that feeds it holds the view borrowed, and
/// a `&mut` write there would need the borrow to end first.
#[derive(Default)]
pub struct Deferred {
    text: RefCell<Option<String>>,
    laid_out: LaidOut,
}

impl Deferred {
    /// A deferred write with nothing pending, released by its own new
    /// signal.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A deferred write released by `laid_out`, for a state this view does
    /// not itself lay out.
    #[must_use]
    pub fn after(laid_out: LaidOut) -> Self {
        Self {
            text: RefCell::new(None),
            laid_out,
        }
    }

    /// The signal this write waits on, to hand to a prepaint hook.
    #[must_use]
    pub fn laid_out(&self) -> &LaidOut {
        &self.laid_out
    }

    /// Sets the signal, releasing the pending write.
    pub fn mark_laid_out(&self) {
        self.laid_out.mark();
    }

    /// Whether the target's element has prepainted, so a write would land.
    #[must_use]
    pub fn is_laid_out(&self) -> bool {
        self.laid_out.is_laid_out()
    }

    /// Writes `text` now when the hold has released, and otherwise
    /// remembers it, replacing anything still pending.
    pub fn set(&self, text: impl Into<String>, write: impl FnOnce(&str)) {
        let text = text.into();
        if self.is_laid_out() {
            *self.text.borrow_mut() = None;
            write(&text);
        } else {
            *self.text.borrow_mut() = Some(text);
        }
    }

    /// Writes anything still pending, once the hold has released. Call
    /// from `render`.
    pub fn flush(&self, write: impl FnOnce(&str)) {
        if self.is_laid_out()
            && let Some(text) = self.text.borrow_mut().take()
        {
            write(&text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Deferred;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// The writes a deferred write performed, in order.
    fn recorder() -> (Rc<RefCell<Vec<String>>>, impl Fn(&str)) {
        let log = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&log);
        (log, move |text: &str| {
            sink.borrow_mut().push(text.to_owned())
        })
    }

    #[test]
    fn a_write_before_the_layout_waits() {
        let (log, write) = recorder();
        let deferred = Deferred::new();
        deferred.set("first", &write);
        assert!(log.borrow().is_empty(), "nothing lands before the layout");

        deferred.flush(&write);
        assert!(log.borrow().is_empty(), "nothing lands before the layout");

        deferred.mark_laid_out();
        deferred.flush(&write);
        assert_eq!(*log.borrow(), ["first"]);
    }

    #[test]
    fn the_prepaint_hook_releases_the_hold() {
        let (log, write) = recorder();
        let deferred = Deferred::new();
        deferred.set("hooked", &write);

        deferred.laid_out().flag().set(true);
        assert!(
            deferred.is_laid_out(),
            "the cell the hook carries is the hold"
        );
        deferred.flush(&write);
        assert_eq!(*log.borrow(), ["hooked"]);
    }

    #[test]
    fn a_second_writer_waits_on_the_owner_layout() {
        let (log, write) = recorder();
        let owner = Deferred::new();
        // Another view writing the same state, holding the owner's signal.
        let other = Deferred::after(owner.laid_out().clone());
        other.set("from elsewhere", &write);
        assert!(log.borrow().is_empty(), "the owner's layout governs");

        owner.mark_laid_out();
        other.flush(&write);
        assert_eq!(*log.borrow(), ["from elsewhere"]);
    }

    #[test]
    fn a_write_after_the_layout_lands_immediately() {
        let (log, write) = recorder();
        let deferred = Deferred::new();
        deferred.mark_laid_out();
        deferred.set("now", &write);
        assert_eq!(*log.borrow(), ["now"]);
    }

    #[test]
    fn only_the_last_pending_text_lands() {
        let (log, write) = recorder();
        let deferred = Deferred::new();
        deferred.set("stale", &write);
        deferred.set("fresh", &write);
        deferred.mark_laid_out();
        deferred.flush(&write);
        assert_eq!(
            *log.borrow(),
            ["fresh"],
            "a superseded draft is not written after the layout"
        );
    }

    #[test]
    fn a_flushed_write_does_not_repeat() {
        let (log, write) = recorder();
        let deferred = Deferred::new();
        deferred.set("once", &write);
        deferred.mark_laid_out();
        deferred.flush(&write);
        deferred.flush(&write);
        assert_eq!(*log.borrow(), ["once"]);
    }

    #[test]
    fn an_unmounted_element_holds_forever() {
        let (log, write) = recorder();
        let deferred = Deferred::new();
        deferred.set("held", &write);
        for _ in 0..5 {
            deferred.flush(&write);
        }
        assert!(
            log.borrow().is_empty(),
            "renders alone never release the hold"
        );
    }
}
