//! Fixtures shared by this crate's tests.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use kage_core::{Risk, ToolOutput};
use kage_tools::{ExecMode, Tool, ToolContext, ToolError};

/// A barrier with a timeout: each arrival waits until `parties` calls
/// have arrived, or gives up after `timeout`.
#[derive(Debug)]
pub(crate) struct Meet {
    parties: usize,
    timeout: Duration,
    arrived: Mutex<usize>,
    all_here: Condvar,
}

impl Meet {
    pub(crate) fn new(parties: usize, timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            parties,
            timeout,
            arrived: Mutex::new(0),
            all_here: Condvar::new(),
        })
    }

    /// Arrive and wait. Returns whether every party arrived in time.
    fn arrive(&self) -> bool {
        let mut arrived = self.arrived.lock().unwrap();
        *arrived += 1;
        self.all_here.notify_all();
        let (arrived, _) = self
            .all_here
            .wait_timeout_while(arrived, self.timeout, |n| *n < self.parties)
            .unwrap();
        *arrived >= self.parties
    }
}

/// Meets the other calls of its batch at a shared [`Meet`]. Its result is
/// an error when the others never arrived, which is what sequential
/// dispatch produces.
#[derive(Debug)]
pub(crate) struct MeetTool {
    pub(crate) name: &'static str,
    pub(crate) mode: Option<ExecMode>,
    pub(crate) meet: Arc<Meet>,
}

impl Tool for MeetTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &'static str {
        "waits for the other calls of its batch"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execution_mode(&self) -> Option<ExecMode> {
        self.mode
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let met = self.meet.arrive();
        Ok(ToolOutput {
            is_error: !met,
            text: if met { "met" } else { "alone" }.into(),
            structured: None,
            terminate: false,
        })
    }
}
