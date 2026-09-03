//! Otto header art, pre-rendered by tools/otto-render into assets/otto.ans.

use ansi_to_tui::IntoText;
use ratatui::text::Text;

pub const ART: &str = include_str!("../assets/otto.ans");
pub const WIDTH: u16 = 40;

pub fn text() -> Text<'static> {
    ART.into_text().unwrap_or_else(|_| Text::raw(ART))
}

pub fn height() -> u16 {
    ART.lines().count() as u16
}
