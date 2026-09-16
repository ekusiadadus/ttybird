use std::collections::{HashMap, HashSet};
use std::path::Path;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap,
};

use crate::model::{Activity, Confidence, Provider, Session, Snapshot, Target};
use crate::navigation::GhosttyTerminal;

const TEAL: Color = Color::Rgb(57, 197, 187);
const TEAL_DIM: Color = Color::Rgb(49, 139, 137);
const SLATE: Color = Color::Rgb(126, 145, 157);
const SLATE_DARK: Color = Color::Rgb(35, 52, 61);
const BORDER: Color = Color::Rgb(65, 83, 92);
const AMBER: Color = Color::Rgb(231, 181, 82);
const CORAL: Color = Color::Rgb(239, 126, 116);

#[derive(Debug, Clone)]
pub struct GhosttyPicker {
    pub session: Session,
    pub terminals: Vec<GhosttyTerminal>,
    pub selected: usize,
}

#[derive(Default)]
pub struct App {
    pub handoff: Option<crate::handoff_view::View>,
    pub attention: Vec<crate::attention::AttentionItem>,
    pub show_inbox: bool,
    pub inbox_selected: usize,
    pub snapshots: Vec<Snapshot>,
    pub selected: usize,
    pub query: String,
    pub searching: bool,
    pub needs_only: bool,
    pub show_background: bool,
    pub show_history: bool,
    pub live_only: bool,
    pub show_help: bool,
    pub notice: Option<String>,
    pub refreshing: bool,
    pub liveness_unavailable: bool,
    pub show_details: bool,
    pub detail_scroll: u16,
    pub show_preview: bool,
    pub preview_text: Option<Text<'static>>,
    pub preview_notice: Option<String>,
    pub preview_scroll: u16,
    pub show_conversation: bool,
    pub conversation_text: Option<String>,
    pub collapsed: HashSet<(String, Provider, String)>,
    pub ghostty_picker: Option<GhosttyPicker>,
    pub managed_id: Option<String>,
    pub managed_parent: bool,
    pub managed_text: Option<Text<'static>>,
    pub managed_cursor: Option<(u16, u16)>,
    pub managed_notice: Option<String>,
    pub terminal_input: bool,
}

/// A subagent often has no independent terminal. Use the recorded ancestry,
/// never a directory match, to navigate to its nearest available parent.
pub fn navigation_session<'a>(
    session: &'a Session,
    snapshots: &'a [Snapshot],
) -> Option<&'a Session> {
    let mut current = session;
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(current.id.as_str()) {
            return None;
        }
        if current.target.is_some() || current.parent_id.is_none() {
            return Some(current);
        }
        let parent = current.parent_id.as_deref()?;
        current = snapshots
            .iter()
            .flat_map(|s| &s.sessions)
            .find(|candidate| {
                candidate.id == parent
                    && candidate.host == session.host
                    && candidate.provider == session.provider
            })?;
    }
}

fn enter_hint(session: &Session, snapshots: &[Snapshot]) -> String {
    match navigation_session(session, snapshots) {
        Some(target)
            if target.id != session.id && matches!(target.target, Some(Target::Managed { .. })) =>
        {
            "Enter: operate PARENT terminal · c: read this child's conversation".into()
        }
        Some(target) if matches!(target.target, Some(Target::Managed { .. })) => {
            "Enter: operate owned terminal · Ctrl+]: return to list".into()
        }
        Some(target) if target.id != session.id && target.target.is_some() => {
            "Enter: open parent terminal · c: read this child's conversation".into()
        }
        Some(target) if target.id != session.id => {
            "Enter: link the parent's terminal once · child has no separate pane".into()
        }
        Some(target) if target.target.is_none() => {
            "Enter: link this session to its terminal (one-time setup)".into()
        }
        Some(_) => "Enter: open this session's terminal".into(),
        None => "No parent terminal found · c: read this child's conversation".into(),
    }
}

struct TreeRow<'a> {
    session: &'a Session,
    parent: Option<&'a Session>,
    depth: usize,
    ancestor_has_more: Vec<bool>,
    is_last: bool,
    /// All direct children retained in the snapshot, including filtered rows.
    child_count: usize,
    /// Direct children admitted by the current filters, before branch folding.
    visible_child_count: usize,
    working_children: usize,
    collapsed: bool,
}

impl App {
    pub fn set_snapshots(&mut self, snapshots: Vec<Snapshot>) {
        let previous = self.selected_session().cloned();
        self.snapshots = snapshots;
        self.liveness_unavailable = false;
        self.restore_selection(previous.as_ref());
    }

    pub fn invalidate_liveness(&mut self) {
        self.liveness_unavailable = true;
        self.ghostty_picker = None;
        self.preview_text = None;
        self.show_conversation = false;
        self.conversation_text = None;
        self.preview_scroll = 0;
        self.preview_notice = Some("Current liveness unavailable; preview paused.".into());
        for snapshot in &mut self.snapshots {
            for session in &mut snapshot.sessions {
                session.pid = None;
                session.process_started_at = None;
                session.tty = None;
                session.target = None;
                session.activity = Activity::Unknown;
                session.confidence = Confidence::Unknown;
                session.evidence = "Last observation retained after collection failure; current liveness unavailable".into();
            }
        }
        self.move_selection(0);
    }

    fn sorted_sessions(&self) -> Vec<&Session> {
        let query = self.query.trim().to_lowercase();
        let universe: Vec<_> = self
            .snapshots
            .iter()
            .flat_map(|snapshot| snapshot.sessions.iter())
            .collect();
        let hierarchy: HashMap<_, _> = universe
            .iter()
            .map(|session| (session_key(session), hierarchy_key(session, &universe)))
            .collect();
        let mut group_ranks = HashMap::new();
        let mut group_workspaces = HashMap::new();
        for session in &universe {
            let Some((root_id, _)) = hierarchy.get(&session_key(session)) else {
                continue;
            };
            let group_key = (session.host.clone(), session.provider, root_id.clone());
            group_ranks
                .entry(group_key.clone())
                .and_modify(|rank: &mut u8| *rank = (*rank).min(state_rank(session)))
                .or_insert_with(|| state_rank(session));
            if session.id == *root_id {
                group_workspaces.insert(group_key, workspace(session));
            }
        }
        let mut rows: Vec<_> = self
            .snapshots
            .iter()
            .flat_map(|snapshot| snapshot.sessions.iter())
            .filter(|session| {
                self.show_background
                    || !(is_background(session) || is_auxiliary_process(session, &universe))
            })
            .filter(|session| self.show_history || session.pid.is_some())
            .filter(|session| {
                !self.needs_only
                    || (session.activity == Activity::WaitingInput
                        && session.confidence == Confidence::Observed)
            })
            .filter(|session| !self.live_only || session.pid.is_some())
            .filter(|session| query.is_empty() || matches_query(session, &query))
            .collect();

        rows.sort_by(|left, right| {
            let left_hierarchy = hierarchy
                .get(&session_key(left))
                .cloned()
                .unwrap_or_else(|| (left.id.clone(), 0));
            let right_hierarchy = hierarchy
                .get(&session_key(right))
                .cloned()
                .unwrap_or_else(|| (right.id.clone(), 0));
            let left_group = (left.host.clone(), left.provider, left_hierarchy.0.clone());
            let right_group = (
                right.host.clone(),
                right.provider,
                right_hierarchy.0.clone(),
            );
            group_ranks
                .get(&left_group)
                .copied()
                .unwrap_or_else(|| state_rank(left))
                .cmp(
                    &group_ranks
                        .get(&right_group)
                        .copied()
                        .unwrap_or_else(|| state_rank(right)),
                )
                .then_with(|| {
                    group_workspaces
                        .get(&left_group)
                        .cloned()
                        .unwrap_or_else(|| workspace(left))
                        .cmp(
                            &group_workspaces
                                .get(&right_group)
                                .cloned()
                                .unwrap_or_else(|| workspace(right)),
                        )
                })
                .then_with(|| provider_label(&left.provider).cmp(provider_label(&right.provider)))
                .then_with(|| left.host.cmp(&right.host))
                .then_with(|| left_hierarchy.cmp(&right_hierarchy))
                .then_with(|| state_rank(left).cmp(&state_rank(right)))
                .then_with(|| workspace(left).cmp(&workspace(right)))
                .then_with(|| left.id.cmp(&right.id))
        });
        rows
    }

    fn tree_rows(&self) -> Vec<TreeRow<'_>> {
        let sorted = self.sorted_sessions();
        let universe: Vec<_> = self
            .snapshots
            .iter()
            .flat_map(|snapshot| snapshot.sessions.iter())
            .collect();
        let by_key: HashMap<_, _> = universe
            .iter()
            .map(|session| (session_key(session), *session))
            .collect();
        let visible_keys: HashSet<_> = sorted.iter().map(|session| session_key(session)).collect();
        let mut known_children: HashMap<_, Vec<&Session>> = HashMap::new();
        for child in &universe {
            let Some(parent_key) = parent_key(child) else {
                continue;
            };
            if by_key.contains_key(&parent_key) {
                known_children.entry(parent_key).or_default().push(child);
            }
        }
        let mut visible_children: HashMap<_, Vec<&Session>> = HashMap::new();
        for child in &sorted {
            let Some(parent_key) = parent_key(child) else {
                continue;
            };
            if visible_keys.contains(&parent_key) {
                visible_children.entry(parent_key).or_default().push(child);
            }
        }
        let roots: Vec<_> = sorted
            .iter()
            .copied()
            .filter(|session| {
                parent_key(session).is_none_or(|parent| !visible_keys.contains(&parent))
            })
            .collect();
        let reveal_collapsed = !self.query.trim().is_empty() || self.needs_only;
        let mut visited = HashSet::new();
        let mut rows = Vec::with_capacity(sorted.len());

