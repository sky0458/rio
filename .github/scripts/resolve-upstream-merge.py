"""Resolve the four known conflicts from the October 2026 official Rio sync.

Always preserve both independent additions (Session V2 + upstream).
Fail rather than accidentally choosing an entire side on unknown conflicts.
"""
import re
from pathlib import Path

PATTERN = re.compile(
    r"(?m)^<<<<<<< HEAD\n(?P<ours>.*?)^=======\n(?P<theirs>.*?)^>>>>>>> [^\n]*\n",
    re.DOTALL | re.MULTILINE,
)

TARGETS = {
    "frontends/rioterm/src/context/mod.rs": 3,
    "frontends/rioterm/src/main.rs": 1,
    "frontends/rioterm/src/router/mod.rs": 5,
    "rio-vt/src/performer/handler.rs": 1,
}


def resolve(path: str, expected_count: int) -> None:
    file = Path(path)
    content = file.read_text()
    matched = list(PATTERN.finditer(content))
    if len(matched) != expected_count:
        raise ValueError(f"{path}: expected {expected_count} conflicts; got {len(matched)}")

    def replacement(m: re.Match) -> str:
        ours, theirs = m.group("ours"), m.group("theirs")
        if path.endswith("/context/mod.rs"):
            if "pub launch: Shell" in ours and "title_dirty:" in theirs:
                return ours + theirs
            if "launch: Shell::default()" in ours and "title_dirty: false" in theirs:
                return ours + theirs
            if "launch: config.shell.clone()" in ours and "title_dirty: false" in theirs:
                return ours + theirs
        elif path.endswith("/main.rs"):
            if "mod session;" in ours and "mod shell_integration;" in theirs:
                return ours + theirs
        elif path.endswith("/router/mod.rs"):
            if "session_name: " in ours and "quit_requested:" in theirs:
                return ours + theirs
        elif path.endswith("/handler.rs"):
            if "parse_osc9_9_current_directory" in ours and "parse_conemu_working_directory" in theirs:
                # Official parser accepts Nushell/ConEmu OSC 9;9 reports,
                # absolute Windows and Unix paths, semicolon payloads,
                # while also consuming malformed OSC 9;4 / 9;9 reports.
                return theirs
        raise ValueError(f"Unknown conflict in {path}\nOURS:\n{ours}\nTHEIRS:\n{theirs}")

    new_content = PATTERN.sub(replacement, content)
    if any(s in new_content for s in ("<<<<<<< HEAD", "=======", ">>>>>>> ")):
        raise ValueError(f"Unresolved conflict marker in {path}")
    file.write_text(new_content)


for path, count in TARGETS.items():
    resolve(path, count)

print("Resolved conflicts preserving Session V2, PTY lifecycle, shell integration and OSC 9.", flush=True)
