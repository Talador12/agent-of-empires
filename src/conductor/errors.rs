//! Curated error-pattern library ported from aoaoe's
//! `session-error-pattern-library.ts`. Small catalog of patterns
//! (compiler errors, dependency failures, common Rust/Python/Node
//! traceback shapes) each with a remediation hint. Reasoner uses the
//! hint to steer nudges toward known fixes.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ErrorMatch {
    pub name: &'static str,
    pub language: &'static str,
    pub remediation: &'static str,
}

struct Pattern {
    name: &'static str,
    language: &'static str,
    needles: &'static [&'static str],
    remediation: &'static str,
}

const PATTERNS: &[Pattern] = &[
    Pattern {
        name: "rust_borrow_checker",
        language: "rust",
        needles: &[
            "cannot borrow",
            "borrow of moved value",
            "cannot move out of",
        ],
        remediation: "restructure ownership, `.clone()` the value, or take a reference",
    },
    Pattern {
        name: "rust_lifetime",
        language: "rust",
        needles: &["missing lifetime specifier", "does not live long enough"],
        remediation: "annotate the lifetime, use an owned type, or restructure the scope",
    },
    Pattern {
        name: "rust_trait_bound",
        language: "rust",
        needles: &["the trait bound", "not satisfied", "not implemented for"],
        remediation: "derive or implement the missing trait on the type",
    },
    Pattern {
        name: "python_module_not_found",
        language: "python",
        needles: &["modulenotfounderror", "no module named"],
        remediation: "install the missing package with pip or add it to requirements",
    },
    Pattern {
        name: "python_indentation",
        language: "python",
        needles: &["indentationerror", "unexpected indent"],
        remediation: "align indentation levels; mixed tabs and spaces are the usual cause",
    },
    Pattern {
        name: "node_module_not_found",
        language: "node",
        needles: &["cannot find module", "module_not_found"],
        remediation: "run `npm install` or check the import path",
    },
    Pattern {
        name: "typescript_type_error",
        language: "typescript",
        needles: &["ts2322", "ts2345", "ts2339"],
        remediation: "align the types or narrow with a guard",
    },
    Pattern {
        name: "git_merge_conflict",
        language: "git",
        needles: &["merge conflict", "conflict marker", "<<<<<<<"],
        remediation: "resolve the conflicting hunks and commit the resolution",
    },
    Pattern {
        name: "connection_refused",
        language: "generic",
        needles: &["connection refused", "econnrefused"],
        remediation: "check that the target service is running and reachable",
    },
    Pattern {
        name: "permission_denied",
        language: "generic",
        needles: &["permission denied", "eacces"],
        remediation: "chmod / chown the target or run with the required privilege",
    },
    Pattern {
        name: "disk_full",
        language: "generic",
        needles: &["no space left on device", "enospc"],
        remediation: "free space in the offending mount and retry",
    },
    Pattern {
        name: "rate_limit",
        language: "generic",
        needles: &["rate limit", "too many requests", "429"],
        remediation: "back off, add a delay, or use an authenticated request",
    },
];

/// Return the first matching pattern for the given pane text, or `None`
/// if no pattern matches. Case-insensitive.
pub fn match_error(pane: &str) -> Option<ErrorMatch> {
    if pane.trim().is_empty() {
        return None;
    }
    let lower = pane.to_ascii_lowercase();
    for p in PATTERNS {
        if p.needles.iter().any(|n| lower.contains(n)) {
            return Some(ErrorMatch {
                name: p.name,
                language: p.language,
                remediation: p.remediation,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_pane_matches_nothing() {
        assert!(match_error("").is_none());
    }

    #[test]
    fn borrow_checker_error_is_caught() {
        let pane = "error[E0382]: cannot borrow `x` as mutable more than once";
        let m = match_error(pane).unwrap();
        assert_eq!(m.name, "rust_borrow_checker");
        assert_eq!(m.language, "rust");
    }

    #[test]
    fn missing_python_module_is_caught() {
        let m = match_error("ModuleNotFoundError: No module named 'requests'").unwrap();
        assert_eq!(m.name, "python_module_not_found");
    }

    #[test]
    fn permission_denied_is_generic() {
        let m = match_error("write /etc/hosts: permission denied").unwrap();
        assert_eq!(m.language, "generic");
    }

    #[test]
    fn ambiguous_text_returns_first_match() {
        // `permission denied` and `rate limit` both mentioned; first-match wins.
        let pane = "permission denied\n... some other content\nrate limit exceeded";
        let m = match_error(pane).unwrap();
        assert_eq!(m.name, "permission_denied");
    }
}
