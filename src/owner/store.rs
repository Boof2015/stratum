use anyhow::{ensure, Context, Result};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use fs2::FileExt;
use rcgen::{CertificateParams, KeyPair, PublicKeyData};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;

const FILE_LIMIT: u64 = 64 * 1024;
const BOOTSTRAP_SECONDS: u64 = 30 * 60;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub listen: SocketAddr,
    /// Exact browser origins, not upstream or listener discovery addresses.
    pub admin_origins: Vec<String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            listen: "127.0.0.1:7843".parse().unwrap(),
            admin_origins: vec![
                "https://localhost:7843".into(),
                "https://127.0.0.1:7843".into(),
            ],
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "Unsupported configuration version.");
        ensure!(
            !self.admin_origins.is_empty() && self.admin_origins.len() <= 16,
            "Configure between 1 and 16 administration origins."
        );
        for value in &self.admin_origins {
            let url = reqwest::Url::parse(value)
                .map_err(|_| anyhow::anyhow!("Invalid administration origin."))?;
            ensure!(
                value.len() <= 512
                    && url.scheme() == "https"
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && url.path() == "/"
                    && url.origin().ascii_serialization() == *value,
                "Administration origins must be HTTPS origins without a path, credentials, query or fragment."
            );
        }
        Ok(())
    }
    pub(crate) fn accepts_host(&self, host: &str) -> bool {
        self.admin_origins.iter().any(|origin| {
            // Compare normalized origins so the implicit HTTPS port also works.
            reqwest::Url::parse(&format!("https://{host}"))
                .ok()
                .filter(|url| {
                    url.username().is_empty()
                        && url.password().is_none()
                        && url.path() == "/"
                        && url.query().is_none()
                        && url.fragment().is_none()
                })
                .is_some_and(|url| url.origin().ascii_serialization() == *origin)
        })
    }
    fn certificate_names(&self) -> Vec<String> {
        self.admin_origins
            .iter()
            .map(|origin| {
                reqwest::Url::parse(origin)
                    .unwrap()
                    .host_str()
                    .unwrap()
                    .trim_matches(['[', ']'])
                    .to_owned()
            })
            .collect()
    }
}

// No Debug: these structures contain private material.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrivateState {
    version: u32,
    installation_id: String,
    key_pem: String,
    certificate_pem: String,
    owner: Option<Owner>,
    bootstrap: Option<Bootstrap>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    password_hash: String,
    generation: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bootstrap {
    digest: String,
    expires_at: u64,
}

#[derive(Clone)]
pub struct Store {
    directory: PathBuf,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub installation_id: String,
    pub spki_sha256: String,
    pub certificate_sha256: String,
}

impl Store {
    /// Initialization is explicit. Opening a damaged installation never silently
    /// creates a new identity or reopens owner enrollment.
    pub fn initialize(directory: &Path, config: &Config) -> Result<Self> {
        config.validate()?;
        prepare_directory(directory, true)?;
        let store = Self {
            directory: directory.to_owned(),
        };
        let _lock = store.lock("state.lock")?;
        if store.directory.join("private.json").try_exists()? {
            let state = store.read_state()?;
            tls_config(&state)?;
            // Recover a crash between the two initial atomic writes, preserving
            // the private identity. Existing configuration is never overwritten.
            if !store.directory.join("config.json").try_exists()? {
                store.write("config.json", config)?;
            }
            store.config()?;
        } else {
            ensure!(
                !store.directory.join("config.json").try_exists()?,
                "Private state is missing. Restore it from backup; initialization will not replace this identity."
            );
            let key = KeyPair::generate()?;
            let state = PrivateState {
                version: 1,
                installation_id: random_hex(16)?,
                certificate_pem: certificate(config, &key)?,
                key_pem: key.serialize_pem(),
                owner: None,
                bootstrap: None,
            };
            store.write("private.json", &state)?;
            store.write("config.json", config)?;
        }
        Ok(store)
    }

    pub fn open(directory: &Path) -> Result<Self> {
        prepare_directory(directory, false)?;
        let store = Self {
            directory: directory.to_owned(),
        };
        let _lock = store.lock("state.lock")?;
        store.config()?;
        tls_config(&store.read_state()?)?;
        Ok(store)
    }

    pub fn config(&self) -> Result<Config> {
        let config: Config = self.read("config.json")?;
        config.validate()?;
        Ok(config)
    }

