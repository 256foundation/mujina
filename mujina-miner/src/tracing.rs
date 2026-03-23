//! Provide tracing, tailored to this program.
//!
//! At startup, the program should call [`init`] to install a tracing subscriber
//! (i.e., something that emits events to a log).
//!
//! The rest of the program can include `use crate::tracing::prelude::*`
//! for convenient access to the `trace!()`, `debug!()`, `info!()`,
//! `warn!()`, and `error!()` macros.

use std::fmt;
use time::OffsetDateTime;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::{
    filter::{EnvFilter, LevelFilter},
    fmt::{
        FmtContext, FormatEvent, FormatFields,
        format::{DefaultFields, Writer as FmtWriter},
        time::FormatTime,
    },
    prelude::*,
    registry::LookupSpan,
};

pub mod prelude {
    #[allow(unused_imports)]
    pub use tracing::{debug, error, info, trace, warn};
}

/// Initialize logging.
///
/// If running under systemd, use journald; otherwise fall back to
/// stdout.
pub fn init() {
    if !journald::try_init() {
        init_stdout();
    }
}

#[cfg(target_os = "linux")]
mod journald {
    use std::env;
    use std::io;
    use std::os::unix::io::AsRawFd;

    use nix::libc;
    use tracing_subscriber::prelude::*;

    use super::prelude::*;

    /// If running under systemd journal, install a journald subscriber
    /// and return `true`. Otherwise return `false`.
    pub fn try_init() -> bool {
        if !stderr_is_journal_stream() {
            return false;
        }

        if let Ok(layer) = tracing_journald::layer() {
            tracing_subscriber::registry().with(layer).init();
            true
        } else {
            error!("Failed to initialize journald logging, using stdout.");
            false
        }
    }

    /// Check if stderr is connected to systemd journal by validating
    /// JOURNAL_STREAM.
    ///
    /// Per systemd documentation, programs should parse the device and
    /// inode numbers from JOURNAL_STREAM and compare them against
    /// stderr's file descriptor to detect I/O redirection and ensure
    /// the connection is genuine.
    ///
    /// See: https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html#%24JOURNAL_STREAM
    fn stderr_is_journal_stream() -> bool {
        let journal_stream = match env::var("JOURNAL_STREAM") {
            Ok(val) => val,
            Err(_) => return false,
        };

        // Parse "device:inode" format
        let parts: Vec<&str> = journal_stream.split(':').collect();
        if parts.len() != 2 {
            return false;
        }

        let expected_dev: u64 = match parts[0].parse() {
            Ok(dev) => dev,
            Err(_) => return false,
        };

        let expected_ino: u64 = match parts[1].parse() {
            Ok(ino) => ino,
            Err(_) => return false,
        };

        // Get actual device and inode from stderr
        let stderr = io::stderr();
        let fd = stderr.as_raw_fd();

        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut stat) } != 0 {
            return false;
        }

        stat.st_dev == expected_dev && stat.st_ino == expected_ino
    }
}

#[cfg(not(target_os = "linux"))]
mod journald {
    pub fn try_init() -> bool {
        false
    }
}

// Log to stdout, filtering according to environment variable RUST_LOG,
// overriding the default level (ERROR) to INFO.
fn init_stdout() {
    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .with_env_var("RUST_LOG")
        .from_env_lossy();

    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_timer(LocalTimer)
                .with_target(true)
                .fmt_fields(DefaultFields::new())
                .event_format(CustomFormatter),
        )
        .init();
}

/// Custom event formatter that strips crate prefix, colors the target,
/// and displays fields on a second line for readability.
struct CustomFormatter;

/// Visitor that collects fields into a string buffer.
struct FieldCollector {
    fields: Vec<(String, String)>,
    message: Option<String>,
}

impl FieldCollector {
    fn new() -> Self {
        Self {
            fields: Vec::new(),
            message: None,
        }
    }
}

