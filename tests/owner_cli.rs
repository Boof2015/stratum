//! Bounded subprocess/TLS checks of the actual binary. No GUI, music or existing
//! installation is used. Every child is killed/reaped by its guard on failure.
#![cfg(unix)]
use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName};
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

const PASSWORD: &str = "disposable owner password";
const NEW_PASSWORD: &str = "replacement owner password";

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn wait(child: &mut Child, limit: Duration) -> ExitStatus {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "Sidecar subprocess exceeded its deadline"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn command(dir: &Path, args: &[&str], input: Option<&str>) -> (bool, String) {
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_sidecar"))
            .args(args)
            .arg("--data-dir")
            .arg(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    if let Some(input) = input {
        child
            .0
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    drop(child.0.stdin.take());
    let status = wait(&mut child.0, Duration::from_secs(10));
    let mut out = String::new();
    child
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    let mut error = String::new();
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut error)
        .unwrap();
    assert!(!out.contains(PASSWORD) && !error.contains(PASSWORD));
    assert!(!out.contains(NEW_PASSWORD) && !error.contains(NEW_PASSWORD));
    (status.success(), out)
}
fn roots(dir: &Path) -> Arc<rustls::ClientConfig> {
    let state: Value =
        serde_json::from_slice(&fs::read(dir.join("private.json")).unwrap()).unwrap();
    let cert =
        CertificateDer::from_pem_slice(state["certificate_pem"].as_str().unwrap().as_bytes())
            .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    Arc::new(config)
}
struct HttpResponse {
    status: u16,
    head: String,
    body: String,
}
fn request(
    addr: SocketAddr,
    tls: Arc<rustls::ClientConfig>,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Value,
) -> HttpResponse {
    let connection =
        rustls::ClientConnection::new(tls, ServerName::try_from("localhost").unwrap()).unwrap();
    let socket = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut stream = rustls::StreamOwned::new(connection, socket);
    let body = body.to_string();
    let cookie_header = cookie
        .map(|v| format!("Cookie: {v}\r\n"))
        .unwrap_or_default();
    write!(stream,"{method} {path} HTTP/1.1\r\nHost: localhost:7843\r\nOrigin: https://localhost:7843\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{cookie_header}Connection: close\r\n\r\n{body}",body.len()).unwrap();
    stream.flush().unwrap();
    let mut response = String::new();
    stream
        .take(32 * 1024)
        .read_to_string(&mut response)
        .unwrap();
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("complete HTTP response");
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    HttpResponse {
        status,
        head: head.into(),
        body: body.into(),
    }
}
fn cookie(response: &HttpResponse) -> String {
    response
        .head
        .lines()
        .find_map(|line| line.strip_prefix("set-cookie: "))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .into()
}
fn start(dir: &Path, log: &Path) -> (ChildGuard, SocketAddr, Option<String>) {
    let output = fs::File::create(log).unwrap();
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_sidecar"))
            .args(["serve", "--data-dir"])
            .arg(dir)
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.0.try_wait().unwrap().is_some() {
            let log = fs::read_to_string(log).unwrap();
            let safe = log
                .lines()
                .filter(|line| !line.starts_with("Owner setup code"))
                .collect::<Vec<_>>()
                .join("\n");
            panic!("Sidecar exited during startup: {safe}");
        }
        let text = fs::read_to_string(log).unwrap();
        if text.contains("fingerprint is") {
            let addr = text
                .lines()
                .find_map(|line| line.strip_prefix("Sidecar HTTPS listener: "))
                .unwrap()
                .parse()
                .unwrap();
            let code = text
                .lines()
                .find_map(|line| line.strip_prefix("Owner setup code (valid for 30 minutes): "))
                .map(str::to_owned);
            // First launch prints the code after the certificate line.
            let configured =
                serde_json::from_slice::<Value>(&fs::read(dir.join("private.json")).unwrap())
                    .unwrap()["owner"]
                    .is_object();
            if configured || code.is_some() {
                return (child, addr, code);
            }
        }
        assert!(
            Instant::now() < deadline,
            "Sidecar did not report readiness"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn stop(child: &mut ChildGuard) {
    unsafe {
        libc::kill(child.0.id() as i32, libc::SIGTERM);
    }
    assert!(wait(&mut child.0, Duration::from_secs(8)).success());
}

#[test]
fn actual_cli_and_https_preserve_identity_and_revoke_owner_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("data");
    assert!(
        command(
            &dir,
            &[
                "init",
                "--listen",
                "127.0.0.1:0",
                "--admin-origin",
                "https://localhost:7843"
            ],
            None
        )
        .0
    );
    let identity: Value = serde_json::from_str(&command(&dir, &["status"], None).1).unwrap();
    let tls = roots(&dir);
    let (mut server, addr, code) = start(&dir, &temp.path().join("first.log"));
    assert!(
        !command(&dir, &["serve"], None).0,
        "a second process must not serve this directory"
    );
    assert_eq!(
        request(
            addr,
            tls.clone(),
            "GET",
            "/admin/api/setup",
            None,
            json!({})
        )
        .status,
        401
    );
    let response = request(
        addr,
        tls.clone(),
        "POST",
        "/admin/api/claim",
        None,
        json!({"code":code.unwrap(),"password":PASSWORD}),
    );
    assert_eq!(response.status, 200);
    let first_cookie = cookie(&response);
    let response = request(
        addr,
        tls.clone(),
        "GET",
        "/admin/api/setup",
        Some(&first_cookie),
        json!({}),
    );
    assert_eq!(response.status, 200);
    assert!(response.body.contains("owner-ready"));
    assert!(
        command(
            &dir,
            &["reset-password", "--password-stdin"],
            Some(NEW_PASSWORD)
        )
        .0
    );
    assert_eq!(
        request(
            addr,
            tls.clone(),
            "GET",
            "/admin/api/setup",
            Some(&first_cookie),
            json!({})
        )
        .status,
        401
    );
    assert_eq!(
        request(
            addr,
            tls.clone(),
            "POST",
            "/admin/api/login",
            None,
            json!({"password":PASSWORD})
        )
        .status,
        401
    );
    let response = request(
        addr,
        tls.clone(),
        "POST",
        "/admin/api/login",
        None,
        json!({"password":NEW_PASSWORD}),
    );
    assert_eq!(response.status, 200);
    let next_cookie = cookie(&response);
    let after: Value = serde_json::from_str(&command(&dir, &["status"], None).1).unwrap();
    assert_eq!(identity["identity"], after["identity"]);
    assert_eq!(identity["config"], after["config"]);
    stop(&mut server);
    let (mut restarted, addr, code) = start(&dir, &temp.path().join("restart.log"));
    assert!(code.is_none(), "restart must not reopen enrollment");
    assert_eq!(
        request(
            addr,
            tls.clone(),
            "GET",
            "/admin/api/setup",
            Some(&next_cookie),
            json!({})
        )
        .status,
        401
    );
    assert_eq!(
        request(
            addr,
            tls,
            "POST",
            "/admin/api/login",
            None,
            json!({"password":NEW_PASSWORD})
        )
        .status,
        200
    );
    stop(&mut restarted);
}

#[test]
fn cli_setup_is_noninteractive_when_requested_and_rejects_accidental_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("data");
    assert!(!command(&dir, &["serve"], None).0);
    assert!(command(&dir, &["init"], None).0);
    assert!(
        !command(&dir, &["set-password"], None).0,
        "a service must never wait on an interactive prompt"
    );
    assert!(command(&dir, &["set-password", "--password-stdin"], Some(PASSWORD)).0);
    assert!(
        !command(
            &dir,
            &["set-password", "--password-stdin"],
            Some(NEW_PASSWORD)
        )
        .0
    );
    assert!(!command(&dir, &["setup-code"], None).0);
    assert!(!command(&dir, &["reset-password", "--password-stdin"], Some("short")).0);
    assert!(command(&dir, &["init"], None).0);
    assert!(!fs::read_to_string(dir.join("private.json"))
        .unwrap()
        .contains(PASSWORD));
}
