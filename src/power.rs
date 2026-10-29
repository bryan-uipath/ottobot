//! Power source via `pmset -g batt` (macOS). A daemon on battery will sleep when the lid closes,
//! so runs may be interrupted; the TUI and tray surface that.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Power {
    pub on_ac: bool,
    pub percent: Option<u8>,
}

impl Power {
    pub fn summary(&self) -> String {
        let pct = self.percent.map(|p| format!(" {p}%")).unwrap_or_default();
        if self.on_ac {
            format!("AC{pct}")
        } else {
            format!("battery{pct}")
        }
    }
}

/// None when pmset is unavailable or unparsable (Linux, desktops without a battery still report AC).
pub fn read() -> Option<Power> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = std::process::Command::new("pmset").args(["-g", "batt"]).output().ok()?;
    parse(&String::from_utf8_lossy(&out.stdout))
}

fn parse(s: &str) -> Option<Power> {
    let first = s.lines().next()?;
    // Anything other than AC (battery, UPS) means the machine can sleep on us.
    let on_ac = if first.contains("'AC Power'") {
        true
    } else if first.contains("drawing from '") {
        false
    } else {
        return None;
    };
    let percent = s.lines().nth(1).and_then(|l| l.split('\t').nth(1)).and_then(|f| f.split('%').next()).and_then(|p| p.trim().parse().ok());
    Some(Power { on_ac, percent })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pmset() {
        let s = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=1)\t80%; AC attached; not charging present: true\n";
        assert_eq!(parse(s), Some(Power { on_ac: true, percent: Some(80) }));
        let s = "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=1)\t43%; discharging; 3:10 remaining present: true\n";
        assert_eq!(parse(s), Some(Power { on_ac: false, percent: Some(43) }));
        assert_eq!(parse("Now drawing from 'AC Power'\n"), Some(Power { on_ac: true, percent: None }));
        assert_eq!(parse("Now drawing from 'UPS Power'\n"), Some(Power { on_ac: false, percent: None }));
        assert_eq!(parse("garbage"), None);
    }
}
