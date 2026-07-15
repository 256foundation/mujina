//! Registry of environment variables that control the daemon, plus the
//! help text shown by `mujina-minerd --help`.
//!
//! Each variable is read in its own module; this registry is the single
//! place that documents them for users. Keep it in sync when adding or
//! changing a control variable.

use std::fmt::Write;

/// Render the grouped environment-variable reference for `--help`.
pub fn help_text() -> String {
    let mut out = String::from("Configuration is read from environment variables:\n");

    for group in GROUPS {
        write!(out, "\n{}:\n", group.title).unwrap();
        for var in group.vars {
            writeln!(out, "  {}", var.name).unwrap();
            out.push_str(&wrap(var.summary, INDENT, WIDTH));
            if let Some(default) = var.default {
                writeln!(out, "{INDENT}default: {default}").unwrap();
            }
            if let Some(example) = var.example {
                writeln!(out, "{INDENT}example: {example}").unwrap();
            }
        }
    }

    out
}

/// Hanging indent applied to every line under a variable name.
const INDENT: &str = "      ";

/// Maximum rendered line width before wrapping.
const WIDTH: usize = 79;

/// Wrap `text` into `indent`-prefixed lines no wider than `width`,
/// breaking on spaces. Words longer than the available width are kept
/// whole rather than split.
fn wrap(text: &str, indent: &str, width: usize) -> String {
    let avail = width.saturating_sub(indent.len());
    let mut out = String::new();
    let mut line = String::new();

    for word in text.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > avail {
            writeln!(out, "{indent}{line}").unwrap();
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        writeln!(out, "{indent}{line}").unwrap();
    }

    out
}

/// A titled group of related variables.
struct EnvGroup {
    title: &'static str,
    vars: &'static [EnvVar],
}

/// A single environment variable users can set to control the daemon.
struct EnvVar {
    /// Variable name, e.g. `MUJINA_POOL_URL`.
    name: &'static str,

    /// One-line explanation of what the variable does.
    summary: &'static str,

    /// Behavior when the variable is unset, when there is a meaningful
    /// fallback worth naming.
    default: Option<&'static str>,

    /// Example value, when one clarifies the expected format.
    example: Option<&'static str>,
}

const GROUPS: &[EnvGroup] = &[
    EnvGroup {
        title: "Pool (job source)",
        vars: &[
            EnvVar {
                name: "MUJINA_POOL_URL",
                summary: "Stratum v1 pool URL. When unset, the daemon runs a \
                          built-in dummy job source instead.",
                default: None,
                example: Some("stratum+tcp://pool.example.com:3333"),
            },
            EnvVar {
                name: "MUJINA_POOL_USER",
                summary: "Worker username sent to the pool.",
                default: Some("mujina-testing"),
                example: Some("myworker.1"),
            },
            EnvVar {
                name: "MUJINA_POOL_PASS",
                summary: "Worker password sent to the pool.",
                default: Some("x"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_POOL_FORCED_RATE",
                summary: "Override the share target so the source receives \
                          roughly this many shares per minute regardless of \
                          pool difficulty, for testing share submission at low \
                          hashrate. Requires MUJINA_POOL_URL.",
                default: Some("18 when set to an invalid value"),
                example: None,
            },
        ],
    },
    EnvGroup {
        title: "CPU miner",
        vars: &[
            EnvVar {
                name: "MUJINA_CPUMINER_THREADS",
                summary: "Number of CPU mining threads. Setting this enables the \
                          CPU mining backend, which needs no ASIC hardware.",
                default: Some("unset disables CPU mining"),
                example: Some("4"),
            },
            EnvVar {
                name: "MUJINA_CPUMINER_DUTY",
                summary: "CPU duty cycle percent (1-100). Each thread hashes this \
                          fraction of every second and sleeps the rest, capping \
                          sustained CPU load.",
                default: Some("50"),
                example: None,
            },
        ],
    },
    EnvGroup {
        title: "API server",
        vars: &[EnvVar {
            name: "MUJINA_API_LISTEN",
            summary: "Address the REST API listens on. A bare host or IP gets the \
                      default port :7785 appended.",
            default: Some("127.0.0.1:7785"),
            example: Some("0.0.0.0:7785"),
        }],
    },
    EnvGroup {
        title: "Hardware",
        vars: &[EnvVar {
            name: "MUJINA_USB_DISABLE",
            summary: "Set to any value to skip USB board discovery, useful for \
                      CPU-only runs.",
            default: Some("unset enables USB discovery"),
            example: None,
        }],
    },
    EnvGroup {
        title: "Logging",
        vars: &[
            EnvVar {
                name: "MUJINA_LOG",
                summary: "Log filter for Mujina's own modules, overriding \
                          RUST_LOG. Module names are written as the log output \
                          shows them, without a crate prefix. A bare level like \
                          'debug' applies to all of Mujina.",
                default: None,
                example: Some("asic::bm13xx=trace"),
            },
            EnvVar {
                name: "RUST_LOG",
                summary: "Log filter in tracing-subscriber EnvFilter syntax. \
                          A directive that names a crate adds to the built-in \
                          defaults; a bare level like 'debug' replaces them, \
                          as in any Rust program.",
                default: Some("warn,mujina_miner=info"),
                example: Some("nusb=debug"),
            },
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_text_wraps_within_width() {
        for line in help_text().lines() {
            assert!(
                line.len() <= WIDTH,
                "line exceeds {WIDTH} columns ({}): {line:?}",
                line.len()
            );
        }
    }

    #[test]
    fn help_text_lists_every_registered_variable() {
        let text = help_text();
        for group in GROUPS {
            assert!(
                text.contains(group.title),
                "missing group title: {}",
                group.title
            );
            for var in group.vars {
                assert!(text.contains(var.name), "missing variable: {}", var.name);
                if let Some(example) = var.example {
                    assert!(text.contains(example), "missing example for: {}", var.name);
                }
            }
        }
    }
}
