//! Policy layer for the conductor. Ports aoaoe's `AoaoeConfig.policies`
//! block. Mechanism lives on `Instance`; policy lives here.

use std::time::Duration;

/// User-facing policies applied to reasoner recommendations before the
/// executor touches session state. Every field defaults conservative.
#[derive(Debug, Clone)]
pub struct ConductorPolicies {
    /// Allow actions that remove a session from the active view
    /// (`Action::Archive`). Off by default: the conductor should never
    /// make a session disappear without explicit opt-in.
    pub allow_destructive: bool,

    /// Allow `Nudge` recommendations (sending an input message into a
    /// running session's agent). Off by default because injecting text
    /// into a session that a human might be paying attention to is
    /// disruptive.
    pub allow_nudge: bool,

    /// Minimum time between successive actions on the same session.
    /// Ports aoaoe's `actionCooldownMs`. Guards against a reasoner that
    /// keeps recommending actions on the same session every tick.
    pub action_cooldown: Duration,

    /// If set, the watcher skips reasoning during this daily window.
    /// Format is `"HH:MM-HH:MM"` in the daemon's local timezone. Ports
    /// aoaoe's `quietHours`. `None` means no window (always active).
    pub quiet_hours: Option<QuietHours>,
}

impl Default for ConductorPolicies {
    fn default() -> Self {
        Self {
            allow_destructive: false,
            allow_nudge: false,
            action_cooldown: Duration::from_secs(30),
            quiet_hours: None,
        }
    }
}

/// Parsed `HH:MM-HH:MM` window. Kept as a separate type so parse errors
/// happen at construction, not on every tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuietHours {
    pub start_minutes: u32,
    pub end_minutes: u32,
}

impl QuietHours {
    /// Parse `"HH:MM-HH:MM"` (24-hour). Rejects malformed input rather
    /// than silently defaulting.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let (start, end) = s
            .split_once('-')
            .ok_or_else(|| anyhow::anyhow!("expected `HH:MM-HH:MM`, got {:?}", s))?;
        Ok(Self {
            start_minutes: parse_hh_mm(start.trim())?,
            end_minutes: parse_hh_mm(end.trim())?,
        })
    }

    /// True when the given minute-of-day falls inside the window.
    /// Handles wrap-around (e.g. `22:00-06:00`).
    pub fn contains(self, minute_of_day: u32) -> bool {
        if self.start_minutes <= self.end_minutes {
            minute_of_day >= self.start_minutes && minute_of_day < self.end_minutes
        } else {
            minute_of_day >= self.start_minutes || minute_of_day < self.end_minutes
        }
    }
}

fn parse_hh_mm(s: &str) -> anyhow::Result<u32> {
    let (h, m) = s
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("expected `HH:MM`, got {:?}", s))?;
    let h: u32 = h
        .parse()
        .map_err(|_| anyhow::anyhow!("bad hour: {:?}", h))?;
    let m: u32 = m
        .parse()
        .map_err(|_| anyhow::anyhow!("bad minute: {:?}", m))?;
    if h >= 24 || m >= 60 {
        anyhow::bail!("`HH:MM` out of range: {}:{}", h, m);
    }
    Ok(h * 60 + m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_conservative() {
        let p = ConductorPolicies::default();
        assert!(!p.allow_destructive);
        assert!(!p.allow_nudge);
        assert_eq!(p.action_cooldown, Duration::from_secs(30));
        assert!(p.quiet_hours.is_none());
    }

    #[test]
    fn quiet_hours_parse_valid_windows() {
        let q = QuietHours::parse("22:00-06:00").unwrap();
        assert_eq!(q.start_minutes, 22 * 60);
        assert_eq!(q.end_minutes, 6 * 60);
    }

    #[test]
    fn quiet_hours_wrap_at_midnight() {
        let q = QuietHours::parse("22:00-06:00").unwrap();
        assert!(q.contains(23 * 60));
        assert!(q.contains(3 * 60));
        assert!(!q.contains(12 * 60));
    }

    #[test]
    fn quiet_hours_same_day() {
        let q = QuietHours::parse("12:00-13:00").unwrap();
        assert!(q.contains(12 * 60 + 30));
        assert!(!q.contains(13 * 60));
        assert!(!q.contains(11 * 60));
    }

    #[test]
    fn quiet_hours_rejects_bad_input() {
        assert!(QuietHours::parse("not a window").is_err());
        assert!(QuietHours::parse("25:00-01:00").is_err());
        assert!(QuietHours::parse("12:60-13:00").is_err());
        assert!(QuietHours::parse("12:00").is_err());
    }
}