        for (index, root) in roots.iter().enumerate() {
            let parent = parent_key(root).and_then(|key| by_key.get(&key).copied());
            append_tree(
                root,
                parent,
                0,
                Vec::new(),
                index + 1 == roots.len(),
                &visible_children,
                &known_children,
                &self.collapsed,
                reveal_collapsed,
                &mut visited,
                &mut rows,
            );
        }
        // A component with no root is cyclic. Emit each remaining node once in
        // the stable sorted order, and let the visited set break the cycle.
        for session in sorted {
            if visited.contains(&session_key(session)) {
                continue;
            }
            append_tree(
                session,
                parent_key(session).and_then(|key| by_key.get(&key).copied()),
                0,
                Vec::new(),
                true,
                &visible_children,
                &known_children,
                &self.collapsed,
                reveal_collapsed,
                &mut visited,
                &mut rows,
            );
        }
        rows
    }

    pub fn rows(&self) -> Vec<&Session> {
        self.tree_rows()
            .into_iter()
            .map(|row| row.session)
            .collect()
    }

    pub fn selected_session(&self) -> Option<&Session> {
        self.rows().get(self.selected).copied()
    }

    pub fn move_selection(&mut self, delta: isize) {
        let count = self.rows().len();
        if count == 0 {
            self.selected = 0;
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(count.saturating_sub(1));
    }

    pub fn restore_selection(&mut self, previous: Option<&Session>) {
        let rows = self.rows();
        let Some(previous) = previous else {
            self.selected = self.selected.min(rows.len().saturating_sub(1));
            return;
        };
        let previous_key = session_key(previous);
        if let Some(index) = rows
            .iter()
            .position(|session| session_key(session) == previous_key)
        {
            self.selected = index;
            return;
        }

        let by_key: HashMap<_, _> = self
            .snapshots
            .iter()
            .flat_map(|snapshot| snapshot.sessions.iter())
            .map(|session| (session_key(session), session))
            .collect();
        let mut candidate = parent_key(previous);
        let mut seen = HashSet::new();
        while let Some(key) = candidate {
            if !seen.insert(key.clone()) {
                break;
            }
            if let Some(index) = rows.iter().position(|session| session_key(session) == key) {
                self.selected = index;
                return;
            }
            candidate = by_key.get(&key).and_then(|session| parent_key(session));
        }
        self.selected = self.selected.min(rows.len().saturating_sub(1));
    }

    pub fn toggle_branch(&mut self) {
        let Some((key, child_count)) = self
            .tree_rows()
            .get(self.selected)
            .map(|row| (session_key(row.session), row.visible_child_count))
        else {
            return;
        };
        if child_count == 0 {
            return;
        }
        if !self.collapsed.insert(key.clone()) {
            self.collapsed.remove(&key);
        }
    }

    pub fn collapse_or_parent(&mut self) {
        let Some((key, parent, child_count, collapsed)) =
            self.tree_rows().get(self.selected).map(|row| {
                (
                    session_key(row.session),
                    row.parent.map(session_key),
                    row.visible_child_count,
                    row.collapsed,
                )
            })
        else {
            return;
        };
        if child_count > 0 && !collapsed {
            self.collapsed.insert(key);
            return;
        }
        if let Some(parent) = parent
            && let Some(index) = self
                .tree_rows()
                .iter()
                .position(|row| session_key(row.session) == parent)
        {
            self.selected = index;
        }
    }

    pub fn expand_or_child(&mut self) {
        let Some((key, child_count, collapsed)) = self.tree_rows().get(self.selected).map(|row| {
            (
                session_key(row.session),
                row.visible_child_count,
                row.collapsed,
            )
        }) else {
            return;
        };
        if child_count == 0 {
            return;
        }
        if collapsed {
            self.collapsed.remove(&key);
            return;
        }
        if let Some(index) = self
            .tree_rows()
            .iter()
            .position(|row| row.parent.is_some_and(|parent| session_key(parent) == key))
        {
            self.selected = index;
        }
    }
}

fn session_key(session: &Session) -> (String, Provider, String) {
    (session.host.clone(), session.provider, session.id.clone())
}

fn parent_key(session: &Session) -> Option<(String, Provider, String)> {
    session
        .parent_id
        .as_ref()
        .map(|parent| (session.host.clone(), session.provider, parent.clone()))
}

#[allow(clippy::too_many_arguments)]
fn append_tree<'a>(
    session: &'a Session,
    parent: Option<&'a Session>,
    depth: usize,
    ancestor_has_more: Vec<bool>,
    is_last: bool,
    visible_children: &HashMap<(String, Provider, String), Vec<&'a Session>>,
    known_children: &HashMap<(String, Provider, String), Vec<&'a Session>>,
    collapsed_keys: &HashSet<(String, Provider, String)>,
    reveal_collapsed: bool,
    visited: &mut HashSet<(String, Provider, String)>,
    rows: &mut Vec<TreeRow<'a>>,
) {
    let key = session_key(session);
    if !visited.insert(key.clone()) {
        return;
    }
    let child_count = known_children.get(&key).map_or(0, Vec::len);
    let visible_child_count = visible_children.get(&key).map_or(0, Vec::len);
    let working_children = visible_children.get(&key).map_or(0, |children| {
        children
            .iter()
            .filter(|child| {
                child.pid.is_some()
                    && child.activity == Activity::Working
                    && child.confidence != Confidence::Unknown
            })
            .count()
    });
    let collapsed = !reveal_collapsed && visible_child_count > 0 && collapsed_keys.contains(&key);
    rows.push(TreeRow {
        session,
        parent,
        depth,
        ancestor_has_more: ancestor_has_more.clone(),
        is_last,
        child_count,
        visible_child_count,
        working_children,
        collapsed,
    });

    let Some(children) = visible_children.get(&key) else {
        return;
    };
    if collapsed {
        for child in children {
            mark_hidden_descendants(child, visible_children, visited);
        }
        return;
    }

    let mut child_ancestors = ancestor_has_more;
    if depth > 0 {
        child_ancestors.push(!is_last);
    }
    for (index, child) in children.iter().enumerate() {
        append_tree(
            child,
            Some(session),
            depth + 1,
            child_ancestors.clone(),
            index + 1 == children.len(),
            visible_children,
            known_children,
            collapsed_keys,
            reveal_collapsed,
            visited,
            rows,
        );
    }
}

fn mark_hidden_descendants(
    session: &Session,
    visible_children: &HashMap<(String, Provider, String), Vec<&Session>>,
    visited: &mut HashSet<(String, Provider, String)>,
) {
    let key = session_key(session);
    if !visited.insert(key.clone()) {
        return;
    }
    if let Some(children) = visible_children.get(&key) {
        for child in children {
            mark_hidden_descendants(child, visible_children, visited);
        }
    }
}

fn hierarchy_key(session: &Session, sessions: &[&Session]) -> (String, usize) {
    let mut current = session;
    let mut depth = 0;
    let mut seen = HashSet::new();

    while let Some(parent_id) = current.parent_id.as_deref() {
        if !seen.insert(current.id.as_str()) {
            return (session.id.clone(), 0);
        }
        let Some(parent) = sessions.iter().copied().find(|candidate| {
            candidate.host == session.host
                && candidate.provider == session.provider
                && candidate.id == parent_id
        }) else {
            return (parent_id.to_owned(), depth + 1);
        };
        current = parent;
        depth += 1;
    }

    (current.id.clone(), depth)
}

fn is_raw_headless(session: &Session) -> bool {
    session.id.contains("-pid-") && session.evidence.to_lowercase().contains("headless")
}

fn is_retained_child(session: &Session) -> bool {
    session.pid.is_some()
        && session.parent_id.is_some()
        && matches!(
            session.activity,
            Activity::Unknown | Activity::Idle | Activity::Ended
        )
}

fn is_background(session: &Session) -> bool {
    is_raw_headless(session) || is_retained_child(session)
}

// Presentation grouping only, not proof of a process/session relationship.
// Never copy its TTY, PID or navigation target onto the logical session.
fn is_auxiliary_process(session: &Session, all: &[&Session]) -> bool {
    is_raw_process(session)
        && session.cwd.is_some()
        && all.iter().any(|other| {
            !is_raw_process(other)
                && other.parent_id.is_none()
                && other.pid.is_some()
                && other.host == session.host
                && other.provider == session.provider
                && other.cwd == session.cwd
        })
}

fn is_raw_process(session: &Session) -> bool {
    session.id.contains("-pid-")
        && session
            .evidence
            .to_lowercase()
            .contains("session metadata unavailable")
}

fn role_label(session: &Session, child_count: usize) -> &'static str {
    if session.parent_id.is_some() {
        "Subagent"
    } else if child_count > 0 {
        "Parent"
    } else if matches!(session.target, Some(Target::Managed { .. })) {
        "Terminal"
    } else if is_raw_process(session) {
        "Process"
    } else if session.pid.is_none() {
        "History"
    } else {
        "Session"
    }
}

fn role_detail(session: &Session, child_count: usize) -> String {
    match role_label(session, child_count) {
        "Parent" => format!(
            "Parent · {child_count} known direct {}",
            if child_count == 1 {
                "child"
            } else {
                "children"
            }
        ),
        "Subagent" if child_count > 0 => format!(
            "Subagent · linked to parent session · {child_count} known direct {}",
            if child_count == 1 {
                "child"
            } else {
                "children"
            }
        ),
        "Subagent" => "Subagent · linked to parent session".to_owned(),
        "Process" => "Process · live PID without session metadata".to_owned(),
        "History" => "History · session metadata without a live PID".to_owned(),
        _ => "Session · no parent relationship reported".to_owned(),
    }
}

fn shares_parent_process(session: &Session, parent: Option<&Session>) -> bool {
    let Some(parent) = parent else {
        return false;
    };
    matches!(
        (
            session.pid,
            session.process_started_at,
            parent.pid,
            parent.process_started_at,
        ),
        (Some(pid), Some(started), Some(parent_pid), Some(parent_started))
            if pid == parent_pid && started == parent_started
    )
}

fn shares_parent_terminal(session: &Session, parent: Option<&Session>) -> bool {
    shares_parent_process(session, parent)
        && matches!(
            (
                session.tty.as_deref(),
                parent.and_then(|parent| parent.tty.as_deref())
            ),
            (Some(tty), Some(parent_tty)) if tty == parent_tty
        )
}

fn model_label(session: &Session) -> String {
    let Some(model) = session.model.as_deref() else {
        return "—".to_owned();
    };
    let model = sanitize(model, 100);
    match model.to_ascii_lowercase().as_str() {
        "gpt-6-astra" => "Astra".to_owned(),
        "gpt-5.6-sol" => "Sol".to_owned(),
        "gpt-5.6-terra" => "Terra".to_owned(),
        "gpt-5.6-luna" => "Luna".to_owned(),
        "" => "—".to_owned(),
        _ => sanitize(&model, 12),
    }
}

fn is_attention(session: &Session) -> bool {
    session.activity == Activity::WaitingInput && session.confidence == Confidence::Observed
}

fn state_rank(session: &Session) -> u8 {
    if is_attention(session) {
        return 0;
    }
    match session.activity {
        Activity::Working => 1,
        Activity::WaitingTool => 2,
        Activity::Idle => 3,
        Activity::Unknown => 4,
        Activity::Ended => 5,
        Activity::WaitingInput => 6,
    }
}

fn matches_query(session: &Session, query: &str) -> bool {
    let fields = [
        session.id.as_str(),
        session.host.as_str(),
        session.cwd.as_deref().unwrap_or_default(),
        session.tty.as_deref().unwrap_or_default(),
        session.model.as_deref().unwrap_or_default(),
        session.parent_id.as_deref().unwrap_or_default(),
        session.insights.title.as_deref().unwrap_or_default(),
        provider_label(&session.provider),
        state_label(session),
    ];
    fields
        .iter()
        .any(|value| value.to_lowercase().contains(query))
}

fn sanitize(value: &str, limit: usize) -> String {
    let mut cleaned = String::new();
    let mut previous_space = false;
    for character in value.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if character.is_whitespace() {
            if previous_space {
                continue;
            }
            previous_space = true;
            cleaned.push(' ');
        } else {
            previous_space = false;
            cleaned.push(character);
        }
        if cleaned.chars().count() >= limit {
            break;
        }
    }
    cleaned.trim().to_owned()
}

