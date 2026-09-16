//! Read-only workspace enrichment shared by collection and the dashboard.
use crate::{model::Snapshot, workspace::Inspector};
use std::collections::HashMap;

pub fn enrich_workspaces(snapshot: &mut Snapshot) {
    let mut inspector = Inspector::new();
    let mut paths = HashMap::new();
    for session in &mut snapshot.sessions {
        if session.pid.is_none() {
            continue;
        }
        let Some(cwd) = session.cwd.as_deref() else {
            continue;
        };
        if !paths.contains_key(cwd) && paths.len() < 32 {
            paths.insert(
                cwd.to_owned(),
                inspector.inspect(std::path::Path::new(cwd)).ok(),
            );
        }
        session.insights.workspace = paths.get(cwd).cloned().flatten();
    }
    let roots = crate::workspace::live_root_counts(&snapshot.sessions);
    for session in &mut snapshot.sessions {
        session.insights.sharing = session
            .insights
            .workspace
            .as_ref()
            .and_then(|w| roots.get(&w.checkout_root))
            .filter(|count| **count > 1)
            .map(|count| {
                format!("Shared checkout: {count} live root sessions · editing is not verified")
            });
    }
}
