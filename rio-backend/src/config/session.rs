use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Copy)]
pub struct Session {
    /// Upper bound of history+screen lines captured per pane on session save.
    /// `0` disables scrollback capture and serialization completely.
    #[serde(
        default = "default_max_scrollback_lines",
        rename = "max-scrollback-lines"
    )]
    pub max_scrollback_lines: usize,
}

fn default_max_scrollback_lines() -> usize {
    2000
}

impl Default for Session {
    fn default() -> Session {
        Session {
            max_scrollback_lines: default_max_scrollback_lines(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_disables_scrollback_capture() {
        let decoded: Session = toml::from_str("max-scrollback-lines = 0").unwrap();
        assert_eq!(decoded.max_scrollback_lines, 0);
    }
}
