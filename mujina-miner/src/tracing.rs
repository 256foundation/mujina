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
        .with(tracing_subscriber::fmt::layer().with_timer(LocalTimer))
        .init();
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
