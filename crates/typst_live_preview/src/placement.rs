//! Where a new preview goes.
//!
//! The first preview opens in a pane of its own. After that a preview joins an
//! existing preview pane as a tab: one pane per preview eats the window, and
//! tabs keep the documents together.
//!
//! Pane focus history decides which pane to join, and `Workspace` keeps that
//! list private. This fork does not modify upstream files, so the same
//! information is rebuilt from public events instead: `Workspace::Event`'s
//! `PaneAdded` names every center pane, and `workspace::pane::Event::Focus`
//! fires whenever one gains focus.

use std::{
    collections::HashSet,
    sync::{Mutex, OnceLock, PoisonError},
};

use gpui::{App, Context, Entity, EntityId, WeakEntity};
use workspace::{Pane, Workspace, pane::Event as PaneEvent};

use crate::view::TypstPreviewView;

/// Center panes, most recently focused first.
static RECENT_PANES: OnceLock<Mutex<Vec<WeakEntity<Pane>>>> = OnceLock::new();
/// Panes already subscribed to, so a pane is watched once.
static WATCHED_PANES: OnceLock<Mutex<HashSet<EntityId>>> = OnceLock::new();

fn recent_panes() -> &'static Mutex<Vec<WeakEntity<Pane>>> {
    RECENT_PANES.get_or_init(|| Mutex::new(Vec::new()))
}

fn watched_panes() -> &'static Mutex<HashSet<EntityId>> {
    WATCHED_PANES.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Starts tracking focus for this workspace's center panes.
pub(crate) fn watch_workspace(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    for pane in workspace.panes() {
        watch_pane(pane, cx);
    }

    let workspace = cx.entity();
    cx.subscribe(&workspace, |_, _, event: &workspace::Event, cx| {
        if let workspace::Event::PaneAdded(pane) = event {
            watch_pane(pane, cx);
        }
    })
    .detach();
}

fn watch_pane(pane: &Entity<Pane>, cx: &mut Context<Workspace>) {
    {
        let mut watched = watched_panes()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !watched.insert(pane.entity_id()) {
            return;
        }
    }

    cx.subscribe(pane, |_, pane, event: &PaneEvent, _cx| {
        if matches!(event, PaneEvent::Focus) {
            record_focus(&pane);
        }
    })
    .detach();
}

fn record_focus(pane: &Entity<Pane>) {
    let mut recent = recent_panes()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    recent.retain(|candidate| {
        candidate
            .upgrade()
            .is_some_and(|candidate| candidate.entity_id() != pane.entity_id())
    });
    recent.insert(0, pane.downgrade());
}

fn recent_ids() -> Vec<EntityId> {
    recent_panes()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter_map(|pane| pane.upgrade())
        .map(|pane| pane.entity_id())
        .collect()
}

/// The pane a new preview should join, or `None` to open a fresh pane.
pub(crate) fn destination(workspace: &Workspace, cx: &App) -> Option<Entity<Pane>> {
    let previews: Vec<Entity<Pane>> = workspace
        .panes()
        .iter()
        .filter(|pane| contains_preview(pane, cx))
        .cloned()
        .collect();
    let preview_ids: Vec<EntityId> = previews.iter().map(|pane| pane.entity_id()).collect();

    let chosen = pick(
        &preview_ids,
        workspace.active_pane().entity_id(),
        &recent_ids(),
    )?;
    previews
        .into_iter()
        .find(|pane| pane.entity_id() == chosen)
}

/// The pure part of [`destination`], so the policy is testable without a window.
///
/// * no preview pane — open a new one;
/// * one preview pane — add a tab there;
/// * several — join the most recently focused one that is not the current pane.
pub(crate) fn pick<T: Copy + Eq>(
    preview_ids: &[T],
    current: T,
    recent: &[T],
) -> Option<T> {
    match preview_ids {
        [] => None,
        [only] => Some(*only),
        _ => recent
            .iter()
            .copied()
            .filter(|id| *id != current)
            .find(|id| preview_ids.contains(id))
            .or_else(|| preview_ids.iter().copied().find(|id| *id != current))
            .or_else(|| preview_ids.first().copied()),
    }
}

fn contains_preview(pane: &Entity<Pane>, cx: &App) -> bool {
    pane.read(cx)
        .items_of_type::<TypstPreviewView>()
        .next()
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::pick;

    fn id(value: usize) -> usize {
        value
    }

    #[test]
    fn no_preview_pane_opens_a_new_one() {
        assert_eq!(pick(&[], id(1), &[]), None);
    }

    #[test]
    fn a_single_preview_pane_takes_a_tab() {
        assert_eq!(pick(&[id(7)], id(1), &[]), Some(id(7)));
        // Even when it is the current pane.
        assert_eq!(pick(&[id(7)], id(7), &[]), Some(id(7)));
    }

    #[test]
    fn several_preview_panes_pick_the_most_recent_one_that_is_not_current() {
        let previews = [id(7), id(8), id(9)];
        let recent = [id(9), id(1), id(8), id(7)];

        // 9 is the most recent, and current, so 8 wins.
        assert_eq!(pick(&previews, id(9), &recent), Some(id(8)));
        // 9 is most recent and not current, so it wins.
        assert_eq!(pick(&previews, id(1), &recent), Some(id(9)));
    }

    #[test]
    fn unknown_focus_history_falls_back_to_a_preview_pane() {
        let previews = [id(7), id(8)];
        assert_eq!(pick(&previews, id(1), &[]), Some(id(7)));
        assert_eq!(pick(&previews, id(7), &[]), Some(id(8)));
        assert_eq!(pick(&previews, id(7), &[id(7)]), Some(id(8)));
    }
}
