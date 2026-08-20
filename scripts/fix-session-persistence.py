from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"expected block not found in {path}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")


replace_once(
    "frontends/rioterm/src/session/mod.rs",
    '''    pub fn save(&self, path: &Path) -> std::io::Result<()> {\n        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;\n        std::fs::write(path, bytes)\n    }\n''',
    '''    pub fn save(&self, path: &Path) -> std::io::Result<()> {\n        if let Some(parent) = path\n            .parent()\n            .filter(|parent| !parent.as_os_str().is_empty())\n        {\n            std::fs::create_dir_all(parent)?;\n        }\n        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;\n        std::fs::write(path, bytes)\n    }\n''',
)

session = Path("frontends/rioterm/src/session/mod.rs")
text = session.read_text(encoding="utf-8")
if "mod session_persistence_tests" not in text:
    text += r'''

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
        sample_state().save(&path).expect("session save must succeed");
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
'''
    session.write_text(text, encoding="utf-8")

replace_once(
    "rio-backend/src/config/mod.rs",
    '''#[cfg(target_os = "windows")]\n#[inline]\npub fn config_dir_path() -> PathBuf {\n    std::env::var("RIO_CONFIG_HOME")\n        .map(PathBuf::from)\n        .unwrap_or(\n            dirs::home_dir()\n                .unwrap()\n                .join("AppData")\n                .join("Local")\n                .join("rio"),\n        )\n}\n''',
    '''#[cfg(target_os = "windows")]\n#[inline]\npub fn config_dir_path() -> PathBuf {\n    if let Some(path) = std::env::var_os("RIO_CONFIG_HOME") {\n        return PathBuf::from(path);\n    }\n\n    std::env::var_os("LOCALAPPDATA")\n        .map(PathBuf::from)\n        .unwrap_or_else(|| {\n            dirs::home_dir()\n                .expect("unable to determine Windows home directory")\n                .join("AppData")\n                .join("Local")\n        })\n        .join("rio")\n}\n''',
)

replace_once(
    "frontends/rioterm/src/router/mod.rs",
    '''            SessionRestore::Always => self.save_session(),\n''',
    '''            SessionRestore::Always => {\n                let _ = self.save_session();\n            }\n''',
)
replace_once(
    "frontends/rioterm/src/router/mod.rs",
    '''    pub fn save_session(&mut self) {\n        let max = self.window.screen.renderer.session_max_scrollback;\n        let state = crate::session::SessionState {\n            version: crate::session::SESSION_VERSION,\n            windows: vec![crate::session::capture_window(\n                self.window.screen.ctx(),\n                max,\n                &self.window.winit_window,\n            )],\n        };\n        if let Err(err) = state.save(&self.session_path()) {\n            tracing::warn!("session save failed: {err}");\n        }\n    }\n\n    /// Palette "Save Session As": bind this window to `name` and save.\n    pub fn save_session_as(&mut self, name: &str) {\n        let name = crate::session::sanitize_name(name);\n        if name.is_empty() {\n            return;\n        }\n        self.session_name = Some(name);\n        self.save_session();\n    }\n''',
    '''    pub fn save_session(&mut self) -> bool {\n        let max = self.window.screen.renderer.session_max_scrollback;\n        let state = crate::session::SessionState {\n            version: crate::session::SESSION_VERSION,\n            windows: vec![crate::session::capture_window(\n                self.window.screen.ctx(),\n                max,\n                &self.window.winit_window,\n            )],\n        };\n        let path = self.session_path();\n        match state.save(&path) {\n            Ok(()) => true,\n            Err(err) => {\n                tracing::warn!("session save failed at {}: {err}", path.display());\n                false\n            }\n        }\n    }\n\n    /// Palette "Save Session As": bind this window to `name` and save.\n    pub fn save_session_as(&mut self, name: &str) -> bool {\n        let name = crate::session::sanitize_name(name);\n        if name.is_empty() {\n            return false;\n        }\n        self.session_name = Some(name);\n        self.save_session()\n    }\n''',
)
replace_once(
    "frontends/rioterm/src/router/mod.rs",
    '''                        self.save_session();\n                        std::process::exit(0);\n''',
    '''                        if self.save_session() {\n                            std::process::exit(0);\n                        }\n                        self.request_overlay_redraw();\n''',
)

