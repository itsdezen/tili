//! Opt-in timing and diagnostic logging, controlled by `TILI_TRACE_TIMING`.
//!
//! Off by default: launchd redirects this process's stderr to
//! `daemon.err.log`, so a per-command or per-switch line is pure cost — an
//! allocation plus formatting — when nobody is reading it.
//!
//! Call sites pass `format_args!(...)`, so the formatting (and any `Debug`
//! work it implies) happens only after `log` has seen the flag. That is the
//! reason this is not a plain `eprintln!` behind an `if`: a `format!` built
//! before the check would allocate on every command even with tracing off.

use std::fmt::Arguments;
use std::sync::OnceLock;

static ENABLED: OnceLock<bool> = OnceLock::new();

/// Whether timing/diagnostic tracing is on for this process. Read once and
/// cached: this is a debugging switch set when the process is launched, not
/// a setting that hot-reloads while the daemon runs.
pub(crate) fn enabled() -> bool {
    *ENABLED.get_or_init(|| parse_flag(std::env::var("TILI_TRACE_TIMING").ok().as_deref()))
}

/// `1`, `true`, or `yes`, case-insensitively; unset or anything else is off.
/// Pure so tests never have to mutate the process environment.
fn parse_flag(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        value.eq_ignore_ascii_case("1")
            || value.eq_ignore_ascii_case("true")
            || value.eq_ignore_ascii_case("yes")
    })
}

/// Writes `args` to stderr only when tracing is enabled.
pub(crate) fn log(args: Arguments<'_>) {
    if enabled() {
        eprintln!("{args}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_or_empty_is_off() {
        assert!(!parse_flag(None));
        assert!(!parse_flag(Some("")));
    }

    #[test]
    fn truthy_values_are_case_insensitive() {
        for value in ["1", "true", "TRUE", "True", "yes", "YES", "Yes"] {
            assert!(parse_flag(Some(value)), "{value:?} should enable tracing");
        }
    }

    #[test]
    fn everything_else_is_off() {
        for value in ["0", "false", "no", "on", "2", " true", "true "] {
            assert!(
                !parse_flag(Some(value)),
                "{value:?} should leave tracing off"
            );
        }
    }
}
