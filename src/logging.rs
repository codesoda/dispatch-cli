use tracing_appender::rolling;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Initialize tracing with rolling file appender and stderr output.
///
/// Log level priority: DISPATCH_LOG env var > default (info).
pub fn init_tracing() {
    let env_filter =
        EnvFilter::try_from_env("DISPATCH_LOG").unwrap_or_else(|_| EnvFilter::new("info"));

    let log_dir = dirs_log_dir();

    let file_appender = rolling::daily(&log_dir, "dispatch.log");
    let file_layer = fmt::layer()
        .with_writer(file_appender)
        .with_ansi(false)
        .with_target(true);

    let stderr_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_level(true);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .with(stderr_layer)
        .init();
}

fn dirs_log_dir() -> std::path::PathBuf {
    if let Some(data_dir) = dirs::data_local_dir() {
        data_dir.join("dispatch").join("logs")
    } else {
        std::path::PathBuf::from("/tmp/dispatch/logs")
    }
}
