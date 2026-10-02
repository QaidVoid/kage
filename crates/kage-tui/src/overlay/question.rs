//! The questions an `ask_user_question` call asks, one at a time, in
//! the approval panel. Each question lists its choices with their
//! descriptions, then a row to answer in your own words. A question
//! that allows several choices toggles them and ends on a `Done` row.

use kage_core::protocol::Question;
use ratatui::crossterm::event::KeyCode;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_width::UnicodeWidthStr as _;

use crate::theme::Theme;

const INDENT: &str = "   ";

/// What a key did to the questions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Still asking.
    Stay,
    /// Open the field for an answer in the user's own words.
    OwnWords,
    /// Every question has its answer, in order.
    Done(Vec<Vec<String>>),
    /// The user declined to answer.
    Decline,
}

/// The questions of one call and the answers given so far.
#[derive(Clone, Debug)]
pub struct QuestionFlow {
    questions: Vec<Question>,
    /// The question on screen.
    at: usize,
    /// The highlighted row of that question.
    selected: usize,
    /// The choices picked so far, for a question that allows several.
    picked: Vec<bool>,
    answers: Vec<Vec<String>>,
}

impl QuestionFlow {
    /// The questions an `ask_user_question` call's `input` carries, or
    /// `None` when it carries none.
    #[must_use]
    pub fn from_input(input: &Value) -> Option<Self> {
        let questions: Vec<Question> =
            serde_json::from_value(input.get("questions")?.clone()).ok()?;
        let first = questions.first()?.options.len();
        Some(Self {
            questions,
            at: 0,
            selected: 0,
            picked: vec![false; first],
            answers: Vec::new(),
        })
    }

    fn question(&self) -> &Question {
        &self.questions[self.at]
    }

    /// The row that opens the field for the user's own words.
    fn own_row(&self) -> usize {
        self.question().options.len()
    }

    /// Rows of the question on screen: its choices, the own-words row
    /// and, when several choices may be picked, the `Done` row.
    fn rows(&self) -> usize {
        self.own_row() + 1 + usize::from(self.question().multi_select)
    }

    /// The panel's title: the question's header and, when there are
    /// several, which one this is.
    #[must_use]
    pub fn title(&self) -> String {
        let header = &self.question().header;
        match self.questions.len() {
            1 => header.clone(),
            n => format!("{header} ({} of {n})", self.at + 1),
        }
    }