    pub fn identity(&self) -> Result<Identity> {
        let state = self.read_state()?;
        let key = KeyPair::from_pem(&state.key_pem)?;
        let cert = CertificateDer::from_pem_slice(state.certificate_pem.as_bytes())?;
        Ok(Identity {
            installation_id: state.installation_id,
            spki_sha256: digest(&key.subject_public_key_info()),
            certificate_sha256: digest(cert.as_ref()),
        })
    }

    pub fn tls_config(&self) -> Result<Arc<rustls::ServerConfig>> {
        Ok(Arc::new(tls_config(&self.read_state()?)?))
    }

    /// Renewal uses the same key; changing the certificate does not rotate the
    /// advertised SPKI identity. A running server uses the new cert after restart.
    pub fn renew_certificate(&self) -> Result<()> {
        let _lock = self.lock("state.lock")?;
        let mut state = self.read_state()?;
        let key = KeyPair::from_pem(&state.key_pem)?;
        state.certificate_pem = certificate(&self.config()?, &key)?;
        self.write("private.json", &state)
    }

    pub fn owner_generation(&self) -> Result<Option<String>> {
        Ok(self.read_state()?.owner.map(|owner| owner.generation))
    }

    /// Only local callers receive the plaintext code. Rotating it invalidates
    /// any previous code, including one left in earlier container logs.
    pub fn setup_code(&self) -> Result<String> {
        let _lock = self.lock("state.lock")?;
        let mut state = self.read_state()?;
        ensure!(state.owner.is_none(), "An owner password already exists.");
        let code = random_hex(32)?;
        state.bootstrap = Some(Bootstrap {
            digest: digest(code.as_bytes()),
            expires_at: now()? + BOOTSTRAP_SECONDS,
        });
        self.write("private.json", &state)?;
        Ok(code)
    }

    pub(crate) fn claim(&self, code: &str, password: &str) -> Result<Option<String>> {
        let _lock = self.lock("state.lock")?;
        let mut state = self.read_state()?;
        if state.owner.is_some() || !valid_hex(code, 64) {
            return Ok(None);
        }
        let Some(bootstrap) = &state.bootstrap else {
            return Ok(None);
        };
        if now()? >= bootstrap.expires_at
            || !bool::from(
                digest(code.as_bytes())
                    .as_bytes()
                    .ct_eq(bootstrap.digest.as_bytes()),
            )
        {
            return Ok(None);
        }
        let owner = make_owner(password)?;
        let generation = owner.generation.clone();
        state.owner = Some(owner);
        state.bootstrap = None;
        self.write("private.json", &state)?;
        Ok(Some(generation))
    }

    pub(crate) fn login(&self, password: &str) -> Result<Option<String>> {
        let state = self.read_state()?;
        let Some(owner) = state.owner else {
            return Ok(None);
        };
        let parsed = PasswordHash::new(&owner.password_hash)
            .map_err(|_| anyhow::anyhow!("Invalid owner password record."))?;
        Ok(Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
            .then_some(owner.generation))
    }

    /// Local command only. Reset requires an existing owner; first enrollment
    /// uses the separate set-password command or the one-time browser code.
    pub fn set_password(&self, password: &str, reset: bool) -> Result<()> {
        let owner = make_owner(password)?;
        let _lock = self.lock("state.lock")?;
        let mut state = self.read_state()?;
        ensure!(
            state.owner.is_some() == reset,
            "Use set-password for first setup, or reset-password for an existing owner."
        );
        state.owner = Some(owner);
        state.bootstrap = None;
        self.write("private.json", &state)
    }

    pub fn server_lock(&self) -> Result<File> {
        let file = self.lock_file("serve.lock")?;
        FileExt::try_lock_exclusive(&file)
            .context("Another Sidecar process is using this data directory.")?;
        Ok(file)
    }

