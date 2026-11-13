use super::*;
use crate::common::config::VirtualWorkspaceSettings;
use crate::layout_engine::{ColumnId, ColumnPlacement, ColumnReorderRequest};

struct Fixture {
    engine: LayoutEngine,
    store: WindowStore,
    space: SpaceId,
    workspace: VirtualWorkspaceId,
}

impl Fixture {
    fn new() -> Self {
        let settings = LayoutSettings {
            mode: LayoutMode::Scrolling,
            ..Default::default()
        };
        let mut engine = LayoutEngine::new(&VirtualWorkspaceSettings::default(), &settings, None);
        let mut store = WindowStore::default();
        let space = SpaceId::new(901);
        let _ = engine.handle_event(
            &mut store,
            LayoutEvent::SpaceExposed(space, CGSize::new(1200.0, 800.0)),
        );
        let workspace = engine.workspaces.active_workspace(space).unwrap();
        for index in 1..=3 {
            let _ = engine.handle_event(
                &mut store,
                LayoutEvent::WindowAdded(space, WindowId::new(1, index)),
            );
        }
        Self {
            engine,
            store,
            space,
            workspace,
        }
    }

    fn request(&self) -> ColumnReorderRequest {
        let ws = &self.engine.workspaces[self.workspace];
        let tree = ws.layout_system.container_tree(ws.active_layout().unwrap());
        let source = &tree.children[0];
        let target = &tree.children[2];
        let members = |column: &rift_protocol::ContainerTreeNode| {
            column
                .children
                .iter()
                .map(|window| {
                    let window = window.window_id.unwrap();
                    WindowId::new(window.pid, window.idx)
                })
                .collect()
        };
        ColumnReorderRequest {
            space: self.space,
            workspace: self.workspace,
            source: ColumnId(source.node_id),
            target: ColumnId(target.node_id),
            source_members: members(source),
            target_members: members(target),
            placement: ColumnPlacement::After,
        }
    }

    fn members(&self) -> Vec<WindowId> {
        let ws = &self.engine.workspaces[self.workspace];
        ws.layout_system.all_windows_in_layout(ws.active_layout().unwrap())
    }
}

#[test]
fn reorder_accepts_owned_active_scrolling_workspace_without_changing_focus() {
    let mut f = Fixture::new();
    let request = f.request();
    let mut expected = f.members();
    expected.rotate_left(1);
    let selected = f.engine.focused_window();
    assert!(f.engine.reorder_column(request));
    assert_eq!(f.members(), expected);
    assert_eq!(f.engine.focused_window(), selected);
    assert_eq!(f.engine.workspaces.active_workspace(f.space), Some(f.workspace));
}

#[test]
fn reorder_rejects_wrong_space_workspace_and_non_scrolling_layout() {
    let mut f = Fixture::new();
    let initial = f.members();
    let mut request = f.request();
    request.space = SpaceId::new(999);
    assert!(!f.engine.reorder_column(request));
    let request = f.request();
    let other = f.engine.workspaces.list_workspaces(f.space)[1].0;
    f.engine.workspaces.set_active_workspace(f.space, other);
    assert!(!f.engine.reorder_column(request));
    assert_eq!(f.members(), initial);
    f.engine.workspaces.set_active_workspace(f.space, f.workspace);
    let request = f.request();
    f.engine
        .switch_workspace_layout_mode(&f.store, f.space, f.workspace, LayoutMode::Bsp);
    assert!(!f.engine.reorder_column(request));
}

#[test]
fn reorder_rejects_deleted_column_and_reused_ids_with_different_members() {
    let mut f = Fixture::new();
    let request = f.request();
    let _ = f
        .engine
        .handle_event(&mut f.store, LayoutEvent::WindowRemoved(WindowId::new(1, 1)));
    assert!(!f.engine.reorder_column(request.clone()));
    f.engine
        .switch_workspace_layout_mode(&f.store, f.space, f.workspace, LayoutMode::Bsp);
    let _ = f.engine.handle_event(
        &mut f.store,
        LayoutEvent::WindowAdded(f.space, WindowId::new(2, 1)),
    );
    f.engine
        .switch_workspace_layout_mode(&f.store, f.space, f.workspace, LayoutMode::Scrolling);
    let current = f.request();
    assert_eq!(
        current.source, request.source,
        "layout replacement reuses the numerical column ID"
    );
    assert_eq!(current.target, request.target);
    assert_ne!(current.source_members, request.source_members);
    let before = f.members();
    assert!(!f.engine.reorder_column(request));
    assert_eq!(f.members(), before);
}
