//! Synthetic render fixture. Emits Ratatui cells as JSON for screenshot tooling.
use ratatui::{Terminal, backend::TestBackend, style::Color};
use ttybird::{
    model::{Activity, Confidence, Provider, Session, Snapshot, Target},
    ui::App,
};

fn rgb(color: Color, background: bool) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Black => "#101820".into(),
        Color::White | Color::Gray => "#dce7ed".into(),
        _ => {
            if background {
                "#101820".into()
            } else {
                "#dce7ed".into()
            }
        }
    }
}

fn session(
    id: &str,
    cwd: &str,
    provider: Provider,
    activity: Activity,
    confidence: Confidence,
    pid: u32,
) -> Session {
    Session {
        id: id.into(),
        provider,
        parent_id: None,
        host: "local".into(),
        pid: Some(pid),
        process_started_at: Some(100),
        tty: Some(format!("ttys00{pid}")),
        cwd: Some(format!("/work/{cwd}")),
        model: None,
        activity,
        confidence,
        evidence: "Synthetic UI fixture; no real agent content.".into(),
        updated_at: Some(chrono::Utc::now().timestamp()),
        target: None,
    }
}

fn main() -> anyhow::Result<()> {
    let mut snapshot = Snapshot::new("local".into());
    let mut attention = session(
        "example-permission",
        "demo-api",
        Provider::Claude,
        Activity::WaitingInput,
        Confidence::Observed,
        1,
    );
    attention.model = Some("Claude".into());
    attention.target = Some(Target::Tmux {
        socket: None,
        pane: "%1".into(),
    });
    snapshot.sessions.push(attention);
    let mut parent = session(
        "example-astra-parent",
        "ttybird",
        Provider::Codex,
        Activity::Working,
        Confidence::Inferred,
        6,
    );
    parent.model = Some("gpt-6-astra".into());
    let mut child = parent.clone();
    child.id = "example-sol-child".into();
    child.parent_id = Some(parent.id.clone());
    child.model = Some("gpt-5.6-sol".into());
    let mut second_child = child.clone();
    second_child.id = "example-sol-child-02".into();
    second_child.activity = Activity::Unknown;
    let mut grandchild = child.clone();
    grandchild.id = "example-sol-grandchild-03".into();
    grandchild.parent_id = Some(child.id.clone());
    snapshot.sessions.push(second_child);
    snapshot.sessions.push(grandchild);
    snapshot.sessions.push(child);
    snapshot.sessions.push(parent);

    snapshot.sessions.push(session(
        "example-gemini",
        "demo-web",
        Provider::Gemini,
        Activity::Unknown,
        Confidence::Observed,
        2,
    ));
    snapshot.sessions.push(session(
        "example-idle",
        "demo-docs",
        Provider::Claude,
        Activity::Idle,
        Confidence::Observed,
        3,
    ));
    snapshot.sessions.push(session(
        "example-unverified",
        "demo-infra",
        Provider::OpenCode,
        Activity::Unknown,
        Confidence::Observed,
        4,
    ));
    let mut background = session(
        "codex-pid-5-100",
        "demo-infra",
        Provider::Codex,
        Activity::Unknown,
        Confidence::Observed,
        5,
    );
    background.tty = None;
    background.evidence = "headless process; session metadata unavailable".into();
    snapshot.sessions.push(background);
    let mut app = App {
        notice: Some("DEMO — synthetic sample data".into()),
        ..Default::default()
    };
    app.set_snapshots(vec![snapshot]);
    let step = std::env::args()
        .find_map(|arg| {
            arg.strip_prefix("--step=")
                .and_then(|v| v.parse::<u8>().ok())
        })
        .unwrap_or(0);
    if std::env::args().any(|arg| arg == "--tree") || step > 0 {
        app.selected = app
            .rows()
            .iter()
            .position(|s| s.id == "example-astra-parent")
            .unwrap_or(0);
    }
    // Exercise the same tree methods used by the interactive key handler.
    if step == 2 {
        app.toggle_branch();
        assert!(!app.rows().iter().any(|s| s.id == "example-sol-child"));
    }
    if step >= 3 {
        app.toggle_branch();
        app.expand_or_child();
        assert!(app.rows().iter().any(|s| s.id == "example-sol-child"));
    }
    if (4..=6).contains(&step) {
        app.expand_or_child();
        assert!(app.selected_session().unwrap().parent_id.is_some());
    }
    if step == 5 {
        app.show_details = true;
    }
    if std::env::args().any(|arg| arg == "--picker") || step == 7 {
        app.ghostty_picker =
            app.selected_session()
                .cloned()
                .map(|session| ttybird::ui::GhosttyPicker {
                    session,
                    selected: 0,
                    terminals: vec![
                        ttybird::navigation::GhosttyTerminal {
                            id: "11111111-1111-1111-1111-111111111111".into(),
                            cwd: "/work/ttybird".into(),
                            title: "Astra — ttybird".into(),
                        },
                        ttybird::navigation::GhosttyTerminal {
                            id: "22222222-2222-2222-2222-222222222222".into(),
                            cwd: "/work/ttybird".into(),
                            title: "Shell — ttybird".into(),
                        },
                        ttybird::navigation::GhosttyTerminal {
                            id: "33333333-3333-3333-3333-333333333333".into(),
                            cwd: "/work/api".into(),
                            title: "Gemini — api".into(),
                        },
                    ],
                });
    }
    if std::env::args().any(|arg| arg == "--terminal") || step == 6 {
        app.show_preview = true;
        app.preview_notice = Some("DEMO · synthetic terminal output · libghostty-vt".into());
        app.preview_text = Some(ttybird::preview::parse(
            b"\x1b[1;38;2;57;197;187m$ cargo test\x1b[0m\n\n\x1b[32mtest\x1b[0m parser_preserves_colors ... ok\n\x1b[32mtest\x1b[0m tmux_capture_is_read_only ... ok\n\x1b[32mtest\x1b[0m stale_selection_is_cleared ... ok\n\n\x1b[1;32mtest result: ok\x1b[0m\n\nThis is synthetic output for the preview demo.\nNo agent input is sent from this pane.\n",
            80, 20,
        )?);
    }
    let width = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(120u16);
    let height = std::env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(32u16);
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|frame| ttybird::ui::draw(frame, &mut app))?;
    let buffer = terminal.backend().buffer();
    let cells:Vec<_>=buffer.content.iter().enumerate().map(|(i,c)|serde_json::json!({"x":i%usize::from(width),"y":i/usize::from(width),"text":c.symbol(),"fg":rgb(c.fg,false),"bg":rgb(c.bg,true)})).collect();
    println!(
        "{}",
        serde_json::json!({"width":width,"height":height,"cells":cells})
    );
    Ok(())
}
