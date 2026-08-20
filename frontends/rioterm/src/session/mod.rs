//! Session save/restore: tabs, split layout, per-pane working directory
//! and styled scrollback, persisted across runs (`[session]` config).

use crate::context::{self, ContextManager};
use rio_backend::event::EventListener;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Bumped whenever the on-disk shape changes; mismatched files are
/// discarded rather than migrated.
pub const SESSION_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub struct SessionState {
    pub version: u32,
    pub windows: Vec<WindowState>,
}

#[derive(Serialize, Deserialize)]
pub struct WindowState {
    pub tabs: Vec<TabState>,
    pub active_tab: usize,
    /// Physical inner size; (0, 0) when unknown.
    #[serde(default)]
    pub size: (u32, u32),
    /// Physical outer position. Absent on Wayland (compositor-placed).
    #[serde(default)]
    pub position: Option<(i32, i32)>,
}

#[derive(Serialize, Deserialize)]
pub struct TabState {
    pub layout: LayoutNode,
    pub custom_title: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub enum LayoutNode {
    Leaf(PaneState),
    /// Weight is the child's taffy `flex_grow` — proportional share of
    /// the container, not an absolute size.
    Split {
        direction: SplitDir,
        children: Vec<(f32, LayoutNode)>,
    },
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
pub enum SplitDir {
    Horizontal,
    Vertical,
}

#[derive(Serialize, Deserialize)]
pub struct PaneState {
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub is_active: bool,
    pub scrollback: String,
}

impl LayoutNode {
    /// The pane a subtree's first split-off context should spawn as.
    pub fn first_leaf(&self) -> &PaneState {
        match self {
            LayoutNode::Leaf(p) => p,
            LayoutNode::Split { children, .. } => children[0].1.first_leaf(),
        }
    }
}

impl SessionState {
    pub fn load(path: &Path) -> Option<SessionState> {
        let bytes = std::fs::read(path).ok()?;
        let state: SessionState = serde_json::from_slice(&bytes).ok()?;
        if state.version != SESSION_VERSION || state.windows.is_empty() {
            return None;
        }
        Some(state)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        std::fs::write(path, bytes)
    }

    pub fn discard(path: &Path) {
        let _ = std::fs::remove_file(path);
    }
}

/// Keep names filesystem-safe: anything outside [A-Za-z0-9._-]
/// becomes '-'.
pub fn sanitize_name(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Saved session names (sessions/*.json), sorted.
pub fn list_sessions() -> Vec<String> {
    let mut names: Vec<String> =
        std::fs::read_dir(rio_backend::config::sessions_dir_path())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "json") {
                    path.file_stem().map(|s| s.to_string_lossy().into_owned())
                } else {
                    None
                }
            })
            .collect();
    names.sort();
    names
}

/// Capture one window's tabs from the live context tree.
pub fn capture_window<T: EventListener + Clone + Send + 'static>(
    ctx_manager: &ContextManager<T>,
    max_scrollback_lines: usize,
    winit_window: &rio_window::window::Window,
) -> WindowState {
    let size = winit_window.inner_size();
    let position = winit_window.outer_position().ok().map(|p| (p.x, p.y));
    let tabs = ctx_manager
        .grids()
        .iter()
        .map(|grid| TabState {
            custom_title: grid.custom_title.clone(),
            layout: grid.to_layout_node(&mut |ctx, is_active| {
                capture_pane(ctx, is_active, max_scrollback_lines)
            }),
        })
        .collect();

    WindowState {
        tabs,
        active_tab: ctx_manager.current_index(),
        size: (size.width, size.height),
        position,
    }
}

fn capture_pane<T: EventListener>(
    ctx: &context::Context<T>,
    is_active: bool,
    max_scrollback_lines: usize,
) -> PaneState {
    #[cfg(not(target_os = "windows"))]
    let mut cwd = teletypewriter::foreground_process_path(*ctx.main_fd, ctx.shell_pid)
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    #[cfg(target_os = "windows")]
    let mut cwd: Option<String> = None;

    let terminal = ctx.terminal.lock();
    if cwd.is_none() {
        cwd = terminal
            .current_directory
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
    }
    let scrollback = terminal.scrollback_to_ansi(max_scrollback_lines);
    drop(terminal);

    PaneState {
        cwd,
        title: Some(ctx.title.content.clone()).filter(|t| !t.is_empty()),
        is_active,
        scrollback,
    }
}

/// Rebuild one tab's split tree inside the current (single-pane) grid.
/// The grid's sole pane must already have been spawned for
/// `layout.first_leaf()`; this splits out the remaining panes and
/// injects each leaf's scrollback.
pub fn restore_tab_layout<T: EventListener + Clone + Send + 'static>(
    ctx_manager: &mut ContextManager<T>,
    layout: &LayoutNode,
    sugarloaf: &mut rio_backend::sugarloaf::Sugarloaf,
) {
    build_node(ctx_manager, layout, sugarloaf);
    ctx_manager.current_grid_mut().apply_layout_weights(layout);
    restore_active_pane(ctx_manager, layout);
}

fn build_node<T: EventListener + Clone + Send + 'static>(
    ctx_manager: &mut ContextManager<T>,
    layout: &LayoutNode,
    sugarloaf: &mut rio_backend::sugarloaf::Sugarloaf,
) {
    match layout {
        LayoutNode::Leaf(pane) => inject_scrollback(ctx_manager, pane),
        LayoutNode::Split {
            direction,
            children,
        } => {
            // Split the subtree's base leaf once per extra child (rio
            // produces binary trees; >2 children rebuild as a nested
            // chain, which keeps content and approximates ratios).
            let base = ctx_manager.current_grid().current;
            let mut leaves = vec![base];
            for (_, child) in children.iter().skip(1) {
                ctx_manager.split_with_dir(
                    context::next_rich_text_id(),
                    *direction == SplitDir::Vertical,
                    sugarloaf,
                    child.first_leaf().cwd.clone(),
                );
                leaves.push(ctx_manager.current_grid().current);
            }
            for (i, (_, child)) in children.iter().enumerate() {
                ctx_manager.current_grid_mut().set_current(leaves[i]);
                build_node(ctx_manager, child, sugarloaf);
            }
        }
    }
}

fn inject_scrollback<T: EventListener + Clone + Send + 'static>(
    ctx_manager: &mut ContextManager<T>,
    pane: &PaneState,
) {
    if pane.scrollback.is_empty() {
        return;
    }
    let ctx = ctx_manager.current_mut();
    let mut processor = rio_backend::performer::handler::Processor::default();
    let mut terminal = ctx.terminal.lock();
    processor.advance(&mut *terminal, pane.scrollback.as_bytes());
}

fn restore_active_pane<T: EventListener + Clone + Send + 'static>(
    ctx_manager: &mut ContextManager<T>,
    layout: &LayoutNode,
) {
    // Leaves were built depth-first in the same order to_layout_node
    // walks them, so re-walk and select the one flagged active.
    fn find_active_index(node: &LayoutNode, next: &mut usize) -> Option<usize> {
        match node {
            LayoutNode::Leaf(p) => {
                let idx = *next;
                *next += 1;
                p.is_active.then_some(idx)
            }
            LayoutNode::Split { children, .. } => children
                .iter()
                .find_map(|(_, c)| find_active_index(c, next)),
        }
    }
    let mut counter = 0;
    if let Some(idx) = find_active_index(layout, &mut counter) {
        ctx_manager.select_pane_by_order(idx);
    }
}

#[cfg(test)]
mod session_persistence_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rio-session-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn sample_state() -> SessionState {
        SessionState {
            version: SESSION_VERSION,
            windows: vec![WindowState {
                tabs: vec![TabState {
                    layout: LayoutNode::Leaf(PaneState {
                        cwd: Some(r"C:\work\rio".to_string()),
                        title: Some("shell".to_string()),
                        is_active: true,
                        scrollback: "hello\r\n".to_string(),
                    }),
                    custom_title: Some("dev".to_string()),
                }],
                active_tab: 0,
                size: (1280, 720),
                position: Some((20, 30)),
            }],
        }
    }

    #[test]
    fn save_creates_missing_parent_directories_and_round_trips() {
        let root = temp_root("create-parent");
        let path = root.join("missing").join("rio").join("session.json");
        assert!(!path.parent().unwrap().exists());
        sample_state()
            .save(&path)
            .expect("session save must succeed");
        assert!(path.is_file());

        let loaded = SessionState::load(&path).expect("saved session must load");
        assert_eq!(loaded.version, SESSION_VERSION);
        assert_eq!(loaded.windows.len(), 1);
        assert_eq!(loaded.windows[0].tabs.len(), 1);
        assert_eq!(loaded.windows[0].size, (1280, 720));
        assert_eq!(loaded.windows[0].position, Some((20, 30)));
        match &loaded.windows[0].tabs[0].layout {
            LayoutNode::Leaf(pane) => {
                assert_eq!(pane.cwd.as_deref(), Some(r"C:\work\rio"));
                assert!(pane.is_active);
                assert_eq!(pane.scrollback, "hello\r\n");
            }
            LayoutNode::Split { .. } => panic!("expected leaf"),
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn save_overwrites_an_existing_session_file() {
        let root = temp_root("overwrite");
        let path = root.join("session.json");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&path, b"broken").unwrap();
        sample_state().save(&path).expect("overwrite must succeed");
        assert!(SessionState::load(&path).is_some());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn load_rejects_unknown_session_version() {
        let root = temp_root("version");
        let path = root.join("session.json");
        let mut state = sample_state();
        state.version = SESSION_VERSION + 1;
        state.save(&path).unwrap();
        assert!(SessionState::load(&path).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn save_session_action_is_config_parseable() {
        assert_eq!(
            crate::bindings::Action::from("SaveSession".to_string()),
            crate::bindings::Action::SaveSession
        );
    }
}
