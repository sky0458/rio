mod child;
mod conpty;
mod pipes;
mod spsc;

use std::ffi::OsStr;
use std::io::{self};
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::TryRecvError;

use crate::windows::child::ChildExitWatcher;
use crate::{ChildEvent, EventedPty, ProcessReadWrite, Winsize, WinsizeBuilder};
use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

use conpty::Conpty as Backend;
use pipes::{EventedAnonRead as ReadPipe, EventedAnonWrite as WritePipe};

pub struct Pty {
    // Backend is required to be the first field, to ensure correct drop order. Dropping
    // `conout` before `backend` will cause a deadlock (with Conpty).
    backend: Backend,
    conout: ReadPipe,
    conin: WritePipe,
    read_token: corcovado::Token,
    write_token: corcovado::Token,
    child_event_token: corcovado::Token,
    child_watcher: ChildExitWatcher,
}

// Windows PowerShell 5.1 (the inbox shell on Windows 10 1809) and modern
// PowerShell both support this prompt wrapper. Rio already understands OSC 7,
// so report a real file URI instead of teaching the terminal core a second
// working-directory protocol just for Windows.
const POWERSHELL_CWD_HOOK: &str = r#"$global:__rio_original_prompt=$function:prompt; function global:prompt { $loc=$executionContext.SessionState.Path.CurrentLocation; if ($loc.Provider.Name -eq 'FileSystem') { $uri=([System.Uri]$loc.ProviderPath).AbsoluteUri; $host.UI.Write("$([char]27)]7;$uri$([char]27)\") }; if ($null -ne $global:__rio_original_prompt) { & $global:__rio_original_prompt } else { "PS $loc> " } }"#;

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(((bytes.len() + 2) / 3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);

        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn encode_powershell_command(command: &str) -> String {
    let bytes: Vec<u8> = command
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    base64_encode(&bytes)
}

fn shell_basename(shell: Option<&str>) -> String {
    shell
        .unwrap_or("powershell")
        .trim_matches('"')
        .rsplit(|ch| ch == '\\' || ch == '/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn powershell_has_explicit_command(args: &[String]) -> bool {
    args.iter().any(|arg| {
        let option = arg
            .trim_start_matches(|ch| ch == '-' || ch == '/')
            .to_ascii_lowercase();
        matches!(
            option.as_str(),
            "command" | "c" | "commandwithargs" | "encodedcommand" | "ec" | "file" | "f"
        )
    })
}

fn upsert_env(env: &mut Vec<(String, String)>, key: &str, value: String) {
    if let Some((_, existing)) = env
        .iter_mut()
        .find(|(existing_key, _)| existing_key.eq_ignore_ascii_case(key))
    {
        *existing = value;
    } else {
        env.push((key.to_owned(), value));
    }
}

/// Add shell-side cwd reporting without requiring users to edit their profile.
///
/// Windows does not expose a reliable public API for a console process's
/// logical shell directory (Windows PowerShell's `$PWD` can intentionally
/// differ from the process cwd). The shell therefore reports its directory to
/// Rio, exactly like shell integration in other terminals.
fn with_cwd_shell_integration(
    shell: Option<&str>,
    mut args: Vec<String>,
    mut env: Option<Vec<(String, String)>>,
) -> (Vec<String>, Option<Vec<(String, String)>>) {
    match shell_basename(shell).as_str() {
        "cmd" | "cmd.exe" => {
            let current_prompt = env
                .as_ref()
                .and_then(|vars| {
                    vars.iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case("PROMPT"))
                        .map(|(_, value)| value.clone())
                })
                .or_else(|| std::env::var("PROMPT").ok())
                .unwrap_or_else(|| "$P$G".to_owned());

            if !current_prompt.to_ascii_lowercase().contains("]7;file:///") {
                let vars = env.get_or_insert_with(Vec::new);
                // `$E` is ESC and `$P` is cmd.exe's current drive/path. ESC \
                // is the String Terminator accepted by Rio's OSC parser.
                upsert_env(
                    vars,
                    "PROMPT",
                    format!("$E]7;file:///$P$E\\{current_prompt}"),
                );
            }
        }
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => {
            // Do not rewrite configured one-shot commands/scripts into an
            // interactive shell. Interactive PowerShell gets `-NoExit` so the
            // setup command installs the prompt hook and then returns control
            // to the user. Profiles have already run, so their prompt is
            // captured and delegated to rather than replaced.
            if !powershell_has_explicit_command(&args) {
                if !args.iter().any(|arg| {
                    arg.trim_start_matches(|ch| ch == '-' || ch == '/')
                        .eq_ignore_ascii_case("noexit")
                }) {
                    args.push("-NoExit".to_owned());
                }
                args.push("-EncodedCommand".to_owned());
                args.push(encode_powershell_command(POWERSHELL_CWD_HOOK));
            }
        }
        _ => {}
    }

    (args, env)
}

// Creates conpty instead of pty
// Windows Pseudo Console (ConPTY)
//
// `env`, when given, is applied on top of the inherited environment,
// overriding inherited variables of the same name. `None` inherits as-is.
//
// `shell` of `None` means no program was configured, and the default console
// host is used.
pub fn create_pty(
    shell: Option<&str>,
    args: Vec<String>,
    working_directory: &Option<String>,
    env: Option<Vec<(String, String)>>,
    columns: u16,
    rows: u16,
) -> Result<Pty, std::io::Error> {
    let (args, env) = with_cwd_shell_integration(shell, args, env);
    let exec = match shell {
        Some(shell) => Some(if args.is_empty() {
            shell.to_string()
        } else {
            format!("{shell} {}", args.join(" "))
        }),
        // `conpty::new(None, ..)` starts PowerShell itself, but then there is
        // nowhere to attach our generated args. Materialize the default shell
        // command only when integration actually added arguments.
        None if !args.is_empty() => Some(format!("powershell {}", args.join(" "))),
        None => None,
    };
    conpty::new(exec.as_deref(), working_directory, env, columns, rows)
}

impl Pty {
    fn new(
        backend: impl Into<Backend>,
        conout: impl Into<ReadPipe>,
        conin: impl Into<WritePipe>,
        child_watcher: ChildExitWatcher,
    ) -> Self {
        Self {
            backend: backend.into(),
            conout: conout.into(),
            conin: conin.into(),
            read_token: 0.into(),
            write_token: 0.into(),
            child_event_token: 0.into(),
            child_watcher,
        }
    }

    pub fn child_watcher(&self) -> &ChildExitWatcher {
        &self.child_watcher
    }
}

impl ProcessReadWrite for Pty {
    type Reader = ReadPipe;
    type Writer = WritePipe;

    #[inline]
    fn register(
        &mut self,
        poll: &corcovado::Poll,
        token: &mut dyn Iterator<Item = corcovado::Token>,
        interest: corcovado::Ready,
        poll_opts: corcovado::PollOpt,
    ) -> io::Result<()> {
        self.read_token = token.next().unwrap();
        self.write_token = token.next().unwrap();

        if interest.is_readable() {
            poll.register(
                &self.conout,
                self.read_token,
                corcovado::Ready::readable(),
                poll_opts,
            )?
        } else {
            poll.register(
                &self.conout,
                self.read_token,
                corcovado::Ready::empty(),
                poll_opts,
            )?
        }
        if interest.is_writable() {
            poll.register(
                &self.conin,
                self.write_token,
                corcovado::Ready::writable(),
                poll_opts,
            )?
        } else {
            poll.register(
                &self.conin,
                self.write_token,
                corcovado::Ready::empty(),
                poll_opts,
            )?
        }

        self.child_event_token = token.next().unwrap();
        poll.register(
            self.child_watcher.event_rx(),
            self.child_event_token,
            corcovado::Ready::readable(),
            poll_opts,
        )?;

        Ok(())
    }

    #[inline]
    fn reregister(
        &mut self,
        poll: &corcovado::Poll,
        interest: corcovado::Ready,
        poll_opts: corcovado::PollOpt,
    ) -> io::Result<()> {
        if interest.is_readable() {
            poll.reregister(
                &self.conout,
                self.read_token,
                corcovado::Ready::readable(),
                poll_opts,
            )?;
        } else {
            poll.reregister(
                &self.conout,
                self.read_token,
                corcovado::Ready::empty(),
                poll_opts,
            )?;
        }
        if interest.is_writable() {
            poll.reregister(
                &self.conin,
                self.write_token,
                corcovado::Ready::writable(),
                poll_opts,
            )?;
        } else {
            poll.reregister(
                &self.conin,
                self.write_token,
                corcovado::Ready::empty(),
                poll_opts,
            )?;
        }

        poll.reregister(
            self.child_watcher.event_rx(),
            self.child_event_token,
            corcovado::Ready::readable(),
            poll_opts,
        )?;

        Ok(())
    }

    #[inline]
    fn deregister(&mut self, poll: &corcovado::Poll) -> io::Result<()> {
        poll.deregister(&self.conout)?;
        poll.deregister(&self.conin)?;
        poll.deregister(self.child_watcher.event_rx())?;
        Ok(())
    }

    #[inline]
    fn reader(&mut self) -> &mut Self::Reader {
        &mut self.conout
    }

    #[inline]
    fn read_token(&self) -> corcovado::Token {
        self.read_token
    }

    #[inline]
    fn writer(&mut self) -> &mut Self::Writer {
        &mut self.conin
    }

    #[inline]
    fn write_token(&self) -> corcovado::Token {
        self.write_token
    }

    #[inline]
    fn set_winsize(
        &mut self,
        winsize_builder: WinsizeBuilder,
    ) -> Result<(), std::io::Error> {
        let winsize: Winsize = winsize_builder.build();
        self.backend.on_resize(winsize);
        Ok(())
    }
}

impl EventedPty for Pty {
    fn child_event_token(&self) -> corcovado::Token {
        self.child_event_token
    }

    fn next_child_event(&mut self) -> Option<ChildEvent> {
        match self.child_watcher.event_rx().try_recv() {
            Ok(ev) => Some(ev),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(ChildEvent::Exited(None)),
        }
    }
}

fn cmdline(shell: Option<&str>) -> String {
    if let Some(shell) = shell.filter(|shell| !shell.is_empty()) {
        return shell.to_string();
    }

    once("powershell")
        // .chain(shell.args().iter().map(|a| a.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Converts the string slice into a Windows-standard representation for "W"-
/// suffixed function variants, which accept UTF-16 encoded string values.
pub fn win32_string<S: AsRef<OsStr> + ?Sized>(value: &S) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(once(0)).collect()
}

pub fn spawn_daemon<I, S>(program: &str, args: I) -> io::Result<()>
where
    I: IntoIterator<Item = S> + Copy,
    S: AsRef<OsStr>,
{
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_integration_preserves_custom_prompt() {
        let (args, env) = with_cwd_shell_integration(
            Some(r"C:\Windows\System32\cmd.exe"),
            Vec::new(),
            Some(vec![("PROMPT".to_owned(), "custom> ".to_owned())]),
        );
        assert!(args.is_empty());
        let prompt = env
            .unwrap()
            .into_iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("PROMPT"))
            .unwrap()
            .1;
        assert_eq!(prompt, "$E]7;file:///$P$E\\custom> ");
    }

    #[test]
    fn powershell_encoding_matches_known_vector() {
        assert_eq!(
            encode_powershell_command("Write-Output 'ok'"),
            "VwByAGkAdABlAC0ATwB1AHQAcAB1AHQAIAAnAG8AawAnAA=="
        );
    }

    #[test]
    fn powershell_interactive_shell_gets_cwd_hook() {
        let (args, env) = with_cwd_shell_integration(
            Some("powershell.exe"),
            vec!["-NoLogo".to_owned()],
            None,
        );
        assert!(env.is_none());
        assert!(args.iter().any(|arg| arg.eq_ignore_ascii_case("-NoExit")));
        assert!(args
            .iter()
            .any(|arg| arg.eq_ignore_ascii_case("-EncodedCommand")));
        assert_eq!(
            args.last().unwrap(),
            &encode_powershell_command(POWERSHELL_CWD_HOOK)
        );
        assert!(!args.last().unwrap().chars().any(char::is_whitespace));
    }

    #[test]
    fn explicit_powershell_command_is_not_rewritten() {
        let original = vec![
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            "Get-Location".to_owned(),
        ];
        let (args, env) =
            with_cwd_shell_integration(Some("pwsh.exe"), original.clone(), None);
        assert_eq!(args, original);
        assert!(env.is_none());
    }
}
