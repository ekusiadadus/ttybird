use crate::{
    handoff::{self, Draft},
    model::Session,
};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

pub struct View {
    pub draft: Draft,
    pub source: Option<Session>,
    pub editing: bool,
    pub cursor: usize,
    pub scroll: u16,
    pub conversation_added: bool,
}
pub enum Action {
    None,
    Close,
    Save,
    Start,
    Conversation,
}
impl View {
    pub fn new(draft: Draft, source: Option<Session>) -> Self {
        Self {
            draft,
            source,
            editing: false,
            cursor: 0,
            scroll: 0,
            conversation_added: false,
        }
    }
    pub fn event(&mut self, event: Event) -> Action {
        if let Event::Paste(text) = &event {
            if self.editing {
                self.insert(&handoff::clean(text));
            }
            return Action::None;
        }
        let Event::Key(key) = event else {
            return Action::None;
        };
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        if self.editing {
            match key.code {
                KeyCode::Esc => self.editing = false,
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.editing = false
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.insert(&c.to_string())
                }
                KeyCode::Enter => self.insert("\n"),
                KeyCode::Backspace if self.cursor > 0 => {
                    let previous = self.draft.text[..self.cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.draft.text.replace_range(previous..self.cursor, "");
                    self.cursor = previous;
                }
                KeyCode::Delete if self.cursor < self.draft.text.len() => {
                    let length = self.draft.text[self.cursor..]
                        .chars()
                        .next()
                        .unwrap()
                        .len_utf8();
                    self.draft
                        .text
                        .replace_range(self.cursor..self.cursor + length, "");
                }
                KeyCode::Left if self.cursor > 0 => {
                    self.cursor = self.draft.text[..self.cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0)
                }
                KeyCode::Right if self.cursor < self.draft.text.len() => {
                    self.cursor += self.draft.text[self.cursor..]
                        .chars()
                        .next()
                        .unwrap()
                        .len_utf8()
                }
                KeyCode::Home => {
                    self.cursor = self.draft.text[..self.cursor]
                        .rfind('\n')
                        .map_or(0, |i| i + 1)
                }
                KeyCode::End => {
                    self.cursor += self.draft.text[self.cursor..]
                        .find('\n')
                        .unwrap_or(self.draft.text.len() - self.cursor)
                }
                KeyCode::Up => self.move_line(false),
                KeyCode::Down => self.move_line(true),
                KeyCode::PageUp => self.cursor = 0,
                KeyCode::PageDown => self.cursor = self.draft.text.len(),
                _ => (),
            }
            self.scroll = self.draft.text[..self.cursor]
                .chars()
                .filter(|c| *c == '\n')
                .count()
                .saturating_sub(5)
                .min(u16::MAX as usize) as u16;
            return Action::None;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return if key.code == KeyCode::Char('c')
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                Action::Close
            } else {
                Action::None
            };
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => Action::Close,
            KeyCode::Char('e') => {
                self.editing = true;
                Action::None
            }
            KeyCode::Char('w') => Action::Save,
            KeyCode::Char('S') => Action::Start,
            KeyCode::Char('c') if !self.conversation_added => Action::Conversation,
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_add(1);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
                Action::None
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(10);
                Action::None
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(10);
                Action::None
            }
            _ => Action::None,
        }
    }
    fn move_line(&mut self, down: bool) {
        let start = self.draft.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |i| i + 1);
        let column = self.draft.text[start..self.cursor].chars().count();
        let next_start = if down {
            let Some(end) = self.draft.text[self.cursor..].find('\n') else {
                return;
            };
            self.cursor + end + 1
        } else {
            if start == 0 {
                return;
            }
            self.draft.text[..start - 1]
                .rfind('\n')
                .map_or(0, |i| i + 1)
        };
        let line = self.draft.text[next_start..]
            .split('\n')
            .next()
            .unwrap_or("");
        self.cursor = next_start
            + line
                .char_indices()
                .nth(column)
                .map_or(line.len(), |(i, _)| i);
    }
    fn insert(&mut self, text: &str) {
        if self.draft.text.len() + text.len() <= handoff::MAX_TEXT {
            self.draft.text.insert_str(self.cursor, text);
            self.cursor += text.len();
        }
    }
    pub fn draw(&self, frame: &mut Frame, area: Rect, notice: Option<&str>) {
        let popup = Rect::new(
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        frame.render_widget(Clear, popup);
        let title = format!(
            "Handoff → Codex · {} · {}",
            self.draft.workspace.checkout_root.display(),
            self.draft.workspace.branch.as_deref().unwrap_or("detached")
        );
        let footer = if self.editing {
            "EDIT · arrows/Home/End · paste · Ctrl-S or Esc reviews"
        } else {
            "REVIEW · e edit · c include conversation · w save · S share and start Codex · Esc cancel"
        };
        let mut text = self.draft.text.clone();
        if self.editing {
            text.insert(self.cursor, '▏');
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .title_bottom(footer);
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let panels = ratatui::layout::Layout::vertical([
            ratatui::layout::Constraint::Length(2),
            ratatui::layout::Constraint::Min(0),
        ])
        .split(inner);
        frame.render_widget(
            Paragraph::new(notice.unwrap_or(
                "Review and edit the draft. Only S shares it with a new Codex session.",
            ))
            .wrap(Wrap { trim: false }),
            panels[0],
        );
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            panels[1],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    #[test]
    fn editing_unicode_and_paste_cannot_launch_until_review() {
        let workspace = crate::workspace::WorkspaceInfo {
            checkout_root: "/synthetic".into(),
            git_common_dir: "/synthetic/.git".into(),
            branch: Some("main".into()),
            head_commit: None,
            dirty: false,
            changes: vec![],
            changes_truncated: false,
            linked_worktree: false,
        };
        let mut view = View::new(
            Draft {
                version: 1,
                source: "selected".into(),
                title: "test".into(),
                workspace,
                text: "あいう\nxyz".into(),
            },
            None,
        );
        assert!(matches!(view.event(key(KeyCode::Enter)), Action::None));
        view.event(key(KeyCode::Char('e')));
        assert!(matches!(view.event(key(KeyCode::Char('S'))), Action::None));
        view.event(key(KeyCode::Backspace));
        view.event(key(KeyCode::Right));
        view.event(key(KeyCode::Down));
        assert_eq!(&view.draft.text[..view.cursor], "あいう\nx");
        view.event(Event::Paste("秘密\x1b\0".into()));
        assert!(view.draft.text.contains("x秘密yz"));
        assert!(!view.draft.text.contains('\x1b'));
        view.event(key(KeyCode::Backspace));
        assert!(view.draft.text.contains("x秘yz"));
        view.event(key(KeyCode::Esc));
        assert!(matches!(view.event(key(KeyCode::Char('S'))), Action::Start));
        assert!(matches!(view.event(key(KeyCode::Esc)), Action::Close));
    }
}
