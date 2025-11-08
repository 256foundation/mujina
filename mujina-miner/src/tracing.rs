//! Provide tracing, tailored to this program.
//!
//! At startup, the program should call one of the init_* functions at startup
//! to install a tracing subscriber (i.e., something that emits events to a
//! log).
//!
//! The rest of program the can include `use tracing::prelude::*` for convenient
//! access to the `trace!()`, `debug!()`, `info!()`, `warn!()`, and `error!()`
//! macros.

use std::env;
use time::OffsetDateTime;
use tracing_journald;
use tracing_subscriber::{
    filter::{EnvFilter, LevelFilter},
    fmt::{format::Writer, time::FormatTime},
    prelude::*,
};

pub mod prelude {
    #[allow(unused_imports)]
    pub use tracing::{debug, error, info, trace, warn};
}

use prelude::*;

/// Initialize logging.
///
/// If running under systemd, use journald; otherwise fall
/// back to stdout.
pub fn init_journald_or_stdout() {
    if env::var("JOURNAL_STREAM").is_ok() {
        if let Ok(layer) = tracing_journald::layer() {
            tracing_subscriber::registry().with(layer).init();
        } else {
            use_stdout();
            error!("Failed to initialize journald logging, using stdout.");
        }
    } else {
        use_stdout();
    }
}

// Log to stdout, filtering according to environment variable RUST_LOG,
// overriding the default level (ERROR) to INFO.
fn use_stdout() {
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
                .fmt_fields(tracing_subscriber::fmt::format::DefaultFields::new())
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

impl tracing::field::Visit for FieldCollector {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
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

impl<S, N> tracing_subscriber::fmt::FormatEvent<S, N> for CustomFormatter
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        // Write timestamp (no dimming)
        let timestamp = LocalTimer;
        timestamp.format_time(&mut writer)?;
        write!(writer, " ")?;

        // Write level with foreground color
        let level = *event.metadata().level();
        let (level_color, level_text) = match level {
            tracing::Level::ERROR => ("\x1b[31m", "ERROR"), // Red
            tracing::Level::WARN => ("\x1b[33m", "WARN "),  // Yellow
            tracing::Level::INFO => ("\x1b[32m", "INFO "),  // Green
            tracing::Level::DEBUG => ("\x1b[34m", "DEBUG"), // Blue
            tracing::Level::TRACE => ("\x1b[35m", "TRACE"), // Magenta
        };
        write!(writer, "{}{}\x1b[0m ", level_color, level_text)?;

        // Write target (module path), stripping "mujina_miner::" prefix
        let target = event.metadata().target();
        let short_target = target.strip_prefix("mujina_miner::").unwrap_or(target);
        write!(writer, "{}: ", short_target)?;

        // Collect fields using visitor
        let mut visitor = FieldCollector::new();
        event.record(&mut visitor);

        // Write message (normal brightness)
        if let Some(ref msg) = visitor.message {
            // Strip quotes that Debug formatting adds to strings
            let clean_msg = msg.trim_matches('"');
            write!(writer, "{}", clean_msg)?;
        }

        // If there are structured fields, write them on a second line
        if !visitor.fields.is_empty() {
            writeln!(writer)?;
            // Indent to align with module column
            // Timestamp (8 chars) + space + level (5 chars) + space = 15
            write!(writer, "\x1b[90m               ")?; // 15 spaces, bright black (dark gray)
            for (i, (key, value)) in visitor.fields.iter().enumerate() {
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
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
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
