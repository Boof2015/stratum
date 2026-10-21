use anyhow::{ensure, Context, Result};
use rustls::pki_types::ServerName;
use std::{
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpStream},
    sync::Arc,
    time::{Duration, Instant},
};
use stratum_poc::owner::Store;

/// Verify the local HTTPS service before printing installation success or issuing
/// a setup code. No external DNS, proxy, credentials or disabled TLS checks.
pub async fn show(store: Store) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match probe(&store) {
                Ok(()) => break,
                Err(error) if Instant::now() >= deadline => {
                    return Err(error.context(
                        "Sidecar did not answer locally over HTTPS. Check the sidecar service and its saved listen address.",
                    ));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(200)),
            }
        }
        println!("\nSidecar is running.\n");
        for origin in store.config()?.admin_origins {
            println!("Open: {origin}");
        }
        if store.owner_generation()?.is_some() {
            println!("\nSign in with your existing Sidecar password.");
        } else {
            let code = store.setup_code()?;
            println!("\nSetup code (valid for 30 minutes):\n\n{code}\n");
            println!("Enter this code in your browser and choose your Sidecar password.");
        }
        println!("Your browser may warn about Sidecar's local certificate.");
        Ok(())
    })
    .await?
}

fn probe(store: &Store) -> Result<()> {
    let config = store.config()?;
    let origin = reqwest::Url::parse(&config.admin_origins[0])?;
    let host = origin.host_str().context("Missing dashboard host.")?;
    let mut address = config.listen;
    if address.ip().is_unspecified() {
        address.set_ip(match address.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        });
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.add(store.certificate()?)?;
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let socket = TcpStream::connect_timeout(&address, Duration::from_secs(1))?;
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;
    socket.set_write_timeout(Some(Duration::from_secs(1)))?;
    let connection = rustls::ClientConnection::new(
        Arc::new(tls),
        ServerName::try_from(host.trim_matches(['[', ']']).to_owned())?,
    )?;
    let mut stream = rustls::StreamOwned::new(connection, socket);
    write!(
        stream,
        "GET /admin/api/status HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n",
        &config.admin_origins[0][8..]
    )?;
    let mut response = String::new();
    stream.take(4096).read_to_string(&mut response)?;
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .context("Incomplete HTTPS response.")?;
    ensure!(
        headers
            .lines()
            .next()
            .is_some_and(|line| matches!(line, "HTTP/1.0 200 OK" | "HTTP/1.1 200 OK")),
        "Sidecar's HTTPS check failed."
    );
    let status: serde_json::Value = serde_json::from_str(body)?;
    ensure!(
        status["ownerConfigured"].is_boolean(),
        "Unexpected service response."
    );
    Ok(())
}
