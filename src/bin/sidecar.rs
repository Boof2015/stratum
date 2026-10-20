use anyhow::{bail, ensure, Context, Result};
use std::{
    io::{IsTerminal, Read},
    net::TcpListener,
    path::PathBuf,
};
use stratum_poc::owner::{self, Config, Store};
use tokio_util::sync::CancellationToken;

const HELP: &str = "Astra Sidecar — development server

sidecar init --data-dir DIR [--listen IP:PORT] [--admin-origin https://HOST:PORT ...]
sidecar serve --data-dir DIR
sidecar set-password --data-dir DIR [--password-stdin]
sidecar reset-password --data-dir DIR [--password-stdin]
sidecar setup-code --data-dir DIR
sidecar renew-certificate --data-dir DIR
sidecar status --data-dir DIR
sidecar configure --data-dir DIR --settings FILE
sidecar configure-cache --data-dir DIR [--limit-gb N] [--enable | --disable --confirm]
sidecar clear-cache --data-dir DIR --confirm
sidecar check-navidrome --data-dir DIR [--url URL]
sidecar manual-discovery --data-dir DIR
sidecar publish-discovery --data-dir DIR --username NAME [--password-stdin] [--recreate]
sidecar remove-discovery --data-dir DIR --username NAME [--password-stdin]

init creates persistent configuration and HTTPS identity. serve never prompts;
before owner setup it prints a one-time code valid for 30 minutes. Password
commands prompt without echo by default. --password-stdin is for protected input
from automation; passwords are never accepted in command-line arguments.

configure imports connection settings after owner setup; check-navidrome detects
the API without signing in. Publishing/removing discovery asks for temporary
Navidrome administrator authorization. Configured servers serve experimental
authenticated audio over HTTPS, with configurable retained segments. Temporary
working storage is still needed when caching is disabled. The full dashboard follows.";

#[tokio::main(worker_threads = 2)]
async fn main() {
    if std::env::args().nth(1).as_deref() == Some("--audio-worker") {
        stratum_poc::access::audio::worker_main();
        return;
    }
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
            "configure-cache",
            "clear-cache",
            "check-navidrome",
            "manual-discovery",
            "publish-discovery",
            "remove-discovery"
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
    let mut username = None;
    let mut recreate = false;
    let mut cache_limit = None;
    let mut cache_enabled = None;
    let mut confirmed = false;
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
                if matches!(
                    command,
                    "set-password" | "reset-password" | "publish-discovery" | "remove-discovery"
                ) =>
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
            Some("--username")
                if matches!(command, "publish-discovery" | "remove-discovery")
                    && username.is_none() =>
            {
                username = Some(
                    args.next()
                        .and_then(|v| v.into_string().ok())
                        .context("--username needs a Navidrome administrator name.")?,
                );
            }
            Some("--limit-gb") if command == "configure-cache" && cache_limit.is_none() => {
                let gb: u64 = args
                    .next()
                    .and_then(|v| v.into_string().ok())
                    .context("--limit-gb needs a whole number of GB.")?
                    .parse()
                    .context("Use a whole number of GB.")?;
                cache_limit = Some(
                    gb.checked_mul(stratum_poc::cache::GB)
                        .context("Cache limit is too large.")?,
                );
            }
            Some("--enable" | "--disable")
                if command == "configure-cache" && cache_enabled.is_none() =>
            {
                cache_enabled = Some(flag == "--enable");
            }
            Some("--confirm") if matches!(command, "configure-cache" | "clear-cache") => {
                confirmed = true;
            }
            Some("--recreate") if command == "publish-discovery" => {
                recreate = true;
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
        "manual-discovery" | "publish-discovery" | "remove-discovery" => {
            let config = store.config()?;
            let discovery = store.discovery()?;
            let credentials = if command != "manual-discovery" {
                Some(owner::discovery::Credentials {
                    username: username
                        .context("Specify --username for the Navidrome administrator.")?,
                    password: read_navidrome_password(password_stdin)?,
                })
            } else {
                None
            };
            let operation =
                store.begin_discovery(&config.setup.revision, &discovery.revision, None)?;
            if let Some(credentials) = credentials {
                let action = if command == "publish-discovery" {
                    owner::discovery::Action::Publish
                } else {
                    owner::discovery::Action::Remove
                };
                operation
                    .run(action, credentials, recreate, CancellationToken::new())
                    .await?;
                println!("Discovery change verified on Navidrome. Administrator credentials were not saved.");
            } else {
                operation.manual()?;
                println!("Manual discovery selected. No announcement was published.");
            }
        }
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
        "configure-cache" | "clear-cache" => {
            ensure!(
                command == "clear-cache" || cache_enabled.is_some() || cache_limit.is_some(),
                "Choose a cache limit, --enable or --disable."
            );
            let current = store.config()?.storage;
            let saved = store.save_storage(
                &stratum_poc::cache::Change {
                    revision: current.revision,
                    enabled: cache_enabled.unwrap_or(current.enabled),
                    limit_bytes: cache_limit.unwrap_or(current.limit_bytes),
                    clear: command == "clear-cache",
                    confirmed,
                },
                None,
            )?;
            match store.server_lock() {
                Ok(_lease) => {
                    let cache = stratum_poc::cache::Cache::open(&store.cache_directory(), &saved)
                        .context("Settings saved, but retained audio could not be updated. Check storage access and retry.")?;
                    println!("{}", serde_json::to_string_pretty(&cache.stats())?);
                }
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock) =>
                {
                    println!("Cache settings saved. The running service will apply them shortly; active playback keeps its temporary files.");
                }
                Err(error) => {
                    return Err(error.context(
                        "Settings saved, but cache maintenance could not acquire the service lock.",
                    ))
                }
            }
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
                "discovery": store.discovery()?.view(&store.config()?.setup, &store.identity()?),
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

fn read_navidrome_password(from_stdin: bool) -> Result<String> {
    if from_stdin {
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
        Ok(value)
    } else {
        ensure!(
            std::io::stdin().is_terminal(),
            "Use a terminal or --password-stdin with protected input."
        );
        Ok(rpassword::prompt_password(
            "Navidrome administrator password (not saved): ",
        )?)
    }
}
