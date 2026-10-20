use anyhow::{bail, ensure, Context, Result};
use std::{
    io::{IsTerminal, Read},
    net::TcpListener,
    path::PathBuf,
};
use stratum_poc::owner::{self, Config, Store};
use tokio_util::sync::CancellationToken;

const HELP: &str = "Astra Sidecar — owner setup preview

sidecar init --data-dir DIR [--listen IP:PORT] [--admin-origin https://HOST:PORT ...]
sidecar serve --data-dir DIR
sidecar set-password --data-dir DIR [--password-stdin]
sidecar reset-password --data-dir DIR [--password-stdin]
sidecar setup-code --data-dir DIR
sidecar renew-certificate --data-dir DIR
sidecar status --data-dir DIR
sidecar configure --data-dir DIR --settings FILE
sidecar check-navidrome --data-dir DIR [--url URL]

init creates persistent configuration and HTTPS identity. serve never prompts;
before owner setup it prints a one-time code valid for 30 minutes. Password
commands prompt without echo by default. --password-stdin is for protected input
from automation; passwords are never accepted in command-line arguments.

configure imports connection settings after owner setup; check-navidrome detects
the API without signing in. Discovery publication, music serving and the full
dashboard are not wired into this entry point yet.";

#[tokio::main(worker_threads = 2)]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Sidecar: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let Some(command) = args.next() else {
        println!("{HELP}");
        return Ok(());
    };
    if command == "--help" || command == "help" {
        println!("{HELP}");
        return Ok(());
    }
    let command = command.to_str().context("Invalid command.")?;
    ensure!(
        [
            "init",
            "serve",
            "set-password",
            "reset-password",
            "setup-code",
            "renew-certificate",
            "status",
            "configure",
            "check-navidrome"
        ]
        .contains(&command),
        "Unknown command. Use --help."
    );
    let mut directory = None;
    let mut config = Config::default();
    let mut origins = Vec::new();
    let mut password_stdin = false;
    let mut settings_file = None;
    let mut probe_url = None;
    while let Some(flag) = args.next() {
        match flag.to_str() {
            Some("--data-dir") if directory.is_none() => {
                directory = Some(PathBuf::from(
                    args.next().context("--data-dir needs a directory.")?,
                ));
            }
            Some("--listen") if command == "init" => {
                config.listen = args
                    .next()
                    .and_then(|v| v.into_string().ok())
                    .context("--listen needs IP:PORT.")?
                    .parse()
                    .context("Invalid listen address.")?;
            }
            Some("--admin-origin") if command == "init" => {
                origins.push(
                    args.next()
                        .and_then(|v| v.into_string().ok())
                        .context("--admin-origin needs an HTTPS origin.")?,
                );
            }
            Some("--password-stdin")
                if command == "set-password" || command == "reset-password" =>
            {
                password_stdin = true
            }
            Some("--settings") if command == "configure" && settings_file.is_none() => {
                settings_file = Some(PathBuf::from(
                    args.next().context("--settings needs a file.")?,
                ));
            }
            Some("--url") if command == "check-navidrome" && probe_url.is_none() => {
                probe_url = Some(
                    args.next()
                        .and_then(|v| v.into_string().ok())
                        .context("--url needs an address.")?,
                );
            }
            _ => bail!("Unknown or unsupported option. Use --help."),
        }
    }
    let directory = directory.context("Specify a persistent --data-dir.")?;
    if command == "init" {
        if !origins.is_empty() {
            config.admin_origins = origins;
        } else if config.listen != Config::default().listen {
            ensure!(
                !config.listen.ip().is_unspecified(),
                "Supply --admin-origin when binding to all interfaces."
            );
            config.admin_origins = vec![format!("https://{}", config.listen)];
        }
        let store = Store::initialize(&directory, &config)?;
        println!(
            "Sidecar initialized. Configuration and identity are saved in {}.",
            directory.display()
        );
        println!("Installation: {}", store.identity()?.installation_id);
        println!(
            "Run serve to start HTTPS setup, or set-password to choose the owner password here."
        );
        return Ok(());
    }
    let store = Store::open(&directory).context(
        "Could not open Sidecar. Check the data directory; run init for a new installation.",
    )?;
    match command {
        "configure" => {
            let path = settings_file
                .context("Specify --settings with a connection-settings JSON file.")?;
            let mut bytes = Vec::new();
            std::fs::File::open(path)?
                .take(32769)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 32768, "Settings file is too large.");
            let settings: owner::setup::Settings = serde_json::from_slice(&bytes)
                .context("Invalid connection settings. See docs/owner-setup.md for the format.")?;
            let current = store.config()?.setup;
            let saved = store.save_setup(&settings, &current.revision, None)?;
            println!("Connection settings saved ({}). Addresses have not been checked. Run check-navidrome to detect the server; this does not sign in.", saved.stage());
        }
        "check-navidrome" => {
            ensure!(
                store.owner_generation()?.is_some(),
                "Set the owner password before checking Navidrome."
            );
            let url = probe_url
                .or(store.config()?.setup.settings.navidrome_url)
                .context("Save a Navidrome address first, or specify --url.")?;
            let detection = owner::setup::Probe::new()?.check(&url).await?;
            println!("{}", serde_json::to_string_pretty(&detection)?);
        }
        "set-password" | "reset-password" => {
            let password = read_password(password_stdin)?;
            store.set_password(&password, command == "reset-password")?;
            println!("Owner password saved. Previous dashboard sessions are signed out.");
        }
        "setup-code" => print_code(&store.setup_code()?),
        "renew-certificate" => {
            store.renew_certificate()?;
            println!(
                "Certificate renewed using the existing identity key. Restart Sidecar to use it."
            );
        }
        "status" => println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "identity": store.identity()?,
                "ownerConfigured": store.owner_generation()?.is_some(),
                "config": store.config()?,
            }))?
        ),
        "serve" => {
            let _process_lock = store.server_lock()?;
            let config = store.config()?;
            let service = owner::Service::new(store.clone())?;
            let listener =
                TcpListener::bind(config.listen).context("Could not bind Sidecar's HTTPS port.")?;
            println!("Sidecar HTTPS listener: {}", listener.local_addr()?);
            for origin in &config.admin_origins {
                println!("Administration address: {origin}");
            }
            println!("This local certificate can produce a browser warning. Its certificate SHA-256 fingerprint is {}.", store.identity()?.certificate_sha256);
            if store.owner_generation()?.is_none() {
                print_code(&store.setup_code()?);
            }
            let shutdown = CancellationToken::new();
            let stopping = shutdown.clone();
            let signal = tokio::spawn(async move {
                #[cfg(unix)]
                {
                    let mut terminate =
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                            .expect("could not install termination handler");
                    tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
                }
                #[cfg(not(unix))]
                let _ = tokio::signal::ctrl_c().await;
                stopping.cancel();
            });
            let result = owner::serve(listener, service, shutdown).await;
            signal.abort();
            result?;
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn print_code(code: &str) {
    println!("Owner setup code (valid for 30 minutes): {code}");
    println!("Use this code in the browser to create your password. setup-code can replace an expired code.");
}
fn read_password(from_stdin: bool) -> Result<String> {
    let password = if from_stdin {
        let mut bytes = Vec::new();
        std::io::stdin().take(1027).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 1026, "Password input is too long.");
        let mut value = String::from_utf8(bytes).context("Password must be UTF-8.")?;
        if value.ends_with('\n') {
            value.pop();
            if value.ends_with('\r') {
                value.pop();
            }
        }
        value
    } else {
        ensure!(
            std::io::stdin().is_terminal(),
            "Use an interactive terminal, or --password-stdin with protected input."
        );
        let value = rpassword::prompt_password("New Sidecar owner password: ")?;
        let confirmation = rpassword::prompt_password("Confirm password: ")?;
        ensure!(
            value == confirmation,
            "Passwords do not match; nothing was changed."
        );
        value
    };
    owner::validate_password(&password)?;
    Ok(password)
}
