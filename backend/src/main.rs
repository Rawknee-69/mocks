mod api;
mod core;
mod error;
mod handlers;
mod logger;
mod server;

use dotenvy::dotenv;
use tracing::info;

fn main() {
    // Load .env from the backend directory (or parent) if present
    let _ = dotenv();

    logger::init();

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(9001);

    info!("clicky-backend starting on port {}", port);

    let tokio_runtime = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    tokio_runtime.block_on(server::run(port));
}