fn workspace(session: &Session) -> String {
    let Some(cwd) = session.cwd.as_deref() else {
        return "No workspace".to_owned();
    };
    let cwd = sanitize(cwd, 240);
    if cwd == "/" {
        return cwd;
    }
    Path::new(&cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(|name| sanitize(name, 80))
        .unwrap_or_else(|| "No workspace".to_owned())
}

fn provider_label(provider: &Provider) -> &'static str {
    provider.label()
}

fn state_label(session: &Session) -> &'static str {
    if let Some(observation) = &session.insights.activity_observation {
        if observation.source == "codex_app_server"
            && session.confidence == Confidence::Observed
            && session.pid.is_some()
        {
            return match observation.event.as_str() {
                "active" => "Working",
                "idle" => "Ready",
                "waiting_for_input" => "Needs input",
                "system_error" => "Error",
                _ => "Unknown",
            };
        }
        if session.activity == Activity::Idle {
            return match observation.event.as_str() {
                "turn_aborted" => "Interrupted",
                "SessionStart" => "Started",
                "Notification:idle_prompt" => "Prompt idle",
                _ => "Last reply",
            };
        }
        if observation.source == "claude_hook" && session.activity == Activity::WaitingTool {
            return "Tool started";
        }
    }
    if session.activity == Activity::Unknown || session.confidence == Confidence::Unknown {
        return "Unknown";
    }
    match (&session.activity, &session.confidence) {
        (Activity::Working, Confidence::Observed) => "Working",
        (Activity::Working, Confidence::Inferred) => "Active log",
        (Activity::WaitingInput, Confidence::Observed) => "Needs input",
        (Activity::WaitingInput, Confidence::Inferred) => "Input?",
        (Activity::WaitingTool, Confidence::Observed) => "Tool wait",
        (Activity::WaitingTool, Confidence::Inferred) => "Tool wait?",
        (Activity::Idle, Confidence::Observed) => "Idle",
        (Activity::Idle, Confidence::Inferred) => "Last reply",
        (Activity::Ended, Confidence::Observed) => "Ended",
        (Activity::Ended, Confidence::Inferred) => "Ended?",
        (_, Confidence::Unknown) | (Activity::Unknown, _) => "Unknown",
    }
}

fn activity_evidence(session: &Session) -> String {
    let Some(observation) = &session.insights.activity_observation else {
        return if session.pid.is_some() {
            "Host process is live; current activity is not available.".into()
        } else {
            "History only; current activity is not available.".into()
        };
    };
    let age = chrono::Utc::now()
        .timestamp()
        .saturating_sub(observation.observed_at);
    let age = if age < 0 {
        "clock differs".into()
    } else if age < 60 {
        format!("{age}s ago")
    } else if age < 3600 {
        format!("{}m ago", age / 60)
    } else {
        format!("{}h ago", age / 3600)
    };
    let source = match observation.source.as_str() {
        "codex_app_server" => "Codex live server",
        "claude_hook" => "Claude hook",
        _ => "Log event",
    };
    format!("{source}: {} · {age}", sanitize(&observation.event, 80))
}

fn confidence_label(confidence: &Confidence) -> &'static str {
    match confidence {
        Confidence::Observed => "observed",
        Confidence::Inferred => "inferred",
        Confidence::Unknown => "unverified",
    }
}

fn state_style(session: &Session) -> Style {
    if is_attention(session) {
        return Style::default().fg(CORAL).add_modifier(Modifier::BOLD);
    }
    match session.activity {
        Activity::Working => Style::default().fg(TEAL),
        Activity::WaitingTool => Style::default().fg(AMBER),
        Activity::Idle => Style::default().fg(SLATE),
        Activity::Ended | Activity::Unknown | Activity::WaitingInput => Style::default().fg(SLATE),
    }
}

fn terminal_label(session: &Session) -> String {
    match session.target.as_ref() {
        Some(Target::Managed { .. }) => "TTYbird".to_owned(),
        Some(Target::Ghostty { .. }) => "Ghostty binding".to_owned(),
        Some(Target::Tmux { pane, .. }) => format!("tmux {}", sanitize(pane, 16)),
        None => session
            .tty
            .as_deref()
            .and_then(|tty| Path::new(tty).file_name())
            .and_then(|name| name.to_str())
            .map(|name| sanitize(name, 24))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "—".to_owned()),
    }
}

fn short_session_suffix(id: &str) -> String {
    let id = sanitize(id, 220);
    let characters: Vec<_> = id.chars().collect();
    if characters.len() > 10 {
        format!(
            "…{}",
            characters[characters.len() - 8..]
                .iter()
                .collect::<String>()
        )
    } else {
        id
    }
}

fn tree_workspace_label(row: &TreeRow<'_>) -> String {
    let mut label = String::new();
    for has_more in &row.ancestor_has_more {
        label.push_str(if *has_more { "│  " } else { "   " });
    }
    if row.depth > 0 {
        label.push_str(if row.is_last { "└─ " } else { "├─ " });
    }

    if row.visible_child_count > 0 {
        let marker = if row.collapsed { '▸' } else { '▾' };
        let noun = if row.visible_child_count == 1 {
            "child"
        } else {
            "children"
        };
        if row.session.parent_id.is_some() {
            let workspace_changed = row
                .parent
                .is_none_or(|parent| parent.cwd != row.session.cwd);
            label.push(marker);
            label.push(' ');
            if row.depth == 0 || workspace_changed {
                label.push_str(&workspace(row.session));
                label.push_str("  ");
            }
            label.push('[');
            label.push_str(&short_session_suffix(&row.session.id));
            label.push_str(&format!(
                "]  ({} shown {noun}; {} working?)",
                row.visible_child_count, row.working_children
            ));
        } else {
            label.push_str(&format!(
                "{marker} {}  ({} shown {noun}; {} working?)",
                session_title(row.session),
                row.visible_child_count,
                row.working_children
            ));
        }
    } else if row.session.parent_id.is_some() {
        let workspace_changed = row
            .parent
            .is_none_or(|parent| parent.cwd != row.session.cwd);
        if row.depth == 0 || workspace_changed {
            label.push_str(&workspace(row.session));
            label.push_str("  ");
        }
        if let Some(title) = row.session.insights.title.as_deref() {
            label.push_str(&sanitize(title, 100));
        } else {
            label.push('[');
            label.push_str(&short_session_suffix(&row.session.id));
            label.push(']');
        }
    } else {
        label.push_str(&session_title(row.session));
    }
    label
}

fn session_title(session: &Session) -> String {
    session
        .insights
        .title
        .as_deref()
        .map(|title| format!("{} · {}", sanitize(title, 100), workspace(session)))
        .unwrap_or_else(|| workspace(session))
}

fn token_label(session: &Session) -> String {
    let Some(usage) = session.insights.usage.as_ref() else {
        return "—".into();
    };
    let n = usage.total_tokens;
    let number = if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.)
    } else {
        n.to_string()
    };
    if usage.scope == crate::model::TokenUsageScope::Sampled {
        format!("{number}*")
    } else {
        number
    }
}

fn evidence_summary(session: &Session) -> &'static str {
    let evidence = session.evidence.to_lowercase();
    if evidence.contains("headless") {
        if session.target.is_some() {
            "Live headless host process; inherited terminal suppressed. The explicit navigation binding is checked on use."
        } else {
            "Live headless host process; inherited terminal suppressed and child activity is not established."
        }
    } else if evidence.contains("unique writable descriptor") {
        "Process identity and its open session log were matched directly."
    } else if evidence.contains("historical") || session.pid.is_none() {
        "Historical session metadata; no live process is confirmed."
    } else if evidence.contains("hook") || evidence.contains("lifecycle") {
        "Status comes from recent lifecycle metadata."
    } else {
        match session.confidence {
            Confidence::Observed => "Current process evidence was observed directly.",
            Confidence::Inferred => "Activity is inferred from limited metadata.",
            Confidence::Unknown => "No current activity evidence is available.",
        }
    }
}

