//! Print each adapter's discovered catalog (diagnostic only; no model calls).
use sa_adapters::sys::Sys;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let wanted: Vec<String> = std::env::args().skip(1).collect();
    for adapter in sa_adapters::all(Sys::from_process()) {
        if !wanted.is_empty() && !wanted.iter().any(|w| w == adapter.agent()) {
            continue;
        }
        match adapter.catalog().await {
            Ok(c) => println!("{}: {}", adapter.agent(), serde_json::to_string_pretty(&c.value)?),
            Err(e) => println!("{}: error {e:#}", adapter.agent()),
        }
    }
    Ok(())
}
