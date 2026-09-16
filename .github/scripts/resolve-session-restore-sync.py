from pathlib import Path


def resolve_conflicts(path: str, choose):
    p = Path(path)
    lines = p.read_text().splitlines(keepends=True)
    out = []
    i = 0
    block = 0
    while i < len(lines):
        if not lines[i].startswith("<<<<<<< "):
            out.append(lines[i])
            i += 1
            continue
        i += 1
        ours = []
        while i < len(lines) and not lines[i].startswith("======="):
            ours.append(lines[i])
            i += 1
        if i >= len(lines):
            raise RuntimeError(f"unterminated conflict in {path}")
        i += 1
        theirs = []
        while i < len(lines) and not lines[i].startswith(">>>>>>> "):
            theirs.append(lines[i])
            i += 1
        if i >= len(lines):
            raise RuntimeError(f"unterminated conflict in {path}")
        i += 1
        out.extend(choose(block, ours, theirs))
        block += 1
    text = "".join(out)
    if "<<<<<<< " in text or "=======\n" in text or ">>>>>>> " in text:
        raise RuntimeError(f"conflict markers remain in {path}")
    p.write_text(text)
    return block


def context_choice(_idx, ours, _theirs):
    # Session V2 needs per-pane launch metadata, while upstream removed the
    # legacy per-context IME field. Keep launch only and use upstream IME flow.
    return [
        line
        for line in ours
        if "pub ime: Ime," not in line and "ime: Ime::new()," not in line
    ]


context_path = "frontends/rioterm/src/context/mod.rs"
router_path = "frontends/rioterm/src/router/mod.rs"
context_blocks = resolve_conflicts(context_path, context_choice)
router_blocks = resolve_conflicts(router_path, lambda _idx, _ours, theirs: theirs)
if context_blocks != 3:
    raise RuntimeError(f"expected 3 context conflicts, got {context_blocks}")
if router_blocks < 1:
    raise RuntimeError("expected router conflicts")

# Upstream owns IME lifecycle now; the old import is no longer valid after
# resolving the three Context construction conflicts above.
p = Path(context_path)
text = p.read_text().replace("use crate::ime::Ime;\n", "")

# Consolidate current-pane CWD inheritance. Terminal-reported directories are
# accepted only when absolute and free of control characters, matching pebrel's
# defensive cwd-report handling. Configured cwd remains the fallback.
old_block = '''        let mut working_dir = self.config.working_dir.clone();
        if self.config.cwd {
            #[cfg(not(target_os = "windows"))]
            {
                let current_context = self.current();
                if let Ok(path) = teletypewriter::foreground_process_path(
                    *current_context.main_fd,
                    current_context.shell_pid,
                ) {
                    working_dir = Some(path.to_string_lossy().to_string());
                }
            }

            #[cfg(target_os = "windows")]
            {
                let tracked = self.current().terminal.lock().current_directory.clone();
                if let Some(path) = tracked {
                    working_dir = Some(path.to_string_lossy().into_owned());
                }
            }
        }
'''
if text.count(old_block) != 2:
    raise RuntimeError(f"expected split/tab cwd block twice, got {text.count(old_block)}")
text = text.replace(old_block, "        let working_dir = self.focused_working_dir();\n")

split_marker = '''    pub fn split(
        &mut self,
        rich_text_id: usize,
'''
helper = '''    /// Resolve the startup directory for a child pane/window from the focused
    /// terminal. Dynamic terminal reports are trusted only when absolute and
    /// free of control characters; otherwise the configured directory wins.
    fn focused_working_dir(&self) -> Option<String> {
        if !self.config.cwd {
            return self.config.working_dir.clone();
        }

        #[cfg(not(target_os = "windows"))]
        let reported = {
            let current = self.current();
            teletypewriter::foreground_process_path(*current.main_fd, current.shell_pid)
                .ok()
                .map(|path| path.to_string_lossy().into_owned())
        };

        #[cfg(target_os = "windows")]
        let reported = self
            .current()
            .terminal
            .lock()
            .current_directory
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());

        reported
            .filter(|cwd| {
                std::path::Path::new(cwd).is_absolute()
                    && !cwd.chars().any(char::is_control)
            })
            .or_else(|| self.config.working_dir.clone())
    }

'''
if split_marker not in text:
    raise RuntimeError("split marker not found")
text = text.replace(split_marker, helper + split_marker, 1)