fn border_block<'a>(title: &'a str) -> Block<'a> {
    Block::default()
        .title(Span::styled(
            title,
            Style::default().fg(SLATE).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
}

fn render_header(frame: &mut Frame, area: Rect, app: &App, shown: usize) {
    let all: Vec<_> = app
        .snapshots
        .iter()
        .flat_map(|snapshot| snapshot.sessions.iter())
        .collect();
    let distinct_pids: HashSet<_> = all
        .iter()
        .filter_map(|session| session.pid.map(|pid| (&session.host, pid)))
        .collect();
    let live_ttys: HashSet<_> = all
        .iter()
        .filter(|s| s.pid.is_some())
        .filter_map(|s| s.tty.as_ref().map(|tty| (&s.host, tty)))
        .collect();
    let hidden_history = if app.show_history {
        0
    } else {
        all.iter().filter(|s| s.pid.is_none()).count()
    };
    let attention = all.iter().filter(|session| is_attention(session)).count();
    let hidden_retained = all
        .iter()
        .filter(|session| is_retained_child(session))
        .count();
    let hidden_raw = all
        .iter()
        .filter(|session| {
            (is_raw_headless(session) || is_auxiliary_process(session, &all))
                && !is_retained_child(session)
        })
        .count();

    let mut summary = format!(
        "{shown} shown / {} entries  ·  {} live PIDs / {} host TTYs  ·  {attention} need attention",
        all.len(),
        distinct_pids.len(),
        live_ttys.len()
    );
    let unread = app
        .attention
        .iter()
        .filter(|item| item.read_at.is_none() && !item.is_snoozed(chrono::Utc::now().timestamp()))
        .count();
    if unread > 0 {
        summary.push_str(&format!("  ·  {unread} new events (N)"));
    }
    if app.liveness_unavailable {
        summary = format!(
            "Liveness unavailable · {} retained records · retrying collection",
            all.len()
        );
    }
    if hidden_history > 0 {
        summary.push_str(&format!("  ·  {hidden_history} history hidden (h)"));
    }
    if !app.show_background && hidden_retained > 0 {
        summary.push_str(&format!(
            "  ·  {hidden_retained} retained children hidden (b)"
        ));
    }
    if !app.show_background && hidden_raw > 0 {
        summary.push_str(&format!("  ·  {hidden_raw} auxiliary processes (b)"));
    }
    if app.refreshing {
        summary.push_str("  ·  refreshing…");
    }

    let line = Line::from(vec![
        Span::styled(
            " TTYbird ",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ),
        Span::styled(summary, Style::default().fg(SLATE)),
    ]);
    frame.render_widget(
        Paragraph::new(line)
            .block(border_block("Your coding agents"))
            .alignment(Alignment::Left),
        area,
    );
}

fn first_warning(app: &App) -> Option<String> {
    app.notice
        .as_deref()
        .map(|notice| sanitize(notice, 220))
        .or_else(|| {
            app.snapshots
                .iter()
                .flat_map(|snapshot| snapshot.warnings.iter())
                .next()
                .map(|warning| sanitize(warning, 220))
        })
}

fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    let line = if app.searching || (!app.query.is_empty() && first_warning(app).is_none()) {
        Line::from(vec![
            Span::styled(" Search › ", Style::default().fg(TEAL)),
            Span::styled(sanitize(&app.query, 160), Style::default().fg(Color::White)),
            Span::styled(
                if app.searching { "▌" } else { "" },
                Style::default().fg(TEAL),
            ),
            Span::styled("   Esc clears", Style::default().fg(SLATE)),
        ])
    } else if let Some(warning) = first_warning(app) {
        Line::from(vec![
            Span::styled(" Notice  ", Style::default().fg(AMBER)),
            Span::styled(warning, Style::default().fg(SLATE)),
        ])
    } else {
        let mut filters = Vec::new();
        if app.needs_only {
            filters.push("attention");
        }
        if app.live_only {
            filters.push("live PID");
        }
        if app.show_background {
            filters.push("retained/background visible");
        }
        let text = if filters.is_empty() {
            if let Some(session) = app.selected_session() {
                enter_hint(session, &app.snapshots)
            } else if area.width < 72 {
                "Space fold · ←/→ tree · Enter focus terminal".to_owned()
            } else {
                "Space folds a branch · ←/→ navigate tree · Enter focuses the mapped terminal"
                    .to_owned()
            }
        } else {
            format!("Filters: {}", filters.join(" + "))
        };
        Line::from(Span::styled(text, Style::default().fg(TEAL_DIM)))
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn empty_message(app: &App) -> String {
    if app.refreshing && app.snapshots.is_empty() {
        "Refreshing sessions…".to_owned()
    } else if app.snapshots.is_empty() {
        "Offline · no snapshots loaded. Press r to refresh.".to_owned()
    } else if let Some(warning) = first_warning(app) {
        format!("No sessions available. {warning}")
    } else if app.needs_only {
        "No observed sessions currently need input.".to_owned()
    } else if !app.query.trim().is_empty() {
        "No sessions match this search. Esc clears it.".to_owned()
    } else {
        "No sessions match the active filters.".to_owned()
    }
}

fn render_sessions(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    rows: &[TreeRow<'_>],
    multiple_hosts: bool,
) {
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(empty_message(app))
                .style(Style::default().fg(SLATE))
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true })
                .block(border_block("Sessions")),
            area,
        );
        return;
    }

    let compact = area.width < 70;
    let show_agent = area.width >= 52;
    let show_host = multiple_hosts && area.width >= 64;
    // Preserve room for branch labels and child counts before adding the TTY column.
    let show_terminal = area.width >= if show_host { 122 } else { 104 };

    let show_tokens = area.width >= 62;
    let mut headings = vec!["Session / workspace"];
    if show_agent {
        headings.push("Agent");
    }
    headings.extend(["Role", "Model", "State"]);
    if show_tokens {
        headings.push("Tokens");
    }
    if show_terminal {
        headings.push("Host TTY");
    }
    if show_host {
        headings.push("Host");
    }
    let header = Row::new(headings)
        .style(Style::default().fg(TEAL_DIM).add_modifier(Modifier::BOLD))
        .bottom_margin(1);

    let table_rows = rows.iter().map(|row| {
        let session = row.session;
        let parent_style = Style::default().fg(TEAL).add_modifier(Modifier::BOLD);
        let mut cells = vec![
            Cell::from(tree_workspace_label(row)).style(if row.child_count > 0 {
                parent_style
            } else {
                Style::default()
            }),
        ];
        if show_agent {
            cells.push(Cell::from(provider_label(&session.provider)));
        }
        cells.extend([
            Cell::from(role_label(session, row.child_count)).style(if row.child_count > 0 {
                parent_style
            } else {
                Style::default()
            }),
            Cell::from(model_label(session)),
            Cell::from(state_label(session)).style(state_style(session)),
        ]);
        if show_tokens {
            cells.push(Cell::from(token_label(session)));
        }
        if show_terminal {
            cells.push(Cell::from(terminal_label(session)));
        }
        if show_host {
            cells.push(Cell::from(sanitize(&session.host, 48)));
        }
        Row::new(cells)
    });

    let provider_width = rows
        .iter()
        .map(|row| provider_label(&row.session.provider).chars().count() as u16)
        .max()
        .unwrap_or(5)
        .clamp(5, 10);
    let mut widths = vec![Constraint::Min(if compact { 8 } else { 14 })];
    if show_agent {
        widths.push(Constraint::Length(provider_width));
    }
    widths.extend([
        Constraint::Length(8),
        Constraint::Length(if compact { 7 } else { 10 }),
        Constraint::Length(if compact { 10 } else { 12 }),
    ]);
    if show_tokens {
        widths.push(Constraint::Length(8));
    }
    if show_terminal {
        widths.push(Constraint::Length(if compact { 8 } else { 15 }));
    }
    if show_host {
        widths.push(Constraint::Length(if compact { 8 } else { 14 }));
    }

    let table = Table::new(table_rows, widths)
        .header(header)
        .column_spacing(1)
        .block(border_block("Sessions"))
        .row_highlight_style(
            Style::default()
                .fg(Color::White)
                .bg(SLATE_DARK)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    let mut state = TableState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn detail_lines(session: Option<&Session>, snapshots: &[Snapshot]) -> Vec<Line<'static>> {
    let Some(session) = session else {
        return vec![Line::from(Span::styled(
            "Select a session to inspect its verified navigation target.",
            Style::default().fg(SLATE),
        ))];
    };

    let value = |label: &'static str, value: String| {
        Line::from(vec![
            Span::styled(format!("{label:<15}"), Style::default().fg(TEAL_DIM)),
            Span::styled(value, Style::default().fg(Color::White)),
        ])
    };
    let pid = session
        .pid
        .map(|pid| {
            session
                .process_started_at
                .map(|started| format!("{pid} · started {started}"))
                .unwrap_or_else(|| pid.to_string())
        })
        .unwrap_or_else(|| "No live PID".to_owned());
    let navigation = match session.target.as_ref() {
        Some(Target::Managed { .. }) => {
            "Enter or i operates the right pane; Ctrl+] returns to the list".to_owned()
        }
        Some(Target::Ghostty { .. }) => {
            "Saved Ghostty binding; pane existence is checked on Enter".to_owned()
        }
        Some(Target::Tmux { pane, .. }) => {
            format!("Enter opens verified tmux pane {}", sanitize(pane, 16))
        }
        None if session.parent_id.is_some() => enter_hint(session, snapshots),
        None => "No bound terminal · Enter to choose a Ghostty pane on local macOS".to_owned(),
    };
    let parent = session.parent_id.as_deref().and_then(|parent_id| {
        snapshots
            .iter()
            .flat_map(|snapshot| snapshot.sessions.iter())
            .find(|candidate| {
                candidate.host == session.host
                    && candidate.provider == session.provider
                    && candidate.id == parent_id
            })
    });
    let key = session_key(session);
    let child_count = snapshots
        .iter()
        .flat_map(|snapshot| snapshot.sessions.iter())
        .filter(|candidate| parent_key(candidate).as_ref() == Some(&key))
        .count();
    let shares_parent_process = shares_parent_process(session, parent);
    let shares_parent_terminal = shares_parent_terminal(session, parent);
    let mut lines = vec![
        value("Workspace", sanitize(&workspace(session), 100)),
        value(
            "Directory",
            sanitize(session.cwd.as_deref().unwrap_or("Not reported"), 1000),
        ),
        value("Session ID", sanitize(&session.id, 220)),
        value("Role", role_detail(session, child_count)),
        value("State evidence", activity_evidence(session)),
        value(
            "Agent",
            format!(
                "{} · {} ({})",
                provider_label(&session.provider),
                state_label(session),
                confidence_label(&session.confidence)
            ),
        ),
        value(
            "Recorded model",
            sanitize(session.model.as_deref().unwrap_or("Not reported"), 100),
        ),
        value("Host", sanitize(&session.host, 100)),
        value(
            if shares_parent_process {
                "Host PID"
            } else {
                "PID"
            },
            pid,
        ),
        value(
            "Parent",
            sanitize(session.parent_id.as_deref().unwrap_or("None reported"), 220),
        ),
    ];
    if session.parent_id.is_some() {
        lines.push(value(
            "Parent model",
            sanitize(
                parent
                    .and_then(|parent| parent.model.as_deref())
                    .unwrap_or("Not reported"),
                100,
            ),
        ));
    }
    if session.pid.is_none() {
        lines.push(value(
            "Liveness",
            "Log-only history; no currently verified hosting process".into(),
        ));
    } else if session.parent_id.is_some() {
        lines.push(value(
            "Liveness",
            "Host process observed; an open child log does not prove a running child turn".into(),
        ));
    }
    if shares_parent_process {
        lines.push(value(
            "Hosting",
            "Shared parent process (PID/start match)".to_owned(),
        ));
        lines.push(value(
            "Terminal link",
            if shares_parent_terminal {
                "Shared parent kernel TTY".to_owned()
            } else {
                "No shared kernel TTY verified".to_owned()
            },
        ));
    }
    let tty_proof = if session.tty.is_some() {
        "Current kernel TTY assignment; does not prove an open GUI pane"
    } else {
        "No kernel TTY assigned, or an inherited TTY was intentionally suppressed"
    };
    lines.extend([
        value("Host terminal", terminal_label(session)),
        value("TTY proof", tty_proof.into()),
        value("Evidence", evidence_summary(session).to_owned()),
        value("Source", sanitize(&session.evidence, 1200)),
        value("Navigation", navigation),
    ]);
    lines
}

