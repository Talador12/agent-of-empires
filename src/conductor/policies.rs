//! Policy layer for the conductor. Mechanism (mutating a session's
//! attention-stack fields) lives in `Instance`; policy (which actions the
//! conductor is allowed to take on the user's behalf) lives here. Default
//! is conservative: non-destructive, never sends input into a session.

/// User-facing policies applied to reasoner recommendations before the
/// executor touches session state. Ports the shape of aoaoe's
/// `AoaoeConfig.policies` block (see `src/types.ts` in that repo) narrowed
/// to what the first PR actually implements.
///
/// Default is conservative: `bool::default()` is `false` for every field,
/// so `ConductorPolicies::default()` is the "safe" preset.
#[derive(Debug, Clone, Default)]
pub struct ConductorPolicies {
    /// Allow actions that remove a session from the active view
    /// (archive). Off by default: the conductor should never make a
    /// session disappear without explicit opt-in.
    pub allow_destructive: bool,

    /// Allow `Nudge` recommendations (sending an input message into a
    /// running session's agent). Off by default because injecting text
    /// into a session that a human might be paying attention to is
    /// disruptive; user must opt in.
    pub allow_nudge: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_conservative() {
        let p = ConductorPolicies::default();
        assert!(!p.allow_destructive);
        assert!(!p.allow_nudge);
    }
}