    fn lock(&self, name: &str) -> Result<File> {
        let file = self.lock_file(name)?;
        FileExt::lock_exclusive(&file)?;
        Ok(file)
    }
    fn lock_file(&self, name: &str) -> Result<File> {
        let file = private_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.directory.join(name))?;
        check_private_file(&file)?;
        Ok(file)
    }
    fn read_state(&self) -> Result<PrivateState> {
        let state: PrivateState = self.read("private.json")?;
        ensure!(state.version == 1, "Unsupported private-state version.");
        ensure!(
            valid_hex(&state.installation_id, 32),
            "Invalid installation identity."
        );
        if let Some(owner) = &state.owner {
            ensure!(
                valid_hex(&owner.generation, 64),
                "Invalid owner generation."
            );
            // Bound the persisted hash's work parameters before using them.
            // Future parameter upgrades require an explicit schema migration.
            let hash = PasswordHash::new(&owner.password_hash)
                .map_err(|_| anyhow::anyhow!("Invalid owner password record."))?;
            ensure!(
                hash.algorithm.as_str() == "argon2id"
                    && hash.version == Some(19)
                    && hash.params.get_decimal("m") == Some(19456)
                    && hash.params.get_decimal("t") == Some(2)
                    && hash.params.get_decimal("p") == Some(1)
                    && hash.salt.is_some()
                    && hash.hash.is_some(),
                "Unsupported owner password parameters."
            );
            ensure!(state.bootstrap.is_none(), "Invalid owner enrollment state.");
        }
        if let Some(bootstrap) = &state.bootstrap {
            ensure!(
                valid_hex(&bootstrap.digest, 64),
                "Invalid setup-code record."
            );
        }
        Ok(state)
    }
    fn read<T: DeserializeOwned>(&self, name: &str) -> Result<T> {
        let file = private_options()
            .read(true)
            .open(self.directory.join(name))
            .context("Could not read Sidecar state. Check its data directory.")?;
        check_private_file(&file)?;
        let mut bytes = Vec::new();
        file.take(FILE_LIMIT + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= FILE_LIMIT,
            "Sidecar state file is too large."
        );
        serde_json::from_slice(&bytes).map_err(|_| {
            anyhow::anyhow!("Invalid Sidecar state file; restore or correct it before starting.")
        })
    }
    fn write<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        let mut temporary = tempfile::NamedTempFile::new_in(&self.directory)?;
        serde_json::to_writer_pretty(&mut temporary, value)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.directory.join(name))
            .map_err(|_| anyhow::anyhow!("Could not save Sidecar state."))?;
        #[cfg(unix)]
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
}

pub fn validate_password(password: &str) -> Result<()> {
    ensure!(
        password.chars().count() >= 12
            && password.len() <= 1024
            && !password.chars().all(char::is_whitespace)
            && !password.chars().any(char::is_control),
        "Use a password of at least 12 characters, up to 1024 bytes, without control characters."
    );
    Ok(())
}
fn make_owner(password: &str) -> Result<Owner> {
    validate_password(password)?;
    let mut salt = [0_u8; 16];
    getrandom::fill(&mut salt).map_err(|_| anyhow::anyhow!("Randomness unavailable."))?;
    let salt = SaltString::encode_b64(&salt)
        .map_err(|_| anyhow::anyhow!("Could not generate password salt."))?;
    let password_hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|_| anyhow::anyhow!("Could not hash the owner password."))?
        .to_string();
    Ok(Owner {
        password_hash,
        generation: random_hex(32)?,
    })
}
fn certificate(config: &Config, key: &KeyPair) -> Result<String> {
    let mut params = CertificateParams::new(config.certificate_names())?;
    params.not_before = time::OffsetDateTime::now_utc() - time::Duration::days(1);
    params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(365);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Astra Sidecar");
    Ok(params.self_signed(key)?.pem())
}
fn tls_config(state: &PrivateState) -> Result<rustls::ServerConfig> {
    let cert = CertificateDer::from_pem_slice(state.certificate_pem.as_bytes())?;
    let key = PrivateKeyDer::from_pem_slice(state.key_pem.as_bytes())?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_single_cert(vec![cert], key)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}
pub(crate) fn random_hex(length: usize) -> Result<String> {
    let mut bytes = vec![0; length];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("Randomness unavailable."))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
pub(crate) fn digest(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}
fn valid_hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|c| c.is_ascii_hexdigit())
}
fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

#[cfg(unix)]
fn private_options() -> OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    options
}
#[cfg(not(unix))]
fn private_options() -> OpenOptions {
    OpenOptions::new()
}

fn prepare_directory(path: &Path, create: bool) -> Result<()> {
    #[cfg(not(unix))]
    anyhow::bail!("This Sidecar checkpoint supports Unix data-directory permissions only.");
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        if create {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
        }
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_dir()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "Use a private data directory owned by the Sidecar service account (mode 700)."
        );
        Ok(())
    }
}
fn check_private_file(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "Sidecar state must be a regular file.");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "Sidecar state must be private to its service account (mode 600)."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
