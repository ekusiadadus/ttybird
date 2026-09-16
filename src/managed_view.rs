//! Dashboard-only IPC and key normalization for explicitly owned terminals.
use crate::{
    managed::{self, Request, Response},
    managed_vt::KeyInput,
};
use anyhow::{Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::Text;
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

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
enum WatchWork {
    Watch {
        generation: u64,
        id: String,
        cols: u16,
        rows: u16,
    },
    Unwatch,
}
pub struct Client {
    work: mpsc::SyncSender<Work>,
    watch_wake: mpsc::SyncSender<()>,
    watch_state: Arc<Mutex<Option<WatchWork>>>,
    pub replies: Replies,
}

pub struct Replies {
    inner: mpsc::Receiver<Reply>,
    latest_watch: Arc<Mutex<Option<Reply>>>,
}

impl Replies {
    pub fn try_recv(&self) -> std::result::Result<Reply, mpsc::TryRecvError> {
        match self.inner.try_recv() {
            Ok(reply) => Ok(reply),
            Err(mpsc::TryRecvError::Empty) => self
                .latest_watch
                .lock()
                .expect("managed watch reply mutex poisoned")
                .take()
                .ok_or(mpsc::TryRecvError::Empty),
            Err(mpsc::TryRecvError::Disconnected) => self
                .latest_watch
                .lock()
                .expect("managed watch reply mutex poisoned")
                .take()
                .ok_or(mpsc::TryRecvError::Disconnected),
        }
    }

    fn recv(&self) -> std::result::Result<Reply, mpsc::RecvError> {
        if let Some(reply) = self
            .latest_watch
            .lock()
            .expect("managed watch reply mutex poisoned")
            .take()
        {
            return Ok(reply);
        }
        self.inner.recv()
    }
}
impl Client {
    pub fn new(dir: &Path) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Work>(64);
        let (out, reply_rx) = mpsc::sync_channel(64);
        let latest_watch = Arc::new(Mutex::new(None));
        let request_dir = dir.to_path_buf();
        let request_out = out.clone();
        std::thread::spawn(move || {
            while let Ok(work) = rx.recv() {
                let frame = matches!(work.request, Request::Frame { .. });
                let result = (|| -> Result<Update> {
                    match managed::request(&request_dir, &work.id, work.request)? {
                        Response::Frame { screen, ended } => frame_update(screen, ended),
                        Response::Ok => Ok(Update::Sent),
                        Response::Error { message } => bail!("{message}"),
                    }
                })()
                .map_err(|e| format!("{e:#}"));
                if request_out
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
        let (watch_wake, watch_rx) = mpsc::sync_channel(1);
        let watch_state = Arc::new(Mutex::new(None));
        let watch_dir = dir.to_path_buf();
        let manager_state = Arc::clone(&watch_state);
        let manager_latest = Arc::clone(&latest_watch);
        thread::spawn(move || watch_manager(&watch_dir, watch_rx, manager_state, manager_latest));
        Self {
            work: tx,
            watch_wake,
            watch_state,
            replies: Replies {
                inner: reply_rx,
                latest_watch,
            },
        }
    }

    /// Replace the current live screen subscription.
    pub fn watch(&self, generation: u64, id: &str, cols: u16, rows: u16) -> Result<()> {
        *self
            .watch_state
            .lock()
            .map_err(|_| anyhow::anyhow!("Terminal watch state is unavailable"))? =
            Some(WatchWork::Watch {
                generation,
                id: id.into(),
                cols,
                rows,
            });
        wake_watch_manager(&self.watch_wake)
    }

    /// Close the current live screen subscription without stopping its PTY.
    pub fn unwatch(&self) -> Result<()> {
        *self
            .watch_state
            .lock()
            .map_err(|_| anyhow::anyhow!("Terminal watch state is unavailable"))? =
            Some(WatchWork::Unwatch);
        wake_watch_manager(&self.watch_wake)
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
        let Self {
            work,
            watch_wake,
            watch_state: _,
            replies,
        } = self;
        drop(work);
        drop(watch_wake);
        while replies.recv().is_ok() {}
    }
}

fn wake_watch_manager(wake: &mpsc::SyncSender<()>) -> Result<()> {
    match wake.try_send(()) {
        Ok(()) | Err(mpsc::TrySendError::Full(())) => Ok(()),
        Err(mpsc::TrySendError::Disconnected(())) => {
            bail!("Terminal watch worker is disconnected")
        }
    }
}

fn frame_update(screen: crate::managed_vt::Screen, ended: bool) -> Result<Update> {
    Ok(Update::Frame {
        text: crate::preview::parse_vt(screen.vt.as_bytes(), screen.cols, screen.rows)?,
        cursor: screen.cursor,
        ended,
    })
}

#[cfg(unix)]
struct ActiveWatch {
    cancel: managed::SubscriptionCancel,
    cancelled: Arc<AtomicBool>,
    reader: thread::JoinHandle<()>,
}

#[cfg(unix)]
impl ActiveWatch {
    fn stop(self) {
        self.cancelled.store(true, Ordering::Release);
        self.cancel.cancel();
        let _ = self.reader.join();
    }
}

#[cfg(unix)]
fn watch_manager(
    dir: &Path,
    wake: mpsc::Receiver<()>,
    state: Arc<Mutex<Option<WatchWork>>>,
    latest_watch: Arc<Mutex<Option<Reply>>>,
) {
    let mut active: Option<ActiveWatch> = None;
    while wake.recv().is_ok() {
        let Some(next) = state
            .lock()
            .expect("managed watch state mutex poisoned")
            .take()
        else {
            continue;
        };
        if let Some(previous) = active.take() {
            previous.stop();
        }
        let WatchWork::Watch {
            generation,
            id,
            cols,
            rows,
        } = next
        else {
            continue;
        };
        let mut subscription = match managed::subscribe(dir, &id, cols, rows) {
            Ok(subscription) => subscription,
            Err(error) => {
                publish_watch_reply(
                    &latest_watch,
                    Reply {
                        generation,
                        frame: true,
                        result: Err(format!("{error:#}")),
                    },
                );
                continue;
            }
        };
        let cancel = match subscription.cancellation_handle() {
            Ok(cancel) => cancel,
            Err(error) => {
                publish_watch_reply(
                    &latest_watch,
                    Reply {
                        generation,
                        frame: true,
                        result: Err(format!("{error:#}")),
                    },
                );
                continue;
            }
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let reader_cancelled = Arc::clone(&cancelled);
        let reader_latest = Arc::clone(&latest_watch);
        let reader = thread::spawn(move || {
            loop {
                match subscription.read_next() {
                    Ok(Some(Response::Frame { screen, ended })) => {
                        let result =
                            frame_update(screen, ended).map_err(|error| format!("{error:#}"));
                        if !publish_watch_reply(
                            &reader_latest,
                            Reply {
                                generation,
                                frame: true,
                                result,
                            },
                        ) || ended
                        {
                            break;
                        }
                    }
                    Ok(Some(Response::Error { message })) => {
                        publish_watch_reply(
                            &reader_latest,
                            Reply {
                                generation,
                                frame: true,
                                result: Err(message),
                            },
                        );
                        break;
                    }
                    Ok(Some(Response::Ok)) => {
                        publish_watch_reply(
                            &reader_latest,
                            Reply {
                                generation,
                                frame: true,
                                result: Err(
                                    "invalid response on managed screen subscription".into()
                                ),
                            },
                        );
                        break;
                    }
                    Ok(None) if reader_cancelled.load(Ordering::Acquire) => break,
                    Ok(None) => {
                        publish_watch_reply(
                            &reader_latest,
                            Reply {
                                generation,
                                frame: true,
                                result: Err("managed screen subscription closed".into()),
                            },
                        );
                        break;
                    }
                    Err(_) if reader_cancelled.load(Ordering::Acquire) => break,
                    Err(error) => {
                        publish_watch_reply(
                            &reader_latest,
                            Reply {
                                generation,
                                frame: true,
                                result: Err(format!("{error:#}")),
                            },
                        );
                        break;
                    }
                }
            }
        });
        active = Some(ActiveWatch {
            cancel,
            cancelled,
            reader,
        });
    }
    if let Some(active) = active {
        active.stop();
    }
}

#[cfg(not(unix))]
fn watch_manager(
    _dir: &Path,
    wake: mpsc::Receiver<()>,
    state: Arc<Mutex<Option<WatchWork>>>,
    latest_watch: Arc<Mutex<Option<Reply>>>,
) {
    while wake.recv().is_ok() {
        let Some(next) = state
            .lock()
            .expect("managed watch state mutex poisoned")
            .take()
        else {
            continue;
        };
        if let WatchWork::Watch { generation, .. } = next {
            publish_watch_reply(
                &latest_watch,
                Reply {
                    generation,
                    frame: true,
                    result: Err("managed terminal sessions require Unix".into()),
                },
            );
        }
    }
}

fn publish_watch_reply(latest_watch: &Mutex<Option<Reply>>, reply: Reply) -> bool {
    *latest_watch
        .lock()
        .expect("managed watch reply mutex poisoned") = Some(reply);
    true
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_bursts_keep_only_the_latest_reply() {
        let (_sender, receiver) = mpsc::sync_channel(1);
        let latest = Arc::new(Mutex::new(None));
        let replies = Replies {
            inner: receiver,
            latest_watch: Arc::clone(&latest),
        };
        for generation in 1..=128 {
            publish_watch_reply(
                &latest,
                Reply {
                    generation,
                    frame: true,
                    result: Ok(Update::Sent),
                },
            );
        }
        assert_eq!(replies.try_recv().unwrap().generation, 128);
        assert!(matches!(replies.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }
}