fn render_details(
    frame: &mut Frame,
    area: Rect,
    selected: Option<&Session>,
    snapshots: &[Snapshot],
) {
    let lines = if let Some(session) = selected {
        let mut lines = vec![
            Line::styled(
                session_title(session),
                Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
            ),
            Line::from(""),
            Line::from(format!(
                "{} · {} · {}",
                provider_label(&session.provider),
                model_label(session),
                state_label(session)
            )),
            Line::from(""),
        ];
        lines.push(Line::from(activity_evidence(session)));
        if session
            .insights
            .activity_observation
            .as_ref()
            .is_none_or(|o| o.source != "codex_app_server")
        {
            lines.push(Line::from(
                "Log/hook evidence describes the last event, not a live status query.",
            ));
        }
        if let Some(usage) = &session.insights.usage {
            let scope = if usage.scope == crate::model::TokenUsageScope::Sampled {
                "sampled messages only"
            } else {
                "reported session total"
            };
            lines.extend([
                Line::from(format!("Tokens  {} ({scope})", usage.total_tokens)),
                Line::from(format!(
                    "Input   {}  · cached {}",
                    usage.input_tokens,
                    usage
                        .cached_input_tokens
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "not reported".into())
                )),
                Line::from(format!(
                    "Output  {}  · reasoning {}",
                    usage.output_tokens,
                    usage
                        .reasoning_output_tokens
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "not reported".into())
                )),
                Line::from("Cache/reasoning are included, not added again."),
                Line::from("This session only; not context size or a billed cost."),
            ]);
        } else {
            lines.push(Line::from(
                "Token usage not reported in the sampled metadata.",
            ));
        }
        if let Some(git) = &session.insights.workspace {
            lines.push(Line::from(format!(
                "Branch  {} · {}",
                sanitize(git.branch.as_deref().unwrap_or("detached HEAD"), 120),
                if git.dirty {
                    "changes present"
                } else {
                    "clean"
                }
            )));
            lines.push(Line::from(format!(
                "Checkout  {}",
                sanitize(&git.checkout_root.to_string_lossy(), 240)
            )));
            if git.linked_worktree {
                lines.push(Line::from("Separate Git worktree"));
            }
        }
        if let Some(sharing) = &session.insights.sharing {
            lines.push(Line::from(Span::styled(
                sanitize(sharing, 300),
                Style::default().fg(AMBER),
            )));
        }
        lines.extend([
            Line::from(""),
            Line::from(format!("Workspace  {}", workspace(session))),
            Line::from(format!("Terminal   {}", terminal_label(session))),
            Line::from(""),
            Line::from("c  Recent conversation (local, opt-in)"),
            Line::from("p  Read-only terminal preview"),
            Line::from("d  Process identity and evidence"),
            Line::from("H  Prepare a reviewed handoff → Codex"),
            Line::from("N  Attention inbox · read / acknowledge / snooze"),
            Line::from("g  Relink the Ghostty terminal (also for a child's parent)"),
            Line::from(enter_hint(session, snapshots)),
        ]);
        lines
    } else {
        vec![Line::from("Select a session.")]
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(border_block("Session"))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_conversation(frame: &mut Frame, area: Rect, app: &App) {
    let content = app
        .conversation_text
        .as_deref()
        .unwrap_or("Reading recent local messages…");
    let content = content
        .lines()
        .take(120)
        .map(|line| sanitize(line, 2000))
        .collect::<Vec<_>>()
        .join("\n");
    frame.render_widget(
        Paragraph::new(content)
            .wrap(Wrap { trim: false })
            .scroll((app.preview_scroll, 0))
            .block(border_block("Recent conversation · local · c/Esc closes")),
        area,
    );
}

fn render_preview(frame: &mut Frame, area: Rect, app: &App) {
    let has_preview = app.preview_text.is_some();
    let content = app.preview_text.clone().unwrap_or_else(|| {
        let message = app
            .preview_notice
            .as_deref()
            .map(|notice| sanitize(notice, 500))
            .unwrap_or_else(|| "Loading terminal preview…".to_owned());
        Text::from(Line::from(Span::styled(
            message,
            Style::default().fg(SLATE),
        )))
    });
    let mut block = border_block("Terminal preview · read-only");
    if has_preview && let Some(notice) = app.preview_notice.as_deref() {
        block = block.title_bottom(
            Line::from(Span::styled(
                format!(" {} ", sanitize(notice, 120)),
                Style::default().fg(TEAL_DIM),
            ))
            .right_aligned(),
        );
    }
    let scroll = if has_preview {
        let visible_rows = usize::from(area.height.saturating_sub(2));
        let max_scroll = content
            .lines
            .len()
            .saturating_sub(visible_rows)
            .min(usize::from(u16::MAX)) as u16;
        app.preview_scroll.min(max_scroll)
    } else {
        0
    };
    let mut preview = Paragraph::new(content).block(block).scroll((scroll, 0));
    if !has_preview {
        preview = preview.wrap(Wrap { trim: true });
    }
    frame.render_widget(preview, area);
}

fn render_managed(frame: &mut Frame, area: Rect, app: &App) {
    let name = if app.managed_parent {
        "Parent terminal"
    } else {
        "Terminal"
    };
    let mode = if app.terminal_input {
        format!("{name} · INPUT · Ctrl+] returns to list")
    } else {
        format!("{name} · VIEW · Enter or i to type")
    };
    let context = app
        .selected_session()
        .map(|s| {
            format!(
                "{} · {} · {}",
                workspace(s),
                provider_label(&s.provider),
                s.insights
                    .workspace
                    .as_ref()
                    .and_then(|w| w.branch.as_deref())
                    .unwrap_or("branch unknown")
            )
        })
        .unwrap_or_default();
    let block = border_block(&mode).title_bottom(sanitize(&context, 200));
    let inner = block.inner(area);
    let content = app.managed_text.clone().unwrap_or_else(|| {
        Text::from(
            app.managed_notice
                .clone()
                .unwrap_or_else(|| "Connecting to owned terminal…".into()),
        )
    });
    frame.render_widget(Paragraph::new(content).block(block), area);
    if app.terminal_input
        && let Some((x, y)) = app.managed_cursor
        && x < inner.width
        && y < inner.height
    {
        frame.set_cursor_position((inner.x + x, inner.y + y));
    }
}

/// Matches the terminal panel layout below, excluding borders.
pub fn managed_size(area: Rect) -> (u16, u16) {
    let body_height = area.height.saturating_sub(5);
    let (width, height) = if area.width >= 105 {
        let pane = Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(Rect::new(0, 0, area.width, body_height))[1];
        (pane.width, pane.height)
    } else {
        let pane = Layout::vertical([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(Rect::new(0, 0, area.width, body_height))[1];
        (pane.width, pane.height)
    };
    (
        width.saturating_sub(2).clamp(2, 200),
        height.saturating_sub(2).clamp(2, 80),
    )
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let text = if app.managed_id.is_some() {
        if app.terminal_input {
            "INPUT → owned terminal · Ctrl+] returns to list · Ctrl-C goes to the program"
        } else {
            "↑↓/jk choose · Enter/i type in terminal · H handoff · N inbox · q detach · ? help"
        }
    } else if app.show_preview {
        if area.width < 72 {
            "Pg scroll · p close · ←/→ tree · Space fold"
        } else {
            "↑↓/jk choose   ←/→ tree   Space fold   PgUp/PgDn scroll   p close   d details   ? help"
        }
    } else if area.width < 72 {
        "j/k · ←/→ tree · Space fold · Enter focus · ?"
    } else {
        "↑↓/jk move   Space fold   Enter focus   c conversation   p preview   H handoff   N inbox   / search   ?"
    };
    frame.render_widget(
        Paragraph::new(Span::styled(text, Style::default().fg(SLATE))).alignment(Alignment::Center),
        area,
    );
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn render_help(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(if area.width < 65 { 92 } else { 62 }, 72, area);
    frame.render_widget(Clear, popup);
    let help = vec![
        Line::from(Span::styled(
            "TTYbird keeps activity claims separate from navigation bindings.",
            Style::default().fg(TEAL),
        )),
        Line::from(""),
        Line::from("↑ ↓ or j k   Move selection"),
        Line::from("← →          Collapse/parent · expand/first child"),
        Line::from("Space        Toggle the selected branch"),
        Line::from("h            Show/hide log-only history (not live sessions)"),
        Line::from("Enter        Open its terminal; children without one use their parent"),
        Line::from("Enter / i    Owned terminal: enter INPUT mode in the right pane"),
        Line::from("Ctrl+]       Leave INPUT mode; q in the list detaches"),
        Line::from("H / N        Reviewed handoff / attention inbox"),
        Line::from("g            Relink the Ghostty terminal (also for a child's parent)"),
        Line::from("p            Read-only terminal preview; PageUp/PageDown scroll"),
        Line::from("d            Full details; arrows/PageUp/PageDown scroll"),
        Line::from("/            Search sessions"),
        Line::from("a            Observed needs-input sessions only"),
        Line::from("b            Show retained children and raw headless processes"),
        Line::from("c            Recent local user/assistant messages (opt-in, memory only)"),
        Line::from("r            Refresh local and registered hosts"),
        Line::from("Esc          Clear search or close preview/details/help"),
        Line::from("q            Quit"),
    ];
    frame.render_widget(
        Paragraph::new(help)
            .block(border_block("Help  ?"))
            .style(Style::default().fg(Color::White))
            .wrap(Wrap { trim: true }),
        popup,
    );
}

fn render_ghostty_picker(frame: &mut Frame, area: Rect, picker: &GhosttyPicker) {
    let popup = centered_rect(
        if area.width < 70 { 96 } else { 84 },
        if area.height < 18 { 94 } else { 76 },
        area,
    );
    frame.render_widget(Clear, popup);
    let heading = format!(
        "Choose Ghostty pane for [{}]",
        sanitize(&workspace(&picker.session), 48)
    );
    let block = border_block(&heading);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(inner);
    let pid = picker
        .session
        .pid
        .map(|pid| {
            picker
                .session
                .process_started_at
                .map(|started| format!("PID {pid} · started {started}"))
                .unwrap_or_else(|| format!("PID {pid}"))
        })
        .unwrap_or_else(|| "no live PID".to_owned());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" Session ", Style::default().fg(TEAL_DIM)),
            Span::styled(
                short_session_suffix(&picker.session.id),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                format!(" · {pid} · {}", sanitize(&picker.session.host, 48)),
                Style::default().fg(SLATE),
            ),
        ])),
        layout[0],
    );

    if picker.terminals.is_empty() {
        frame.render_widget(
            Paragraph::new("No Ghostty panes are available to choose.")
                .alignment(Alignment::Center)
                .style(Style::default().fg(SLATE))
                .wrap(Wrap { trim: true }),
            layout[1],
        );
    } else {
        let header = Row::new(["Title", "CWD", "Pane ID"])
            .style(Style::default().fg(TEAL_DIM).add_modifier(Modifier::BOLD))
            .bottom_margin(1);
        let rows = picker.terminals.iter().map(|terminal| {
            Row::new([
                Cell::from({
                    let title = sanitize(&terminal.title, 100);
                    if title.is_empty() {
                        "Untitled".to_owned()
                    } else {
                        title
                    }
                }),
                Cell::from(sanitize(&terminal.cwd, 240)),
                Cell::from(short_session_suffix(&terminal.id)),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Percentage(30),
                Constraint::Percentage(50),
                Constraint::Length(10),
            ],
        )
        .header(header)
        .column_spacing(1)
        .row_highlight_style(
            Style::default()
                .fg(Color::White)
                .bg(SLATE_DARK)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
        let selected = picker
            .selected
            .min(picker.terminals.len().saturating_sub(1));
        let mut state = TableState::default().with_selected(Some(selected));
        frame.render_stateful_widget(table, layout[1], &mut state);
    }

    frame.render_widget(
        Paragraph::new("Enter binds this session and focuses the chosen pane · Esc cancels")
            .alignment(Alignment::Center)
            .style(Style::default().fg(TEAL))
            .wrap(Wrap { trim: true }),
        layout[2],
    );
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }

    let count = app.tree_rows().len();
    app.selected = app.selected.min(count.saturating_sub(1));
    let rows = app.tree_rows();
    let multiple_hosts = app
        .snapshots
        .iter()
        .flat_map(|snapshot| snapshot.sessions.iter().map(|session| &session.host))
        .collect::<HashSet<_>>()
        .len()
        > 1;

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    render_header(frame, layout[0], app, rows.len());
    render_status_bar(frame, layout[1], app);

    if layout[2].height < 6 {
        render_sessions(frame, layout[2], app, &rows, multiple_hosts);
    } else if area.width >= 105 {
        let constraints = if app.managed_id.is_some() {
            [Constraint::Percentage(30), Constraint::Percentage(70)]
        } else if app.show_preview {
            [Constraint::Percentage(40), Constraint::Percentage(60)]
        } else {
            [Constraint::Percentage(60), Constraint::Percentage(40)]
        };
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(constraints)
            .split(layout[2]);
        render_sessions(frame, body[0], app, &rows, multiple_hosts);
        if app.managed_id.is_some() && !app.show_conversation {
            render_managed(frame, body[1], app);
        } else if app.show_conversation {
            render_conversation(frame, body[1], app);
        } else if app.show_preview {
            render_preview(frame, body[1], app);
        } else {
            render_details(
                frame,
                body[1],
                rows.get(app.selected).map(|row| row.session),
                &app.snapshots,
            );
        }
    } else {
        let constraints = if app.managed_id.is_some() {
            [Constraint::Percentage(30), Constraint::Percentage(70)]
        } else if app.show_preview {
            [Constraint::Percentage(40), Constraint::Percentage(60)]
        } else {
            [Constraint::Percentage(60), Constraint::Percentage(40)]
        };
        let body = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(layout[2]);
        render_sessions(frame, body[0], app, &rows, multiple_hosts);
        if app.managed_id.is_some() && !app.show_conversation {
            render_managed(frame, body[1], app);
        } else if app.show_conversation {
            render_conversation(frame, body[1], app);
        } else if app.show_preview {
            render_preview(frame, body[1], app);
        } else {
            render_details(
                frame,
                body[1],
                rows.get(app.selected).map(|row| row.session),
                &app.snapshots,
            );
        }
    }
    render_footer(frame, layout[3], app);

    if app.show_details {
        let popup = centered_rect(94, 90, area);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(detail_lines(
                rows.get(app.selected).map(|row| row.session),
                &app.snapshots,
            ))
            .block(border_block("Details · arrows scroll · Esc closes"))
            .wrap(Wrap { trim: true })
            .scroll((app.detail_scroll, 0)),
            popup,
        );
    }

    if app.show_help {
        render_help(frame, area);
    }
    if let Some(picker) = app.ghostty_picker.as_ref() {
        render_ghostty_picker(frame, area, picker);
    }
    if app.show_inbox {
        render_inbox(frame, area, app);
    }
    if let Some(view) = &app.handoff {
        view.draw(frame, area, app.notice.as_deref());
    }
}

