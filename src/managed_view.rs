//! Dashboard-only IPC and key normalization for explicitly owned terminals.
use crate::{
    managed::{self, Request, Response},
    managed_vt::KeyInput,
};
use anyhow::{Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::Text;
use std::{path::Path, sync::mpsc};

pub enum Update {
    Frame {
        text: Text<'static>,
        cursor: Option<(u16, u16)>,
        ended: bool,
    },
    Sent,
}

pub struct Reply {
    pub generation: u64,
    pub frame: bool,
    pub result: Result<Update, String>,
}
struct Work {
    generation: u64,
    id: String,
    request: Request,
}
pub struct Client {
    work: mpsc::SyncSender<Work>,
    pub replies: mpsc::Receiver<Reply>,
}
impl Client {
    pub fn new(dir: &Path) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Work>(64);
        let (out, replies) = mpsc::sync_channel(64);
        let dir = dir.to_path_buf();
        std::thread::spawn(move || {
            while let Ok(work) = rx.recv() {
                let frame = matches!(work.request, Request::Frame { .. });
                let result = (|| -> Result<Update> {
                    match managed::request(&dir, &work.id, work.request)? {
                        Response::Frame { screen, ended } => Ok(Update::Frame {
                            text: crate::preview::parse_vt(
                                screen.vt.as_bytes(),
                                screen.cols,
                                screen.rows,
                            )?,
                            cursor: screen.cursor,
                            ended,
                        }),
                        Response::Ok => Ok(Update::Sent),
                        Response::Error { message } => bail!("{message}"),
                    }
                })()
                .map_err(|e| format!("{e:#}"));
                if out
                    .send(Reply {
                        generation: work.generation,
                        frame,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self { work: tx, replies }
    }
    pub fn submit(&self, generation: u64, id: &str, request: Request) -> Result<()> {
        self.work
            .try_send(Work {
                generation,
                id: id.into(),
                request,
            })
            .map_err(|_| {
                anyhow::anyhow!("Terminal input queue is full or disconnected; input mode stopped")
            })
    }

    /// Finish already accepted keystrokes before detaching. Closing a dashboard
    /// must not silently drop the tail of a paste or a command submission.
    pub fn finish(self) {
        drop(self.work);
        while self.replies.recv().is_ok() {}
    }
}

pub fn key(event: KeyEvent) -> Option<KeyInput> {
    let mut text = None;
    let code = match event.code {
        KeyCode::Char(c) => {
            text = Some(c.to_string());
            "char".into()
        }
        KeyCode::Enter => "enter".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "backtab".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::Insert => "insert".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::F(n @ 1..=12) => format!("f{n}"),
        _ => return None,
    };
    let mut modifiers = 0;
    for (flag, bit) in [
        (KeyModifiers::SHIFT, 1),
        (KeyModifiers::ALT, 2),
        (KeyModifiers::CONTROL, 4),
        (KeyModifiers::SUPER, 8),
    ] {
        if event.modifiers.contains(flag) {
            modifiers |= bit;
        }
    }
    Some(KeyInput {
        code,
        text,
        modifiers,
        release: event.kind == KeyEventKind::Release,
        repeat: event.kind == KeyEventKind::Repeat,
    })
}
