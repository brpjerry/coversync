//! Runs the proxy in front of a server, to try it without the app:
//!
//! ```sh
//! cargo run -p coversync-proxy --example proxy -- https://n5pro.internal/coversync/
//! ```
//!
//! It prints the entry URL; open it in a browser. `RUST_LOG=debug` logs each
//! request it forwards.

use coversync_proxy::{Proxy, Upstream};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let url = std::env::args().nth(1).ok_or("usage: proxy <server URL>")?;
    let upstream = Upstream::parse(&url)?;
    let proxy = Proxy::builder()
        .upstream(Some(upstream.clone()))
        .on_unreachable(|unreachable| eprintln!("page load: {unreachable}"))
        .start()
        .await?;
    match proxy.check(&upstream).await {
        Ok(()) => eprintln!("{} answers.", upstream.url()),
        Err(unreachable) => eprintln!("{unreachable}"),
    }
    println!("{}", proxy.entry_url());
    std::future::pending::<()>().await;
    Ok(())
}