impl Visit for FieldCollector {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{:?}", value));
        } else {
            let formatted = format!("{:?}", value);
            // Clean up Option formatting: Some("foo") -> foo, None -> None
            let cleaned = if let Some(inner) = formatted.strip_prefix("Some(") {
                inner.strip_suffix(')').unwrap_or(inner).to_string()
            } else {
                formatted
            };
            self.fields.push((field.name().to_string(), cleaned));
        }
    }
}

impl<S, N> FormatEvent<S, N> for CustomFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: FmtWriter<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        // Collect fields first so we can extract log.target if present
        let mut visitor = FieldCollector::new();
        event.record(&mut visitor);

        // Write timestamp (no dimming)
        let timestamp = LocalTimer;
        timestamp.format_time(&mut writer)?;
        write!(writer, " ")?;

        // Write level with foreground color
        let level = *event.metadata().level();
        let (level_color, level_text) = match level {
            Level::ERROR => ("\x1b[31m", "ERROR"), // Red
            Level::WARN => ("\x1b[33m", "WARN "),  // Yellow
            Level::INFO => ("\x1b[32m", "INFO "),  // Green
            Level::DEBUG => ("\x1b[34m", "DEBUG"), // Blue
            Level::TRACE => ("\x1b[35m", "TRACE"), // Magenta
        };
        write!(writer, "{}{}\x1b[0m ", level_color, level_text)?;

        // Write target (module path) intelligently:
        // - Strip "mujina_miner::" from our own code to reduce noise
        // - For log compatibility layer, use log.target field if available
        // - Keep full paths from dependencies (e.g., "mio::poll")
        let target = event.metadata().target();
        let short_target = if let Some(stripped) = target.strip_prefix("mujina_miner::") {
            // Our code: strip the prefix
            stripped.to_string()
        } else if target == "log" {
            // Log compatibility layer: extract real target from log.target field
            visitor
                .fields
                .iter()
                .find(|(k, _)| k == "log.target")
                .map(|(_, v)| v.trim_matches('"').to_string())
                .unwrap_or_else(|| target.to_string())
        } else {
            // Dependency code with full module path: use as-is
            target.to_string()
        };
        write!(writer, "{}: ", short_target)?;

        // Write message (normal brightness)
        if let Some(ref msg) = visitor.message {
            // Strip quotes that Debug formatting adds to strings
            let clean_msg = msg.trim_matches('"');
            write!(writer, "{}", clean_msg)?;
        }

        // If there are structured fields, write them on a second line
        // Filter out log.* fields since they're compatibility layer metadata
        let display_fields: Vec<_> = visitor
            .fields
            .iter()
            .filter(|(k, _)| !k.starts_with("log."))
            .collect();

        if !display_fields.is_empty() {
            writeln!(writer)?;
            // Indent to align with module column
            // Timestamp (8 chars) + space + level (5 chars) + space = 15
            write!(writer, "\x1b[90m               ")?; // 15 spaces, bright black (dark gray)
            for (i, (key, value)) in display_fields.iter().enumerate() {
                if i > 0 {
                    write!(writer, ", ")?;
                }
                // Strip quotes from string values
                let clean_value = value.trim_matches('"');
                write!(writer, "{}={}", key, clean_value)?;
            }
            write!(writer, "\x1b[0m")?;
        }

        writeln!(writer)
    }
}

// Provide our own timer that formats timestamps in local time and to the
// nearest second. The default timer was in UTC and formatted timestamps as an
// long, ugly string.
struct LocalTimer;

impl FormatTime for LocalTimer {
    fn format_time(&self, w: &mut FmtWriter<'_>) -> fmt::Result {
        let now = OffsetDateTime::now_local().unwrap_or(OffsetDateTime::now_utc());
        write!(
            w,
            "{}",
            now.format(time::macros::format_description!(
                "[hour]:[minute]:[second]"
            ))
            .unwrap(),
        )
    }
}
