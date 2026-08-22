//! Session V2 persistence: workspace windows, tabs, split topology,
//! per-pane launch specification, working directory and optional scrollback.

use crate::context::{self, ContextManager};
use rio_backend::config::Shell;
use rio_backend::event::EventListener;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const SESSION_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionState {
    pub version: u32,
    pub windows: Vec<WindowState>,
    pub active_window: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowState {
    pub tabs: Vec<TabState>,
    pub active_tab: usize,
    /// Logical inner size, independent of monitor DPI.
    pub size: (f64, f64),
    /// Logical outer position when the window system exposes it.
    pub position: Option<(f64, f64)>,
    pub maximized: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TabState {
    pub layout: LayoutNode,
    /// Depth-first pane index within `layout`.
    pub active_pane: usize,
    pub custom_title: Option<String>,
    pub custom_color: Option<[f32; 4]>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LayoutNode {
    Leaf(PaneState),
    /// Weight is the child's taffy `flex_grow` proportional share.
    Split {
        direction: SplitDir,
        children: Vec<(f32, LayoutNode)>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum SplitDir {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PaneState {
    /// The actual shell program/arguments Rio used to launch this pane.
    pub launch: Shell,
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollback: Option<String>,
}

impl LayoutNode {
    pub fn first_leaf(&self) -> &PaneState {
        match self {
            LayoutNode::Leaf(pane) => pane,
            LayoutNode::Split { children, .. } => children[0].1.first_leaf(),
        }
    }
}

impl SessionState {
    pub fn load(path: &Path) -> Option<SessionState> {
        let bytes = std::fs::read(path).ok()?;
        let state: SessionState = match serde_json::from_slice(&bytes) {
            Ok(state) => state,
            Err(err) => {
                tracing::warn!("invalid session file {}: {err}", path.display());
                return None;
            }
        };
        if state.version != SESSION_VERSION || state.windows.is_empty() {
            tracing::warn!(
                "ignoring incompatible/empty session {} (version {}, expected {})",
                path.display(),
                state.version,
                SESSION_VERSION
            );
            return None;
        }
        Some(state)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, bytes)
    }
}

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

pub fn list_sessions() -> Vec<String> {
    let mut names: Vec<String> =
        std::fs::read_dir(rio_backend::config::sessions_dir_path())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let p = entry.path();
                if p.extension().is_some_and(|ext| ext == "json") {
                    p.file_stem().map(|s| s.to_string_lossy().into_owned())
                } else {
                    None
                }
            })
            .collect();
    names.sort();
    names
}

pub fn capture_window<T: EventListener + Clone + Send + 'static>(
    ctx_manager: &ContextManager<T>,
    max_scrollback_lines: usize,
    winit_window: &rio_window::window::Window,
) -> WindowState {
    let scale = winit_window.scale_factor();
    let inner = winit_window.inner_size().to_logical::<f64>(scale);
    let position = winit_window.outer_position().ok().map(|p| {
        let logical = p.to_logical::<f64>(scale);
        (logical.x, logical.y)
    });

    let tabs = ctx_manager
        .grids()
        .iter()
        .map(|grid| {
            let mut pane_index = 0usize;
            let mut active_pane = 0usize;
            let layout = grid.to_layout_node(&mut |ctx, is_active| {
                let index = pane_index;
                pane_index += 1;
                if is_active {
                    active_pane = index;
                }
                capture_pane(ctx, max_scrollback_lines)
            });
            TabState {
                layout,
                active_pane,
                custom_title: grid.custom_title.clone(),
                custom_color: grid.custom_color,
            }
        })
        .collect();

    WindowState {
        tabs,
        active_tab: ctx_manager.current_index(),
        size: (inner.width, inner.height),
        position,
        maximized: winit_window.is_maximized(),
    }
}

fn capture_pane<T: EventListener>(
    ctx: &context::Context<T>,
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
    let scrollback = if max_scrollback_lines == 0 {
        None
    } else {
        Some(terminal.scrollback_to_ansi(max_scrollback_lines))
            .filter(|content| !content.is_empty())
    };

    PaneState {
        launch: ctx.launch.clone(),
        cwd,
        scrollback,
    }
}

/// Rebuild one tab after the first leaf has already been created from
/// `tab.layout.first_leaf()`. Creation failures propagate to the caller.
pub fn restore_tab_layout<T: EventListener + Clone + Send + 'static>(
    ctx_manager: &mut ContextManager<T>,
    tab: &TabState,
    sugarloaf: &mut rio_backend::sugarloaf::Sugarloaf,
) -> Result<(), String> {
    build_node(ctx_manager, &tab.layout, sugarloaf)?;
    ctx_manager
        .current_grid_mut()
        .apply_layout_weights(&tab.layout);
    ctx_manager.select_pane_by_order(tab.active_pane);
    Ok(())
}

fn build_node<T: EventListener + Clone + Send + 'static>(
    ctx_manager: &mut ContextManager<T>,
    layout: &LayoutNode,
    sugarloaf: &mut rio_backend::sugarloaf::Sugarloaf,
) -> Result<(), String> {
    match layout {
        LayoutNode::Leaf(_) => Ok(()),
        LayoutNode::Split {
            direction,
            children,
        } => {
            let base = ctx_manager.current_grid().current;
            let mut leaves = vec![base];
            for (_, child) in children.iter().skip(1) {
                ctx_manager.split_from_session(
                    context::next_rich_text_id(),
                    *direction == SplitDir::Vertical,
                    sugarloaf,
                    child.first_leaf(),
                )?;
                leaves.push(ctx_manager.current_grid().current);
            }
            for (index, (_, child)) in children.iter().enumerate() {
                ctx_manager.current_grid_mut().set_current(leaves[index]);
                build_node(ctx_manager, child, sugarloaf)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rio-session-v2-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn sample_state(scrollback: Option<String>) -> SessionState {
        SessionState {
            version: SESSION_VERSION,
            active_window: 0,
            windows: vec![WindowState {
                tabs: vec![TabState {
                    layout: LayoutNode::Leaf(PaneState {
                        launch: Shell {
                            program: Some("nu".to_string()),
                            args: vec!["-l".to_string()],
                        },
                        cwd: Some(r"C:\work\rio".to_string()),
                        scrollback,
                    }),
                    active_pane: 0,
                    custom_title: Some("dev".to_string()),
                    custom_color: Some([0.1, 0.2, 0.3, 1.0]),
                }],
                active_tab: 0,
                size: (1280.0, 720.0),
                position: Some((20.0, 30.0)),
                maximized: false,
            }],
        }
    }

    #[test]
    fn v2_save_creates_parent_and_round_trips_launch_state() {
        let root = temp_root("create-parent");
        let p = root.join("missing").join("rio").join("session.json");
        let state = sample_state(Some("hello\r\n".to_string()));
        state.save(&p).unwrap();
        let loaded = SessionState::load(&p).unwrap();
        assert_eq!(loaded, state);
        match &loaded.windows[0].tabs[0].layout {
            LayoutNode::Leaf(pane) => {
                assert_eq!(pane.launch.program.as_deref(), Some("nu"));
                assert_eq!(pane.launch.args, vec!["-l"]);
                assert_eq!(pane.cwd.as_deref(), Some(r"C:\work\rio"));
            }
            _ => panic!("expected leaf"),
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn v1_is_rejected_without_migration() {
        let root = temp_root("version");
        let p = root.join("session.json");
        let mut value = serde_json::to_value(sample_state(None)).unwrap();
        value["version"] = serde_json::json!(1);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&p, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(SessionState::load(&p).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scrollback_none_is_omitted_from_json() {
        let bytes = serde_json::to_vec(&sample_state(None)).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("scrollback"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn nushell_osc9_9_updates_cwd_for_v2_capture() {
        use crate::context::ContextDimension;
        use crate::event::VoidListener;
        use rio_backend::event::WindowId;

        let mut ctx = context::create_mock_context(
            VoidListener {},
            WindowId::from(0),
            0,
            ContextDimension::default(),
        );
        ctx.launch = Shell {
            program: Some("nu".to_string()),
            args: vec!["-l".to_string()],
        };
        let mut processor = rio_backend::performer::handler::Processor::default();
        {
            let mut terminal = ctx.terminal.lock();
            processor.advance(
                &mut *terminal,
                b"\x1b]9;9;C:\\Users\\nu\\workspace\\rio\x1b\\",
            );
        }
        let pane = capture_pane(&ctx, 0);
        assert_eq!(pane.launch.program.as_deref(), Some("nu"));
        assert_eq!(pane.launch.args, vec!["-l"]);
        assert_eq!(pane.cwd.as_deref(), Some(r"C:\Users\nu\workspace\rio"));
        assert!(pane.scrollback.is_none());
    }
}
