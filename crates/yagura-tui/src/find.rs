//! `/` search in a log: the text typed, which lines match, and where `n` / `N` go.

use ratatui::crossterm::event::KeyCode;

/// What a key did to the search being typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Typed {
    /// The text changed (or the key was ignored).
    Edit,
    /// Enter: keep the text and go to the first match.
    Done,
    /// Esc: drop the text.
    Cancel,
}

/// Which match to go to, counted from the bottom line shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The bottom line itself, or the first above it.
    First,
    /// The next one up (older).
    Older,
    /// The next one down (newer).
    Newer,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Find {
    pub query: String,
    pub typing: bool,
}

impl Find {
    /// Typing, or holding a text to step through.
    pub fn active(&self) -> bool {
        self.typing || !self.query.is_empty()
    }

    pub fn start(&mut self) {
        self.query.clear();
        self.typing = true;
    }

    pub fn clear(&mut self) {
        self.query.clear();
        self.typing = false;
    }

    /// Case-insensitive; an empty text matches nothing.
    pub fn matches(&self, line: &str) -> bool {
        !self.query.is_empty() && line.to_lowercase().contains(&self.query.to_lowercase())
    }

    pub fn key(&mut self, code: KeyCode) -> Typed {
        match code {
            KeyCode::Char(c) if !c.is_control() => self.query.push(c),
            KeyCode::Backspace => {
                self.query.pop();
            }
            KeyCode::Enter => {
                self.typing = false;
                return Typed::Done;
            }
            KeyCode::Esc => {
                self.clear();
                return Typed::Cancel;
            }
            _ => {}
        }
        Typed::Edit
    }

    /// The new `back` (lines up from the newest) that puts the wanted match on the bottom line, if there is one.
    pub fn jump(&self, lines: &[String], back: usize, step: Step) -> Option<usize> {
        let end = lines.len().saturating_sub(back.min(lines.len()));
        let hit = match step {
            Step::First => (0..end).rev().find(|&i| self.matches(&lines[i])),
            Step::Older => (0..end.saturating_sub(1)).rev().find(|&i| self.matches(&lines[i])),
            Step::Newer => (end..lines.len()).find(|&i| self.matches(&lines[i])),
        };
        hit.map(|i| lines.len() - 1 - i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> Vec<String> {
        ["boot ok", "ERROR disk", "idle", "error net", "done"].iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn typing_edits_the_text_and_enter_keeps_it() {
        let mut f = Find::default();
        f.start();
        assert!(f.active());
        for c in "erx".chars() {
            f.key(KeyCode::Char(c));
        }
        f.key(KeyCode::Backspace);
        assert_eq!(f.query, "er");
        assert_eq!(f.key(KeyCode::Enter), Typed::Done);
        assert!(!f.typing && f.active());
        assert_eq!(f.key(KeyCode::Esc), Typed::Cancel);
        assert!(!f.active());
    }

    #[test]
    fn matches_ignore_case_and_an_empty_text_matches_nothing() {
        let f = Find { query: "error".into(), typing: false };
        assert!(f.matches("ERROR disk") && f.matches("error net") && !f.matches("idle"));
        assert!(!Find::default().matches("anything"));
    }

    #[test]
    fn jumps_walk_up_and_down_the_matches() {
        let l = log();
        let f = Find { query: "error".into(), typing: false };
        // From the newest line the first match up is "error net" (index 3), which sits on the bottom line at back 1.
        let b = f.jump(&l, 0, Step::First).unwrap();
        assert_eq!(b, 1);
        // Next older is "ERROR disk" (index 1), back 3; nothing older after that.
        let b = f.jump(&l, b, Step::Older).unwrap();
        assert_eq!(b, 3);
        assert_eq!(f.jump(&l, b, Step::Older), None);
        // And back down to the newer one.
        assert_eq!(f.jump(&l, b, Step::Newer), Some(1));
        assert_eq!(f.jump(&l, 1, Step::Newer), None);
        // The bottom line itself counts for First.
        assert_eq!(f.jump(&l, 3, Step::First), Some(3));
        assert_eq!(Find { query: "zzz".into(), typing: false }.jump(&l, 0, Step::First), None);
    }
}
