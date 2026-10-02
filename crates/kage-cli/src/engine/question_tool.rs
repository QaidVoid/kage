//! The `ask_user_question` tool: the model asks the user one to four
//! questions, each with two to four choices, and waits for the
//! answers. The user may pick a choice, several when the question
//! allows it, answer in their own words, or decline.
//!
//! Only a main session with a client to answer gets the tool. An agent
//! reports to its parent instead, and print mode has nobody to ask.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::select_biased;
use kage_core::protocol::{ASK_USER_QUESTION_TOOL, HostEvent, Question, RequestId};
use kage_core::sync::lock;
use kage_core::{Risk, SessionId, ToolOutput};
use kage_tools::{ExecMode, Tool, ToolContext, ToolError};
use serde::Deserialize;

use super::bus::Bus;

/// The answers to one request: per question, the labels picked or the
/// user's own words. `None` when the user declined.
pub(super) type Answers = Option<Vec<Vec<String>>>;

/// Open questions by request id, with the session that asked and where
/// the answers go.
pub(super) type Questions =
    Arc<Mutex<HashMap<RequestId, (SessionId, crossbeam_channel::Sender<Answers>)>>>;

const DESCRIPTION: &str = "Ask the user one to four questions and wait for the answers. Use it \
when a decision is the user's to make and you cannot settle it from the request, the code or a \
sensible default: a choice between approaches, a preference, or missing facts. Do not use it to \
ask for approval to go on, or for things you can find out yourself. Each question offers two to \
four choices with a short label and a description of what picking it means; put a recommended \
choice first and add `(Recommended)` to its label. The user can always answer in their own words \
instead, so do not add an `Other` choice. Set `multi_select` when several choices can apply.";

#[derive(Deserialize)]
struct QuestionInput {
    questions: Vec<Question>,
}

/// Asks one session's user. The dispatcher registers it into the runs
/// of a main session a client answers for.
pub(super) struct QuestionTool {
    session: SessionId,
    bus: Arc<Bus>,
    questions: Questions,
    next_request: Arc<AtomicU64>,
}

impl QuestionTool {
    pub(super) fn new(
        session: SessionId,
        bus: Arc<Bus>,
        questions: Questions,
        next_request: Arc<AtomicU64>,
    ) -> Self {
        Self {
            session,
            bus,
            questions,
            next_request,
        }
    }
}

impl std::fmt::Debug for QuestionTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuestionTool")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl Tool for QuestionTool {
    fn name(&self) -> &str {
        ASK_USER_QUESTION_TOOL
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 4,
                    "items": {
                        "type": "object",
                        "properties": {
                            "header": {
                                "type": "string",
                                "description": "A label of at most 12 characters, such as `Auth method`."
                            },
                            "question": {
                                "type": "string",
                                "description": "The whole question, ending with a question mark."
                            },
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "maxItems": 4,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {
                                            "type": "string",
                                            "description": "The choice in one to five words."
                                        },
                                        "description": {
                                            "type": "string",
                                            "description": "What picking it means, and its trade-offs."
                                        }
                                    },
                                    "required": ["label", "description"]
                                }
                            },
                            "multi_select": {
                                "type": "boolean",
                                "description": "Whether the user may pick several choices."
                            }
                        },
                        "required": ["header", "question", "options"]
                    }
                }
            },
            "required": ["questions"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn execution_mode(&self) -> Option<ExecMode> {
        Some(ExecMode::Sequential)
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: QuestionInput = serde_json::from_value(input)?;
        if let Err(why) = check(&input.questions) {
            return Ok(super::agent_tool::error_output(why));
        }
        let request_id = RequestId(self.next_request.fetch_add(1, Ordering::Relaxed));
        let (reply, answer) = crossbeam_channel::bounded(1);
        lock(&self.questions).insert(request_id, (self.session, reply));
        self.bus.publish(
            self.session,
            HostEvent::QuestionAsked {
                request_id,
                tool_call_id: cx.call_id().cloned(),
                questions: input.questions.clone(),
            },
        );
        let watch = cx.cancel_flag().watch();
        select_biased! {
            recv(answer) -> answers => Ok(ToolOutput {
                text: reply_text(&input.questions, answers.ok().flatten()),
                ..ToolOutput::default()
            }),
            recv(watch.receiver()) -> _ => {
                lock(&self.questions).remove(&request_id);
                self.bus.publish(self.session, HostEvent::QuestionClosed { request_id });
                Err(ToolError::Cancelled)
            }
        }
    }
}

/// Why `questions` cannot be asked, if they cannot.
fn check(questions: &[Question]) -> Result<(), String> {
    if !(1..=4).contains(&questions.len()) {
        return Err("ask one to four questions".to_owned());
    }
    for question in questions {
        if question.question.trim().is_empty() {
            return Err("a question is empty".to_owned());
        }
        if !(2..=4).contains(&question.options.len()) {
            return Err(format!("`{}` needs two to four choices", question.header));
        }
        if question.options.iter().any(|o| o.label.trim().is_empty()) {
            return Err(format!(
                "`{}` has a choice without a label",
                question.header
            ));
        }
    }
    Ok(())
}

/// What the model reads back: each question with the user's answer, or
/// that the user declined.
fn reply_text(questions: &[Question], answers: Answers) -> String {
    let Some(answers) = answers else {
        return "The user declined to answer. Go on with your best judgement, or ask in your \
                reply instead."
            .to_owned();
    };
    let mut text = "The user answered:".to_owned();
    for (question, answer) in questions.iter().zip(answers) {
        let answer = if answer.is_empty() {
            "(no answer)".to_owned()
        } else {
            answer.join(", ")
        };
        let _ = write!(text, "\n- {}: {answer}", question.question);
    }
    text
}

#[cfg(test)]
mod tests {
    use kage_core::protocol::QuestionOption;

    use super::*;

    fn question(options: usize) -> Question {
        Question {
            header: "Auth".into(),
            question: "Which auth method?".into(),
            options: (0..options)
                .map(|n| QuestionOption {
                    label: format!("choice {n}"),
                    description: String::new(),
                })
                .collect(),
            multi_select: false,
        }
    }

    #[test]
    fn questions_need_one_to_four_with_two_to_four_choices() {
        assert!(check(&[question(2)]).is_ok());
        assert!(check(&[]).is_err());
        assert!(check(&vec![question(2); 5]).is_err());
        assert!(check(&[question(1)]).is_err());
        assert!(check(&[question(5)]).is_err());
    }

    #[test]
    fn the_reply_pairs_each_question_with_its_answer() {
        let asked = [question(2), question(3)];
        let text = reply_text(
            &asked,
            Some(vec![
                vec!["choice 0".into()],
                vec!["choice 1".into(), "choice 2".into()],
            ]),
        );
        assert_eq!(
            text,
            "The user answered:\n- Which auth method?: choice 0\n- Which auth method?: choice 1, \
             choice 2"
        );
        assert!(reply_text(&asked, None).starts_with("The user declined"));
    }
}