old_create_window = '''    #[inline]
    pub fn create_new_window(&self) {
        self.event_proxy
            .send_event(RioEvent::CreateWindow, self.window_id);
    }
'''
new_create_window = '''    #[inline]
    pub fn create_new_window(&self) {
        self.event_proxy.send_event(
            RioEvent::CreateWindow(self.focused_working_dir()),
            self.window_id,
        );
    }
'''
if old_create_window not in text:
    raise RuntimeError("create_new_window block not found")
text = text.replace(old_create_window, new_create_window, 1)
p.write_text(text)

# Re-apply Session V2 command-palette additions on top of upstream's new modal
# dispatcher. Everything else in the conflict uses upstream's implementation.
p = Path(router_path)
text = p.read_text()
anchor = '''                            use crate::renderer::command_palette::PaletteAction;

                            // Fonts-mode Enter: copy the family name to
'''
session_enter = '''                            use crate::renderer::command_palette::PaletteAction;

                            // Sessions-mode Enter: save-as or restore the picked
                            // name, then close the palette.
                            if let Some((name, saving)) = self
                                .window
                                .screen
                                .renderer
                                .command_palette
                                .get_selected_session()
                            {
                                self.window
                                    .screen
                                    .renderer
                                    .command_palette
                                    .set_enabled(false);
                                if saving {
                                    self.window
                                        .screen
                                        .context_manager
                                        .request_save_session_as(name);
                                } else {
                                    self.window
                                        .screen
                                        .context_manager
                                        .request_restore_session_named(name);
                                }
                                self.request_overlay_redraw();
                                return true;
                            }

                            // Fonts-mode Enter: copy the family name to
'''
if anchor not in text:
    raise RuntimeError("router selected-action anchor not found")
text = text.replace(anchor, session_enter, 1)

anchor = '''                                // Any other command is a one-shot: close
                                // the palette first, then dispatch.
'''
session_picker = '''                                // Session actions stay inside the palette and
                                // swap to the saved-session list.
                                Some(
                                    action @ (PaletteAction::SaveSessionAs
                                    | PaletteAction::RestoreSessionPicker),
                                ) => {
                                    let names = crate::session::list_sessions();
                                    self.window
                                        .screen
                                        .renderer
                                        .command_palette
                                        .enter_sessions_mode(
                                            names,
                                            action == PaletteAction::SaveSessionAs,
                                        );
                                }
                                // Any other command is a one-shot: close
                                // the palette first, then dispatch.
'''
if anchor not in text:
    raise RuntimeError("router palette-action anchor not found")
text = text.replace(anchor, session_picker, 1)
p.write_text(text)

# Upstream made Grid::style_set private and exposed style_of() as the public
# internal accessor. Keep Session V2 scrollback serialization on that API.
p = Path("rio-vt/src/crosswords/mod.rs")
text = p.read_text()
text = text.replace("self.grid.style_set.get(cell.style_id())", "self.grid.style_of(cell)")
p.write_text(text)

# Carry the focused CWD with CreateWindow, then apply it to a cloned config in
# Application so Router's public window-creation API remains unchanged.
p = Path("rio-vt/src/event/mod.rs")
text = p.read_text()
if "    CreateWindow,\n" not in text:
    raise RuntimeError("CreateWindow event variant not found")
text = text.replace("    CreateWindow,\n", "    CreateWindow(Option<String>),\n", 1)
if "RioEvent::CreateWindow => write!(f, \"CreateWindow\")" not in text:
    raise RuntimeError("CreateWindow debug arm not found")
text = text.replace(
    "RioEvent::CreateWindow => write!(f, \"CreateWindow\")",
    "RioEvent::CreateWindow(_) => write!(f, \"CreateWindow\")",
    1,
)
p.write_text(text)

p = Path("frontends/rioterm/src/application.rs")
text = p.read_text()
old = '''            RioEventType::Rio(RioEvent::CreateWindow) => {
                self.router.create_window(
                    event_loop,
                    self.event_proxy.clone(),
                    &self.config,
                    None,
                    self.app_id.as_deref(),
                );
            }
'''
new = '''            RioEventType::Rio(RioEvent::CreateWindow(working_dir)) => {
                let mut config = self.config.clone();
                if let Some(working_dir) = working_dir {
                    config.working_dir = Some(working_dir);
                    #[cfg(not(target_os = "windows"))]
                    {
                        // The fork PTY path has no working-directory parameter.
                        config.use_fork = false;
                    }
                }
                self.router.create_window(
                    event_loop,
                    self.event_proxy.clone(),
                    &config,
                    None,
                    self.app_id.as_deref(),
                );
            }
'''
if old not in text:
    raise RuntimeError("Application CreateWindow arm not found")
text = text.replace(old, new, 1)
p.write_text(text)