app = Path("frontends/rioterm/src/application.rs")
text = app.read_text(encoding="utf-8")
old = '''            RioEventType::Rio(RioEvent::SaveSession) => {\n                if let Some(route) = self.router.routes.get_mut(&window_id) {\n                    route.save_session();\n                    route\n                        .window\n                        .screen\n                        .renderer\n                        .session_prompt\n                        .set_saved_notice(true);\n                    route.request_redraw();\n                    self.scheduler.schedule(\n                        EventPayload::new(\n                            RioEventType::Rio(RioEvent::ClearSessionNotice),\n                            window_id,\n                        ),\n                        Duration::from_millis(1500),\n                        false,\n                        TimerId::new(Topic::ClearSessionNotice, 0),\n                    );\n                }\n            }\n            RioEventType::Rio(RioEvent::SaveSessionAs(name)) => {\n                if let Some(route) = self.router.routes.get_mut(&window_id) {\n                    route.save_session_as(&name);\n                    route\n                        .window\n                        .screen\n                        .renderer\n                        .session_prompt\n                        .set_saved_notice(true);\n                    route.request_redraw();\n                    self.scheduler.schedule(\n                        EventPayload::new(\n                            RioEventType::Rio(RioEvent::ClearSessionNotice),\n                            window_id,\n                        ),\n                        Duration::from_millis(1500),\n                        false,\n                        TimerId::new(Topic::ClearSessionNotice, 0),\n                    );\n                }\n            }\n'''
new = '''            RioEventType::Rio(RioEvent::SaveSession) => {\n                if let Some(route) = self.router.routes.get_mut(&window_id) {\n                    if route.save_session() {\n                        route\n                            .window\n                            .screen\n                            .renderer\n                            .session_prompt\n                            .set_saved_notice(true);\n                        route.request_redraw();\n                        self.scheduler.schedule(\n                            EventPayload::new(\n                                RioEventType::Rio(RioEvent::ClearSessionNotice),\n                                window_id,\n                            ),\n                            Duration::from_millis(1500),\n                            false,\n                            TimerId::new(Topic::ClearSessionNotice, 0),\n                        );\n                    }\n                }\n            }\n            RioEventType::Rio(RioEvent::SaveSessionAs(name)) => {\n                if let Some(route) = self.router.routes.get_mut(&window_id) {\n                    if route.save_session_as(&name) {\n                        route\n                            .window\n                            .screen\n                            .renderer\n                            .session_prompt\n                            .set_saved_notice(true);\n                        route.request_redraw();\n                        self.scheduler.schedule(\n                            EventPayload::new(\n                                RioEventType::Rio(RioEvent::ClearSessionNotice),\n                                window_id,\n                            ),\n                            Duration::from_millis(1500),\n                            false,\n                            TimerId::new(Topic::ClearSessionNotice, 0),\n                        );\n                    }\n                }\n            }\n'''
if old not in text:
    raise SystemExit("manual save event block not found")
app.write_text(text.replace(old, new, 1), encoding="utf-8")

workflow = Path(".github/workflows/session-restore-windows-release.yml")
text = workflow.read_text(encoding="utf-8")
if "name: Test Windows persistence" not in text:
    needle = "jobs:\n  build-windows:\n"
    insert = '''jobs:\n  test-windows:\n    name: Test Windows persistence (${{ matrix.os }})\n    strategy:\n      fail-fast: false\n      matrix:\n        os: [windows-2022, windows-2025]\n    runs-on: ${{ matrix.os }}\n\n    steps:\n      - name: Checkout feature head\n        uses: actions/checkout@v4\n        with:\n          ref: ${{ env.RELEASE_SHA }}\n\n      - name: Cache Rust build\n        uses: Swatinem/rust-cache@v2\n        with:\n          workspaces: . -> target\n          key: session-persistence-${{ matrix.os }}\n\n      - name: Test session persistence\n        run: cargo test -p rioterm session_persistence_tests -- --nocapture\n\n      - name: Verify session scrollback round trip\n        run: cargo test -p rio-vt scrollback_ansi_round_trip\n\n  build-windows:\n    needs: test-windows\n'''
    if needle not in text:
        raise SystemExit("release jobs marker not found")
    workflow.write_text(text.replace(needle, insert, 1), encoding="utf-8")