    /// The footer hint for the question on screen.
    #[must_use]
    pub fn hint(&self) -> &'static str {
        if self.question().multi_select {
            "space to pick \u{b7} enter \u{b7} esc to skip"
        } else {
            "1-9 or enter \u{b7} esc to skip"
        }
    }

    /// Handle a key on the question on screen.
    pub fn handle_key(&mut self, code: KeyCode) -> Step {
        let rows = self.rows();
        let multi = self.question().multi_select;
        match code {
            KeyCode::Esc => Step::Decline,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = (self.selected + rows - 1) % rows;
                Step::Stay
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1) % rows;
                Step::Stay
            }
            KeyCode::Char(' ') if multi && self.selected < self.own_row() => {
                self.picked[self.selected] ^= true;
                Step::Stay
            }
            KeyCode::Char(c @ '1'..='9') => {
                let row = usize::from(c as u8 - b'1');
                if row >= rows {
                    return Step::Stay;
                }
                self.selected = row;
                self.choose()
            }
            KeyCode::Enter => self.choose(),
            _ => Step::Stay,
        }
    }

    /// Act on the highlighted row: pick a choice (toggle one, for a
    /// question that allows several), open the own-words field, or
    /// finish picking.
    fn choose(&mut self) -> Step {
        let own = self.own_row();
        if self.selected == own {
            return Step::OwnWords;
        }
        if self.selected > own {
            let picked = self
                .question()
                .options
                .iter()
                .zip(&self.picked)
                .filter(|(_, on)| **on)
                .map(|(option, _)| option.label.clone())
                .collect();
            return self.answer(picked);
        }
        if self.question().multi_select {
            self.picked[self.selected] ^= true;
            return Step::Stay;
        }
        let label = self.question().options[self.selected].label.clone();
        self.answer(vec![label])
    }

    /// Answer the question on screen in the user's own words.
    pub fn own_words(&mut self, text: &str) -> Step {
        let text = text.trim();
        if text.is_empty() {
            return Step::Stay;
        }
        self.answer(vec![text.to_owned()])
    }

    /// Record `answer` and move to the next question, or finish.
    fn answer(&mut self, answer: Vec<String>) -> Step {
        self.answers.push(answer);
        if self.at + 1 == self.questions.len() {
            return Step::Done(std::mem::take(&mut self.answers));
        }
        self.at += 1;
        self.selected = 0;
        self.picked = vec![false; self.question().options.len()];
        Step::Stay
    }

    /// The question text wrapped to `width`, above its rows.
    #[must_use]
    pub fn summary(&self, theme: &Theme, width: u16) -> Vec<Line<'static>> {
        let strong = Style::default()
            .fg(theme.assistant_fg)
            .add_modifier(Modifier::BOLD);
        let text = &self.question().question;
        let wrap = width.saturating_sub(4);
        crate::view::wrap_input_rows(text, wrap)
            .into_iter()
            .map(|(s, e)| Line::styled(format!("{INDENT}{}", &text[s..e]), strong))
            .collect()
    }

    /// The rows of the question on screen: each choice with its
    /// description, the own-words row and the `Done` row.
    #[must_use]
    pub fn rows_lines(&self, theme: &Theme, width: u16) -> Vec<Line<'static>> {
        let approval = theme.group_style("KageApproval");
        let strong = approval.add_modifier(Modifier::BOLD);
        let text = Style::default().fg(theme.assistant_fg);
        let muted = theme.group_style("KageMuted");
        let question = self.question();
        let mut labels: Vec<(String, String)> = question
            .options
            .iter()
            .enumerate()
            .map(|(i, option)| {
                let check = match (question.multi_select, self.picked[i]) {
                    (false, _) => "",
                    (true, true) => "[x] ",
                    (true, false) => "[ ] ",
                };
                (
                    format!("{check}{}", option.label),
                    option.description.clone(),
                )
            })
            .collect();
        labels.push(("Answer in my own words".to_owned(), String::new()));
        if question.multi_select {
            let count = self.picked.iter().filter(|on| **on).count();
            labels.push((format!("Done ({count} picked)"), String::new()));
        }
        labels
            .into_iter()
            .enumerate()
            .map(|(i, (label, description))| {
                let on = i == self.selected;
                let label = format!("{}. {label}", i + 1);
                let mut spans = vec![
                    Span::styled(if on { " > " } else { "   " }, strong),
                    Span::styled(label.clone(), if on { strong } else { text }),
                ];
                let room = usize::from(width).saturating_sub(3 + label.width() + 2);
                if !description.is_empty() && room > 8 {
                    spans.push(Span::styled(
                        format!(
                            "  {}",
                            crate::view::truncate_to_width(&description, room, "...")
                        ),
                        muted,
                    ));
                }
                Line::from(spans)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow(multi: bool) -> QuestionFlow {
        QuestionFlow::from_input(&serde_json::json!({"questions": [
            {"header": "Auth", "question": "Which auth?", "multi_select": multi,
             "options": [{"label": "OAuth", "description": "a"}, {"label": "Keys", "description": "b"}]},
            {"header": "Store", "question": "Where?",
             "options": [{"label": "Disk"}, {"label": "Memory"}]},
        ]}))
        .unwrap()
    }

    #[test]
    fn single_choices_answer_each_question_in_turn() {
        let mut flow = flow(false);
        assert_eq!(flow.title(), "Auth (1 of 2)");
        assert_eq!(flow.handle_key(KeyCode::Char('2')), Step::Stay);
        assert_eq!(flow.title(), "Store (2 of 2)");
        assert_eq!(
            flow.handle_key(KeyCode::Enter),
            Step::Done(vec![vec!["Keys".into()], vec!["Disk".into()]])
        );
    }

    #[test]
    fn several_choices_toggle_then_finish_on_done() {
        let mut flow = flow(true);
        flow.handle_key(KeyCode::Char(' '));
        flow.handle_key(KeyCode::Down);
        flow.handle_key(KeyCode::Enter);
        assert_eq!(flow.handle_key(KeyCode::Char('4')), Step::Stay, "done row");
        flow.handle_key(KeyCode::Char('3'));
        assert_eq!(
            flow.own_words("  in redis  "),
            Step::Done(vec![
                vec!["OAuth".into(), "Keys".into()],
                vec!["in redis".into()]
            ])
        );
    }

    #[test]
    fn the_own_words_row_opens_the_field_and_esc_declines() {
        let mut flow = flow(false);
        assert_eq!(flow.handle_key(KeyCode::Char('3')), Step::OwnWords);
        assert_eq!(flow.own_words("  "), Step::Stay, "an empty answer waits");
        assert_eq!(flow.handle_key(KeyCode::Esc), Step::Decline);
    }
}