fn render_inbox(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(94, 86, area);
    frame.render_widget(Clear, popup);
    let now = chrono::Utc::now().timestamp();
    let rows: Vec<_> = app
        .attention
        .iter()
        .map(|item| {
            let label = if now.saturating_sub(item.observed_at) > 300 {
                format!(
                    "Reminder ({}m ago)",
                    now.saturating_sub(item.observed_at) / 60
                )
            } else {
                item.kind.label().to_owned()
            };
            Row::new(vec![
                label,
                sanitize(item.title.as_deref().unwrap_or(&item.session_id), 80),
                item.provider.label().to_owned(),
                if item.is_snoozed(now) {
                    "Snoozed".into()
                } else if item.read_at.is_some() {
                    "Read".into()
                } else {
                    "New".into()
                },
            ])
        })
        .collect();
    let block = border_block(
        "Attention · observed events only; a finished response is not a verified task",
    )
    .title_bottom("↑↓ select · Enter terminal · m acknowledge · z snooze 10m · N/Esc close");
    if rows.is_empty() {
        frame.render_widget(Paragraph::new("No open observed events. Claude hooks provide evidence; unavailable provider states are not inferred.").block(block).wrap(Wrap{trim:true}),popup);
    } else {
        let table = Table::new(
            rows,
            [
                Constraint::Length(22),
                Constraint::Min(10),
                Constraint::Length(9),
                Constraint::Length(8),
            ],
        )
        .block(block)
        .row_highlight_style(Style::default().bg(SLATE_DARK))
        .highlight_symbol("› ");
        let mut state = TableState::default()
            .with_selected(Some(app.inbox_selected.min(app.attention.len() - 1)));
        frame.render_stateful_widget(table, popup, &mut state);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use super::*;

    fn session(id: &str, workspace_name: &str, activity: Activity) -> Session {
        Session {
            id: id.to_owned(),
            provider: Provider::Codex,
            parent_id: None,
            host: "local".to_owned(),
            pid: Some(42),
            process_started_at: Some(100),
            tty: Some("/dev/ttys007".to_owned()),
            cwd: Some(format!("/work/{workspace_name}")),
            model: Some("gpt-test".to_owned()),
            insights: Default::default(),
            activity,
            confidence: Confidence::Observed,
            evidence: "same-user process observed".to_owned(),
            updated_at: Some(200),
            target: Some(Target::Tmux {
                socket: Some("test".to_owned()),
                pane: "%1".to_owned(),
            }),
        }
    }

    fn snapshot(sessions: Vec<Session>) -> Snapshot {
        Snapshot {
            protocol_version: 1,
            host: "local".to_owned(),
            collected_at: 200,
            sessions,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn runtime_idle_and_recorded_reply_are_distinct_and_failed_refresh_is_unknown() {
        let mut s = session("status", "ttybird", Activity::Idle);
        s.confidence = Confidence::Inferred;
        s.insights.activity_observation = Some(crate::model::ActivityObservation {
            source: "codex_log".into(),
            event: "task_complete".into(),
            observed_at: 200,
        });
        assert_eq!(state_label(&s), "Last reply");
        s.confidence = Confidence::Observed;
        let observation = s.insights.activity_observation.as_mut().unwrap();
        observation.source = "codex_app_server".into();
        observation.event = "idle".into();
        assert_eq!(state_label(&s), "Ready");
        let mut app = App::default();
        app.set_snapshots(vec![snapshot(vec![s])]);
        app.invalidate_liveness();
        assert_eq!(state_label(&app.snapshots[0].sessions[0]), "Unknown");
    }

    #[test]
    fn enter_routes_children_to_recorded_parent_not_same_directory() {
        let parent = session("parent", "same", Activity::Working);
        let mut child = session("child", "same", Activity::Working);
        child.parent_id = Some("parent".into());
        child.target = None;
        let snapshots = vec![snapshot(vec![parent, child.clone()])];
        assert_eq!(navigation_session(&child, &snapshots).unwrap().id, "parent");
        assert!(enter_hint(&child, &snapshots).contains("parent terminal"));
        let mut managed_parent = snapshots[0].sessions[0].clone();
        managed_parent.target = Some(Target::Managed {
            session_id: "1234abcd".into(),
        });
        assert!(enter_hint(&child, &[snapshot(vec![managed_parent])]).contains("PARENT terminal"));
        child.parent_id = Some("absent".into());
        assert!(navigation_session(&child, &snapshots).is_none());
        child.parent_id = Some("child".into());
        let cycles = vec![snapshot(vec![child.clone()])];
        assert!(navigation_session(&child, &cycles).is_none());
        child.target = Some(Target::Managed {
            session_id: "1234abcd".into(),
        });
        assert_eq!(navigation_session(&child, &snapshots).unwrap().id, "child");
    }

    #[test]
    fn owned_terminal_view_and_input_are_distinct_and_keep_the_list() {
        let mut s = session("managed-example", "example", Activity::Unknown);
        s.target = Some(Target::Managed {
            session_id: "1234abcd".into(),
        });
        let mut app = App {
            managed_id: Some("1234abcd".into()),
            managed_text: Some(Text::from("SYNTHETIC TERMINAL")),
            ..Default::default()
        };
        app.set_snapshots(vec![snapshot(vec![s])]);
        let text = rendered(&mut app, 160, 30);
        assert!(text.contains("Sessions") && text.contains("SYNTHETIC TERMINAL"));
        assert!(text.contains("Terminal · VIEW"));
        app.terminal_input = true;
        let text = rendered(&mut app, 160, 30);
        assert!(text.contains("Terminal · INPUT") && text.contains("Ctrl+]"));
    }

    fn rendered(app: &mut App, width: u16, height: u16) -> String {
        let buffer = rendered_buffer(app, width, height);
        let mut text = String::new();
        for y in 0..height {
            for x in 0..width {
                text.push_str(buffer.cell((x, y)).unwrap().symbol());
            }
            text.push('\n');
        }
        text
    }

    fn rendered_buffer(app: &mut App, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn lines_text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_wide_medium_and_narrow_without_leaking_controls() {
        let mut malicious = session("codex-session", "checkout\u{1b}[2J", Activity::WaitingInput);
        malicious.host = "build-host".to_owned();
        let mut app = App::default();
        app.set_snapshots(vec![snapshot(vec![
            malicious,
            session("claude-session", "api", Activity::Unknown),
        ])]);

        for (width, height) in [(120, 32), (80, 24), (45, 12)] {
            let text = rendered(&mut app, width, height);
            assert!(text.contains("TTYbird"));
            assert!(text.contains("Session"));
            assert!(text.contains("Role"));
            assert!(text.contains("Model"));
            assert!(!text.contains('\u{1b}'));
        }
    }

    #[test]
    fn parent_and_subagent_keep_their_own_roles_and_models() {
        let mut parent = session("parent-session", "ttybird", Activity::Idle);
        parent.model = Some("gpt-6-astra".to_owned());
        let mut child = session("child-session", "ttybird", Activity::WaitingInput);
        child.parent_id = Some(parent.id.clone());
        child.model = Some("gpt-5.6-sol".to_owned());
        child.pid = parent.pid;
        child.process_started_at = parent.process_started_at;
        let unrelated = session("unrelated", "alpha", Activity::Working);

        let mut app = App::default();
        app.set_snapshots(vec![snapshot(vec![unrelated, child, parent])]);
        let rows = app.tree_rows();
        assert_eq!(rows[0].session.id, "parent-session");
        assert_eq!(role_label(rows[0].session, rows[0].child_count), "Parent");
        assert_eq!(model_label(rows[0].session), "Astra");
        assert_eq!(rows[1].session.id, "child-session");
        assert_eq!(role_label(rows[1].session, rows[1].child_count), "Subagent");
        assert_eq!(model_label(rows[1].session), "Sol");

        let rendered = rendered(&mut app, 180, 32);
        assert!(rendered.contains("Session"));
        assert!(rendered.contains("Subagent"));
        assert!(rendered.contains("Astra"));
        assert!(rendered.contains("Sol"));

        let child = app
            .rows()
            .into_iter()
            .find(|session| session.id == "child-session")
            .unwrap();
        let details = lines_text(&detail_lines(Some(child), &app.snapshots));
        assert!(details.contains("Role           Subagent · linked to parent session"));
        assert!(details.contains("Recorded model gpt-5.6-sol"));
        assert!(details.contains("Parent model   gpt-6-astra"));
        assert!(details.contains("Host PID       42 · started 100"));
        assert!(details.contains("Hosting        Shared parent process (PID/start match)"));
        assert!(details.contains("Terminal link  Shared parent kernel TTY"));

        let parent = app
            .rows()
            .into_iter()
            .find(|session| session.id == "parent-session")
            .unwrap();
        let parent_details = lines_text(&detail_lines(Some(parent), &app.snapshots));
        assert!(parent_details.contains("Recorded model gpt-6-astra"));
        assert!(!parent_details.contains("gpt-5.6-sol"));
        assert!(!parent_details.contains("Parent model"));

        let ordinary = session("ordinary-session", "workspace", Activity::Unknown);
        assert_eq!(role_label(&ordinary, 0), "Session");
        let mut raw = session("codex-pid-42-100", "workspace", Activity::Unknown);
        raw.model = None;
        raw.evidence = "same-user live codex process; session metadata unavailable".to_owned();
        assert_eq!(role_label(&raw, 0), "Process");
        assert_eq!(model_label(&raw), "—");
    }

    #[test]
    fn tree_rows_are_depth_first_and_disambiguate_nested_sessions() {
        let parent = session("parent-00000000", "ttybird", Activity::Working);
        let mut child = session("child-11111111", "ttybird", Activity::Working);
        child.parent_id = Some(parent.id.clone());
        let mut grandchild = session("grandchild-33333333", "ttybird", Activity::Working);
        grandchild.parent_id = Some(child.id.clone());
        let mut other_child = session("child-22222222", "z-worktree", Activity::Working);
        other_child.parent_id = Some(parent.id.clone());
        let unrelated = session("unrelated", "zzzz", Activity::Working);
        let mut app = App::default();
        app.set_snapshots(vec![snapshot(vec![
            unrelated,
            other_child,
            grandchild,
            child,
            parent,
        ])]);

        let rows = app.tree_rows();
        let ids: Vec<_> = rows.iter().map(|row| row.session.id.as_str()).collect();
        assert_eq!(
            &ids[..4],
            &[
                "parent-00000000",
                "child-11111111",
                "grandchild-33333333",
                "child-22222222",
            ]
        );
        assert_eq!(rows[0].child_count, 2);
        assert_eq!(role_label(rows[0].session, rows[0].child_count), "Parent");
        assert_eq!(role_label(rows[1].session, rows[1].child_count), "Subagent");
        assert_eq!(
            tree_workspace_label(&rows[0]),
            "▾ ttybird  (2 shown children; 2 working?)"
        );
        assert_eq!(
            tree_workspace_label(&rows[1]),
            "├─ ▾ […11111111]  (1 shown child; 1 working?)"
        );
        assert_eq!(tree_workspace_label(&rows[2]), "│  └─ […33333333]");
        assert_eq!(tree_workspace_label(&rows[3]), "└─ z-worktree  […22222222]");
    }

    #[test]
    fn branch_controls_persist_across_refresh_and_navigate_relations() {
        let parent = session("parent", "workspace", Activity::Working);
        let mut child = session("child", "workspace", Activity::Working);
        child.parent_id = Some(parent.id.clone());
        let refreshed = snapshot(vec![child.clone(), parent.clone()]);
        let mut app = App::default();
        app.set_snapshots(vec![snapshot(vec![child, parent])]);

        app.toggle_branch();
        assert_eq!(app.rows().len(), 1);
        assert_eq!(app.tree_rows()[0].visible_child_count, 1);
        assert!(app.collapsed.contains(&session_key(&refreshed.sessions[1])));
        app.set_snapshots(vec![refreshed]);
        assert_eq!(app.selected_session().unwrap().id, "parent");
        assert_eq!(app.rows().len(), 1);

        app.expand_or_child();
        assert_eq!(app.rows().len(), 2);
        assert_eq!(app.selected_session().unwrap().id, "parent");
        app.expand_or_child();
        assert_eq!(app.selected_session().unwrap().id, "child");
        app.collapse_or_parent();
        assert_eq!(app.selected_session().unwrap().id, "parent");

        let parent = session("parent", "workspace", Activity::Working);
        let mut child = session("child", "workspace", Activity::Working);
        child.parent_id = Some(parent.id.clone());
        let mut live_only = App {
            live_only: true,
            ..Default::default()
        };
        live_only.set_snapshots(vec![snapshot(vec![child, parent])]);
        live_only.toggle_branch();
        assert_eq!(live_only.rows().len(), 1);
        assert_eq!(live_only.selected_session().unwrap().id, "parent");
    }

    #[test]
    fn search_reveals_collapsed_child_and_restore_selects_visible_ancestor() {
        let parent = session("parent-a", "alpha", Activity::Working);
        let mut child = session("needle-child", "alpha", Activity::Working);
        child.parent_id = Some(parent.id.clone());
        let other = session("root-b", "beta", Activity::Working);
        let mut app = App::default();
        app.set_snapshots(vec![snapshot(vec![other, child, parent])]);
        app.toggle_branch();
        assert_eq!(app.rows().len(), 2);

        app.query = "needle-child".to_owned();
        app.move_selection(0);
        assert_eq!(app.selected_session().unwrap().id, "needle-child");
        let previous = app.selected_session().cloned();
        app.query.clear();
        app.restore_selection(previous.as_ref());
        assert_eq!(app.selected_session().unwrap().id, "parent-a");
    }

    #[test]
    fn linked_subagent_does_not_claim_shared_host_without_exact_identity() {
        let mut parent = session("parent-session", "workspace", Activity::Working);
        parent.model = Some("gpt-6-astra".to_owned());
        let mut child = session("child-session", "workspace", Activity::Working);
        child.parent_id = Some(parent.id.clone());
        child.model = Some("gpt-5.6-sol".to_owned());
        child.pid = Some(99);
        child.process_started_at = Some(101);
        child.tty = Some("/dev/ttys099".to_owned());

        let snapshots = vec![snapshot(vec![parent, child])];
        let child = snapshots[0]
            .sessions
            .iter()
            .find(|session| session.id == "child-session")
            .unwrap();
        let details = lines_text(&detail_lines(Some(child), &snapshots));
        assert!(details.contains("Role           Subagent · linked to parent session"));
        assert!(details.contains("PID            99 · started 101"));
        assert!(!details.contains("Host PID"));
        assert!(!details.contains("Hosting"));
        assert!(!details.contains("Shared parent process and terminal"));
    }

    #[test]
    fn hierarchy_cycles_terminate_and_parent_lookup_stays_on_host() {
        let mut first = session("first", "workspace", Activity::Working);
        first.parent_id = Some("second".to_owned());
        let mut second = session("second", "workspace", Activity::Working);
        second.parent_id = Some("first".to_owned());
        let mut app = App::default();
        app.set_snapshots(vec![snapshot(vec![first, second])]);
        assert_eq!(app.rows().len(), 2);

        let mut remote_parent = session("parent", "workspace", Activity::Working);
        remote_parent.host = "remote".to_owned();
        remote_parent.model = Some("gpt-6-astra".to_owned());
        let mut local_child = session("child", "workspace", Activity::Working);
        local_child.parent_id = Some("parent".to_owned());
        local_child.model = Some("gpt-5.6-sol".to_owned());
        let snapshots = vec![snapshot(vec![remote_parent, local_child])];
        let details = lines_text(&detail_lines(Some(&snapshots[0].sessions[1]), &snapshots));
        assert!(details.contains("Parent model   Not reported"));
        assert!(!details.contains("Parent model   gpt-6-astra"));
        assert!(!details.contains("Hosting"));
    }

    #[test]
    fn failed_refresh_does_not_keep_old_live_tty_claims() {
        let mut app = App::default();
        let live = snapshot(vec![session("live", "workspace", Activity::Working)]);
        app.set_snapshots(vec![live.clone()]);
        app.ghostty_picker = Some(GhosttyPicker {
            session: live.sessions[0].clone(),
            terminals: Vec::new(),
            selected: 0,
        });
        app.preview_text = Some(Text::from("stale screen"));
        app.invalidate_liveness();
        assert!(app.ghostty_picker.is_none());
        assert!(app.preview_text.is_none());
        assert!(app.snapshots[0].sessions[0].pid.is_none());
        assert!(app.snapshots[0].sessions[0].tty.is_none());
        assert!(rendered(&mut app, 180, 32).contains("Liveness unavailable"));
        app.set_snapshots(vec![live]);
        assert!(!app.liveness_unavailable);
        assert!(app.rows()[0].pid.is_some());
    }

    #[test]
    fn history_is_opt_in_and_never_counts_as_live_tty() {
        let mut historical = session("old", "old-workspace", Activity::Unknown);
        historical.pid = None;
        historical.process_started_at = None;
        historical.tty = None;
        let mut app = App::default();
        app.set_snapshots(vec![snapshot(vec![historical])]);
        assert!(app.rows().is_empty());
        assert!(rendered(&mut app, 180, 32).contains("1 history hidden"));
        app.show_history = true;
        assert_eq!(app.rows().len(), 1);
        assert!(rendered(&mut app, 180, 32).contains("0 live PIDs / 0 host TTYs"));
        assert_eq!(role_label(app.rows()[0], 0), "History");
    }

    #[test]
    fn selection_survives_reordered_snapshots_by_identity() {
        let first = session("first", "alpha", Activity::Working);
        let second = session("second", "beta", Activity::Idle);
        let mut app = App::default();
        app.set_snapshots(vec![snapshot(vec![first.clone(), second.clone()])]);
        app.selected = app
            .rows()
            .iter()
            .position(|candidate| candidate.id == "second")
            .unwrap();

        app.set_snapshots(vec![snapshot(vec![second, first])]);
        assert_eq!(app.selected_session().unwrap().id, "second");
    }

    #[test]
    fn hides_only_raw_headless_rows_and_attention_is_strictly_observed() {
        let mut raw = session("codex-pid-42-100", "raw", Activity::Working);
        raw.evidence = "same-user live codex headless process".to_owned();
        let mut child = session("child-thread", "child", Activity::WaitingInput);
        child.parent_id = Some("parent".to_owned());
        let mut inferred = session("inferred", "inferred", Activity::WaitingInput);
        inferred.confidence = Confidence::Inferred;

        let mut app = App::default();
        app.set_snapshots(vec![snapshot(vec![raw, child, inferred])]);
        assert_eq!(app.rows().len(), 2);
        assert!(
            app.rows()
                .iter()
                .any(|session| session.id == "child-thread")
        );

        app.needs_only = true;
        let rows = app.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "child-thread");

        app.show_background = true;
        assert_eq!(app.rows().len(), 1);
    }

    #[test]
    fn auxiliary_processes_are_grouped_without_transferring_navigation() {
        let mut named = session("named", "workspace", Activity::Unknown);
        named.pid = Some(50);
        named.target = None;
        let mut raw = session("codex-pid-42-100", "workspace", Activity::Unknown);
        raw.evidence = "live process; session metadata unavailable".into();
        let mut other = raw.clone();
        other.id = "codex-pid-43-100".into();
        other.pid = Some(43);
        other.cwd = Some("/work/different".into());
        let mut app = App::default();
        app.set_snapshots(vec![snapshot(vec![named, raw, other])]);
        assert_eq!(app.rows().len(), 2);
        assert!(
            app.rows()
                .iter()
                .any(|row| row.id == "named" && row.target.is_none())
        );
        assert!(app.rows().iter().any(|row| row.id == "codex-pid-43-100"));
        app.show_background = true;
        assert_eq!(app.rows().len(), 3);
    }

    #[test]
    fn titles_usage_and_opt_in_conversation_are_visible_without_identity_noise() {
        let mut named = session("named", "workspace", Activity::Working);
        named.insights.title = Some("Release checklist".into());
        named.insights.usage = Some(crate::model::TokenUsage {
            input_tokens: 1200,
            cached_input_tokens: Some(900),
            output_tokens: 300,
            reasoning_output_tokens: Some(100),
            total_tokens: 1500,
            scope: crate::model::TokenUsageScope::Sampled,
        });
        let mut app = App::default();
        app.set_snapshots(vec![snapshot(vec![named])]);
        let text = rendered(&mut app, 200, 34);
        assert!(text.contains("Release checklist"));
        assert!(text.contains("1.5k*"));
        assert!(text.contains("sampled messages only"));
        assert!(!text.contains("process started"));
        app.query = "checklist".into();
        assert_eq!(app.rows().len(), 1);
        app.show_conversation = true;
        app.conversation_text =
            Some("User: synthetic question\n\nAssistant: synthetic reply".into());
        assert!(rendered(&mut app, 200, 34).contains("synthetic reply"));
        app.invalidate_liveness();
        assert!(!app.show_conversation);
        assert!(app.conversation_text.is_none());
    }

    #[test]
    fn retained_children_are_hidden_by_default_and_counted() {
        let parent = session("parent", "workspace", Activity::Unknown);
        let child = |id, activity| {
            let mut child = session(id, "workspace", activity);
            child.parent_id = Some(parent.id.clone());
            child
        };
        let working = child("working", Activity::Working);
        let waiting = child("waiting", Activity::WaitingTool);
        let idle = child("idle", Activity::Idle);
        let ended = child("ended", Activity::Ended);
        let unknown = child("unknown", Activity::Unknown);
        let mut app = App::default();
        app.set_snapshots(vec![snapshot(vec![
            unknown, ended, idle, waiting, working, parent,
        ])]);

        let ids: HashSet<_> = app
            .rows()
            .iter()
            .map(|session| session.id.as_str())
            .collect();
        assert_eq!(ids, HashSet::from(["parent", "working", "waiting"]));
        assert_eq!(app.tree_rows()[0].child_count, 5);
        assert_eq!(app.tree_rows()[0].visible_child_count, 2);
        assert_eq!(app.tree_rows()[0].working_children, 1);
        assert!(tree_workspace_label(&app.tree_rows()[0]).contains("2 shown children; 1 working?"));
        assert!(rendered(&mut app, 200, 32).contains("3 retained children hidden (b)"));

        app.show_background = true;
        assert_eq!(app.rows().len(), 6);
        assert_eq!(app.tree_rows()[0].visible_child_count, 5);
        assert!(tree_workspace_label(&app.tree_rows()[0]).contains("5 shown children; 1 working?"));
    }

    #[test]
    fn retained_children_do_not_create_an_empty_expandable_branch() {
        let parent = session("parent", "workspace", Activity::Working);
        let child = |id, activity| {
            let mut child = session(id, "workspace", activity);
            child.parent_id = Some(parent.id.clone());
            child
        };
        let mut app = App::default();
        app.set_snapshots(vec![snapshot(vec![
            child("unknown", Activity::Unknown),
            child("ended", Activity::Ended),
            child("idle", Activity::Idle),
            parent,
        ])]);

        let rows = app.tree_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].child_count, 3);
        assert_eq!(rows[0].visible_child_count, 0);
        assert_eq!(tree_workspace_label(&rows[0]), "workspace");

        app.toggle_branch();
        assert_eq!(app.rows().len(), 1);
        assert!(app.collapsed.is_empty());

        app.show_background = true;
        let rows = app.tree_rows();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].visible_child_count, 3);
        assert_eq!(
            tree_workspace_label(&rows[0]),
            "▾ workspace  (3 shown children; 0 working?)"
        );
        app.toggle_branch();
        assert_eq!(app.rows().len(), 1);
    }

    #[test]
    fn waiting_children_remain_expandable_without_working_children() {
        let parent = session("parent", "workspace", Activity::Working);
        let mut waiting_input = session("input", "workspace", Activity::WaitingInput);
        waiting_input.parent_id = Some(parent.id.clone());
        let mut waiting_tool = session("tool", "workspace", Activity::WaitingTool);
        waiting_tool.parent_id = Some(parent.id.clone());
        let mut app = App::default();
        app.set_snapshots(vec![snapshot(vec![waiting_tool, waiting_input, parent])]);

        let rows = app.tree_rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].visible_child_count, 2);
        assert_eq!(rows[0].working_children, 0);
        assert_eq!(
            tree_workspace_label(&rows[0]),
            "▾ workspace  (2 shown children; 0 working?)"
        );

        app.toggle_branch();
        assert_eq!(app.rows().len(), 1);
        app.expand_or_child();
        assert_eq!(app.rows().len(), 3);
        assert_eq!(app.selected_session().unwrap().id, "parent");
        app.expand_or_child();
        assert_ne!(app.selected_session().unwrap().id, "parent");
    }

    #[test]
    fn headless_child_shares_host_without_claiming_a_tty_or_live_ghostty_pane() {
        let mut parent = session("parent", "workspace", Activity::Working);
        parent.tty = None;
        parent.target = None;
        let mut child = session("child", "workspace", Activity::Working);
        child.parent_id = Some(parent.id.clone());
        child.tty = None;
        child.evidence = "same-user headless process and log matched by a unique writable descriptor; inherited terminal target suppressed".to_owned();
        child.target = Some(Target::Ghostty {
            terminal_id: "12345678-1234-1234-1234-123456789abc".to_owned(),
        });
        let snapshots = vec![snapshot(vec![child, parent])];
        let child = &snapshots[0].sessions[0];
        let details = lines_text(&detail_lines(Some(child), &snapshots));

        assert!(details.contains("Host PID       42 · started 100"));
        assert!(details.contains("Hosting        Shared parent process (PID/start match)"));
        assert!(details.contains("Terminal link  No shared kernel TTY verified"));
        assert!(details.contains("Host terminal  Ghostty binding"));
        assert!(
            details.contains(
                "No kernel TTY assigned, or an inherited TTY was intentionally suppressed"
            )
        );
        assert!(details.contains("explicit navigation binding is checked on use"));
        assert!(details.contains("Saved Ghostty binding; pane existence is checked on Enter"));
    }

    #[test]
    fn empty_and_filtered_views_explain_the_state() {
        let mut app = App::default();
        assert!(rendered(&mut app, 45, 12).contains("Offline"));

        app.set_snapshots(vec![snapshot(vec![session(
            "idle",
            "workspace",
            Activity::Idle,
        )])]);
        app.needs_only = true;
        assert!(rendered(&mut app, 80, 24).contains("No observed sessions"));
    }

    #[test]
    fn details_and_preview_overlays_render_and_crop_on_narrow_terminals() {
        let mut s = session("long-session-id", "workspace", Activity::Unknown);
        s.cwd = Some("/work/team/project".into());
        let mut details = App {
            show_details: true,
            ..Default::default()
        };
        details.set_snapshots(vec![snapshot(vec![s])]);
        let text = rendered(&mut details, 80, 24);
        assert!(text.contains("/work/team/project"));
        assert!(text.contains("Source"));
        details.detail_scroll = 8;
        assert!(rendered(&mut details, 45, 12).contains("Details"));

        let mut app = App {
            show_preview: true,
            preview_text: Some(Text::from(vec![
                Line::from("first preview line"),
                Line::from("second preview line with a tail that must be cropped HIDDEN_TAIL"),
                Line::from("third preview line"),
                Line::from("fourth preview line"),
            ])),
            preview_scroll: 1,
            ..Default::default()
        };
        app.set_snapshots(vec![snapshot(vec![session(
            "preview-session",
            "workspace",
            Activity::Working,
        )])]);

        let text = rendered(&mut app, 45, 12);
        assert!(text.contains("Terminal preview"));
        assert!(text.contains("second preview"));
        assert!(!text.contains("first preview"));
        assert!(!text.contains("HIDDEN_TAIL"));
        assert!(!text.contains("Directory"));

        let mut unavailable = App {
            show_preview: true,
            preview_notice: Some(
                "Preview unavailable: the selected session has no readable tmux pane.".to_owned(),
            ),
            preview_scroll: 99,
            ..Default::default()
        };
        let text = rendered(&mut unavailable, 45, 14);
        assert!(text.contains("Preview unavailable"));
        assert!(text.contains("readable tmux pane"));
        assert!(text.contains("p close"));
    }

    #[test]
    fn preview_preserves_passed_terminal_styles() {
        let mut app = App {
            show_preview: true,
            preview_text: Some(Text::from(Line::from(Span::styled(
                "@ styled terminal cell",
                Style::default()
                    .fg(Color::Red)
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            )))),
            preview_notice: Some("Updated 12:34:56 · visible screen only".to_owned()),
            preview_scroll: 99,
            ..Default::default()
        };

        let buffer = rendered_buffer(&mut app, 120, 32);
        let rendered_text = rendered(&mut app, 120, 32);
        let cell = buffer
            .content()
            .iter()
            .find(|cell| cell.symbol() == "@")
            .expect("styled preview cell should be rendered");
        assert_eq!(cell.fg, Color::Red);
        assert_eq!(cell.bg, Color::Blue);
        assert!(cell.modifier.contains(Modifier::BOLD));
        assert!(rendered_text.contains("Updated 12:34:56 · visible screen only"));
    }

    #[test]
    fn ghostty_picker_renders_frozen_session_candidates_and_selection() {
        let mut attached = session("session-1234567890abcdef", "ttybird", Activity::Working);
        attached.pid = Some(1951);
        attached.process_started_at = Some(777);
        let mut app = App {
            ghostty_picker: Some(GhosttyPicker {
                session: attached,
                terminals: vec![
                    GhosttyTerminal {
                        id: "11111111-1111-1111-1111-111111111111".to_owned(),
                        cwd: "/work/other".to_owned(),
                        title: "Other pane".to_owned(),
                    },
                    GhosttyTerminal {
                        id: "22222222-2222-2222-2222-222222222222".to_owned(),
                        cwd: "/work/ttybird".to_owned(),
                        title: "@ selected\u{1b}[2J".to_owned(),
                    },
                ],
                selected: 1,
            }),
            ..Default::default()
        };

        let buffer = rendered_buffer(&mut app, 110, 30);
        let text = rendered(&mut app, 110, 30);
        assert!(text.contains("Choose Ghostty pane for [ttybird]"));
        assert!(text.contains("…90abcdef"));
        assert!(text.contains("PID 1951 · started 777"));
        assert!(text.contains("Other pane"));
        assert!(text.contains("/work/ttybird"));
        assert!(text.contains("…22222222"));
        assert!(text.contains("Enter binds this session and focuses the chosen pane"));
        assert!(text.contains("Esc cancels"));
        assert!(!text.contains('\u{1b}'));
        let selected_cell = buffer
            .content()
            .iter()
            .find(|cell| cell.symbol() == "@")
            .expect("selected terminal title should render");
        assert_eq!(selected_cell.bg, SLATE_DARK);

        let attached = session("session-id", "workspace", Activity::Unknown);
        let mut empty = App {
            ghostty_picker: Some(GhosttyPicker {
                session: attached,
                terminals: Vec::new(),
                selected: 99,
            }),
            ..Default::default()
        };

        let text = rendered(&mut empty, 45, 12);
        assert!(text.contains("Choose Ghostty pane"));
        assert!(text.contains("No Ghostty panes"));
        assert!(text.contains("Esc cancels"));
    }
}
