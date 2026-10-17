use std::{io::Read, path::PathBuf};
use stratum_poc::access::{serve, Config, Service};
use tokio_util::sync::CancellationToken;

#[tokio::main(worker_threads = 2)]
async fn main() {
    if run().await.is_err() {
        // Never print deserialization/network errors containing supplied secrets.
        eprintln!("Sidecar access checkpoint failed; check the local configuration and port.");
        std::process::exit(1);
    }
}
async fn run() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    anyhow::ensure!(
        args.len() == 2 && args[0] == "--config",
        "expected --config PATH"
    );
    let mut bytes = Vec::new();
    std::fs::File::open(PathBuf::from(&args[1]))?
        .take(8193)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= 8192, "configuration too large");
    let config: Config = serde_json::from_slice(&bytes)?;
    let service = Service::new(&config)?;
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    println!(
        "{}",
        serde_json::json!({"listen": format!("http://{}", listener.local_addr()?), "playback": false})
    );
    let shutdown = CancellationToken::new();
    let signal_stop = shutdown.clone();
    let signal = tokio::spawn(async move {
        #[cfg(unix)]
        {
            if let Ok(mut terminate) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            } else {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        signal_stop.cancel();
    });
    let result = serve(listener, service, &config, shutdown).await;
    signal.abort();
    result?;
    Ok(())
}
