//! Owner-only radio announcement maintenance. No credentials are persisted;
//! an interrupted operation is reconciled against Navidrome before retrying.
use super::{
    setup::{Endpoint, Probe, Settings, SetupState},
    store, Identity, Store,
};
use anyhow::{ensure, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fs::File, time::Duration};
use tokio_util::sync::CancellationToken;

const MARKER: &str = "stratum-v1=";
const CAPABILITIES: &str = "/stratum/access/v1/capabilities";

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Manifest {
    pub version: u32,
    pub kind: String,
    pub installation: String,
    pub spki_sha256: String,
    pub endpoints: Vec<Endpoint>,
}
impl Manifest {
    pub fn new(settings: &Settings, identity: &Identity) -> Result<Self> {
        let settings = settings.normalized()?;
        ensure!(
            !settings.listeners.is_empty(),
            "Save listener addresses before publishing discovery."
        );
        let value = Self {
            version: 1,
            kind: "astra-sidecar".into(),
            installation: identity.installation_id.clone(),
            spki_sha256: identity.spki_sha256.clone(),
            endpoints: settings.listeners,
        };
        value.validate()?;
        Ok(value)
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1 && self.kind == "astra-sidecar",
            "Unsupported discovery metadata."
        );
        ensure!(
            hex(&self.installation, 32) && hex(&self.spki_sha256, 64),
            "Invalid discovery identity."
        );
        let normalized = Settings {
            navidrome_url: None,
            listeners: self.endpoints.clone(),
        }
        .normalized()?;
        ensure!(
            !self.endpoints.is_empty() && normalized.listeners == self.endpoints,
            "Invalid discovery addresses."
        );
        ensure!(self.descriptor().len() <= 16384, "Discovery addresses are too long to publish. Use fewer or shorter addresses, or manual setup.");
        Ok(())
    }
    pub fn stream_url(&self) -> String {
        format!("{}{CAPABILITIES}", self.endpoints[0].url)
    }
    pub fn descriptor(&self) -> String {
        let encoded: String = serde_json::to_vec(self)
            .expect("serializable manifest")
            .iter()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-._~".contains(b) {
                    (*b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        format!("{}#{MARKER}{encoded}", self.stream_url())
    }
    pub fn name(&self) -> String {
        format!("Stratum — {}", self.installation)
    }
}
fn hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn decode(value: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut input = value.bytes();
    while let Some(byte) = input.next() {
        bytes.push(if byte == b'%' {
            let a = (input.next()? as char).to_digit(16)?;
            let b = (input.next()? as char).to_digit(16)?;
            (a * 16 + b) as u8
        } else {
            byte
        });
    }
    String::from_utf8(bytes).ok()
}
#[derive(Clone, Deserialize)]
struct Radio {
    id: String,
    name: String,
    #[serde(rename = "streamUrl")]
    stream_url: String,
    #[serde(default, rename = "homePageUrl")]
    homepage: String,
}
impl Radio {
    fn manifest(&self) -> Option<Manifest> {
        if self.homepage.len() > 16384 {
            return None;
        }
        let (url, fragment) = self.homepage.split_once('#')?;
        if url != self.stream_url {
            return None;
        }
        let value: Manifest =
            serde_json::from_str(&decode(fragment.strip_prefix(MARKER)?)?).ok()?;
        value.validate().ok()?;
        if value.stream_url() != self.stream_url {
            return None;
        }
        Some(value)
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub navidrome_url: String,
    pub station_id: Option<String>,
    pub manifest: Manifest,
}
impl Record {
    fn validate(&self) -> Result<()> {
        ensure!(
            super::setup::normalize_navidrome(&self.navidrome_url)? == self.navidrome_url,
            "Invalid publication address."
        );
        if let Some(id) = &self.station_id {
            ensure!(
                !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control),
                "Invalid station ID."
            );
        }
        self.manifest.validate()
    }
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Publish,
    Remove,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub action: Action,
    pub target: Record,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Discovery {
    pub revision: String,
    pub manual: bool,
    pub record: Option<Record>,
    pub pending: Option<Attempt>,
    pub error: Option<Error>,
}
impl Default for Discovery {
    fn default() -> Self {
        Self {
            revision: "initial".into(),
            manual: false,
            record: None,
            pending: None,
            error: None,
        }
    }
}
impl Discovery {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.revision == "initial" || hex(&self.revision, 64),
            "Invalid discovery revision."
        );
        if let Some(record) = &self.record {
            record.validate()?;
        }
        if let Some(pending) = &self.pending {
            pending.target.validate()?;
        }
        Ok(())
    }
    pub fn view(&self, setup: &SetupState, identity: &Identity) -> Value {
        let status = if self.pending.is_some() {
            "unverified"
        } else if self.error.is_some() {
            "problem"
        } else if let Some(record) = &self.record {
            if setup.settings.navidrome_url.as_ref() == Some(&record.navidrome_url)
                && Manifest::new(&setup.settings, identity).ok().as_ref() == Some(&record.manifest)
            {
                "published"
            } else {
                "update-needed"
            }
        } else if self.manual {
            "manual"
        } else {
            "not-published"
        };
        json!({"revision":self.revision,"status":status,"error":self.error.map(Error::message),
            "canRecreate":self.error==Some(Error::Missing), "hasAnnouncement":self.record.is_some() || self.pending.is_some(),
            "target":self.pending.as_ref().map(|p|&p.target.navidrome_url).or(self.record.as_ref().map(|r|&r.navidrome_url)),
            "pendingAction":self.pending.as_ref().map(|p|p.action)})
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Error {
    Busy,
    Conflict,
    Setup,
    Authentication,
    NotAdmin,
    RateLimited,
    Network,
    Response,
    Duplicate,
    Missing,
    Ownership,
    PreviousServer,
    Interrupted,
    Unverified,
    Storage,
}
impl Error {
    pub fn message(self) -> &'static str {
        match self {
            Self::Busy=>"Another discovery operation is running. Wait for it to finish.",
            Self::Conflict=>"Settings changed elsewhere. Reload saved settings before trying again.",
            Self::Setup=>"Save a Navidrome address and listener addresses first. Very long address lists may need to be shortened for discovery.",
            Self::Authentication=>"Navidrome didn't accept this sign-in. Enter your Navidrome administrator username and password again.",
            Self::NotAdmin=>"This Navidrome account isn't an administrator. Use an administrator account to manage discovery, or choose manual setup.",
            Self::RateLimited=>"Navidrome is limiting sign-in attempts. Wait for its cooldown, then try again.",
            Self::Network=>"Couldn't complete the request to Navidrome. Check the connection and certificate, then retry with administrator sign-in.",
            Self::Response=>"Navidrome returned an unexpected response. Check the server and retry.",
            Self::Duplicate=>"Another or conflicting Stratum announcement exists. Check Navidrome's radio list; Sidecar won't overwrite an unrelated station.",
            Self::Missing=>"The saved announcement was removed from Navidrome. Choose Recreate announcement if you want it back.",
            Self::Ownership=>"The saved radio station no longer matches this Sidecar identity. Check it in Navidrome; it was left unchanged.",
            Self::PreviousServer=>"An announcement may still exist at the previous Navidrome address. Remove it using that address before publishing on this one.",
            Self::Interrupted=>"The operation was interrupted. Sign in again and retry to check Navidrome's actual state.",
            Self::Unverified=>"The change couldn't be verified. It may already have reached Navidrome. Retry to check before making another change.",
            Self::Storage=>"Sidecar couldn't save discovery state. Check its data directory before trying again.",
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}
impl std::error::Error for Error {}

// No Debug or serialization: retained only for one bounded operation.
pub struct Credentials {
    pub username: String,
    pub password: String,
}
impl Credentials {
    pub fn valid(&self) -> bool {
        !self.username.is_empty()
            && self.username.len() <= 256
            && !self.username.chars().any(char::is_control)
            && !self.password.is_empty()
            && self.password.len() <= 1024
    }
}
struct Admin {
    client: Client,
    base: String,
    username: String,
    token: String,
    salt: String,
}
impl Admin {
    async fn login(base: &str, credentials: Credentials) -> std::result::Result<Self, Error> {
        if !credentials.valid() {
            return Err(Error::Authentication);
        }
        Probe::new()
            .map_err(|_| Error::Network)?
            .check(base)
            .await
            .map_err(|_| Error::Network)?;
        let client = super::setup::Probe::builder()
            .build()
            .map_err(|_| Error::Network)?;
        let response = client
            .post(format!("{base}/auth/login"))
            .json(&json!({"username":credentials.username,"password":credentials.password}))
            .send()
            .await
            .map_err(|_| Error::Network)?;
        drop(credentials);
        let value = read_json(response).await?;
        if value.get("isAdmin").and_then(Value::as_bool) != Some(true) {
            return Err(Error::NotAdmin);
        }
        let text = |key: &str, max| {
            value
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control))
                .map(str::to_owned)
                .ok_or(Error::Response)
        };
        // The native login supplies ephemeral Subsonic credentials. The bearer
        // token is neither needed nor retained by this operation.
        Ok(Self {
            client,
            base: base.into(),
            username: text("username", 256)?,
            token: text("subsonicToken", 128)?,
            salt: text("subsonicSalt", 128)?,
        })
    }
    async fn call(
        &self,
        method: &str,
        extra: &[(&str, String)],
    ) -> std::result::Result<Value, Error> {
        let mut form = vec![
            ("f", "json".into()),
            ("v", "1.16.1".into()),
            ("c", "AstraSidecar".into()),
            ("u", self.username.clone()),
            ("t", self.token.clone()),
            ("s", self.salt.clone()),
        ];
        form.extend(extra.iter().cloned());
        let response = self
            .client
            .post(format!("{}/rest/{method}.view", self.base))
            .form(&form)
            .send()
            .await
            .map_err(|_| Error::Network)?;
        let value = read_json(response).await?;
        let body = value
            .get("subsonic-response")
            .filter(|b| {
                b.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.eq_ignore_ascii_case("navidrome"))
            })
            .ok_or(Error::Response)?;
        if body.get("status").and_then(Value::as_str) != Some("ok") {
            return Err(match body["error"]["code"].as_u64() {
                Some(40 | 41) => Error::Authentication,
                Some(50) => Error::NotAdmin,
                _ => Error::Response,
            });
        }
        Ok(body.clone())
    }
    async fn radios(&self) -> std::result::Result<Vec<Radio>, Error> {
        let value = self.call("getInternetRadioStations", &[]).await?;
        let group = value
            .get("internetRadioStations")
            .and_then(Value::as_object)
            .ok_or(Error::Response)?;
        let items = group
            .get("internetRadioStation")
            .cloned()
            .unwrap_or(json!([]));
        let radios: Vec<Radio> = serde_json::from_value(items).map_err(|_| Error::Response)?;
        if radios.len() > 4096 || radios.iter().any(|r| r.id.is_empty() || r.id.len() > 256) {
            return Err(Error::Response);
        }
        Ok(radios)
    }
}
async fn read_json(mut response: reqwest::Response) -> std::result::Result<Value, Error> {
    if response.status().as_u16() == 429 {
        return Err(Error::RateLimited);
    }
    if matches!(response.status().as_u16(), 401 | 403) {
        return Err(Error::Authentication);
    }
    if !response.status().is_success() {
        return Err(Error::Network);
    }
    if !response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| {
            s.split(';')
                .next()
                .is_some_and(|s| s.trim().eq_ignore_ascii_case("application/json"))
        })
    {
        return Err(Error::Response);
    }
    if response.content_length().is_some_and(|n| n > 1024 * 1024) {
        return Err(Error::Response);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Network)? {
        if bytes.len() + chunk.len() > 1024 * 1024 {
            return Err(Error::Response);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::Response)
}

pub struct Operation {
    pub(crate) store: Store,
    pub(crate) _lock: File,
    pub(crate) setup: SetupState,
    pub(crate) state: Discovery,
    pub(crate) identity: Identity,
    pub(crate) generation: String,
}
impl Operation {
    pub fn manual(mut self) -> Result<Discovery> {
        ensure!(
            self.state.record.is_none() && self.state.pending.is_none(),
            Error::Ownership
        );
        self.authorize(&CancellationToken::new())?;
        self.state.manual = true;
        self.state.error = None;
        self.save()?;
        Ok(self.state)
    }
    fn authorize(&self, cancel: &CancellationToken) -> std::result::Result<(), Error> {
        if cancel.is_cancelled()
            || self
                .store
                .owner_generation()
                .map_err(|_| Error::Storage)?
                .as_deref()
                != Some(&self.generation)
        {
            return Err(Error::Interrupted);
        }
        Ok(())
    }
    fn save(&mut self) -> std::result::Result<(), Error> {
        self.state.revision = store::random_hex(32).map_err(|_| Error::Storage)?;
        self.store
            .write_discovery(&self.state)
            .map_err(|_| Error::Storage)
    }
    pub async fn run(
        mut self,
        action: Action,
        credentials: Credentials,
        recreate: bool,
        cancel: CancellationToken,
    ) -> std::result::Result<Discovery, Error> {
        self.authorize(&cancel)?;
        let desired = || -> std::result::Result<Record, Error> {
            Ok(Record {
                navidrome_url: self
                    .setup
                    .settings
                    .navidrome_url
                    .clone()
                    .ok_or(Error::Setup)?,
                station_id: None,
                manifest: Manifest::new(&self.setup.settings, &self.identity)
                    .map_err(|_| Error::Setup)?,
            })
        };
        let previous = self
            .state
            .pending
            .as_ref()
            .map(|p| &p.target)
            .or(self.state.record.as_ref())
            .cloned();
        let mut target = if action == Action::Publish {
            desired()?
        } else {
            previous.clone().ok_or(Error::Missing)?
        };
        if let Some(previous) = previous {
            if previous.navidrome_url != target.navidrome_url {
                return Err(Error::PreviousServer);
            }
            target.station_id = previous.station_id;
        }
        self.state.error = None;
        let result = tokio::select! {
            _=cancel.cancelled()=>Err(Error::Interrupted),
            result=tokio::time::timeout(Duration::from_secs(25),self.execute(action,credentials,recreate,&target,&cancel))=>result.unwrap_or(Err(Error::Unverified)),
        };
        match result {
            Ok(record) => {
                self.state.record = record;
                self.state.pending = None;
                self.state.manual = action == Action::Remove;
                self.state.error = None;
            }
            Err(error) => {
                self.state.error = Some(error);
                self.save()?;
                return Err(error);
            }
        }
        self.save()?;
        Ok(self.state)
    }
    async fn execute(
        &mut self,
        action: Action,
        credentials: Credentials,
        recreate: bool,
        target: &Record,
        cancel: &CancellationToken,
    ) -> std::result::Result<Option<Record>, Error> {
        let admin = Admin::login(&target.navidrome_url, credentials).await?;
        self.authorize(cancel)?;
        let radios = admin.radios().await?;
        let selected = select(&radios, target, action, recreate)?;
        self.authorize(cancel)?;
        if action == Action::Remove {
            if let Some(radio) = selected {
                self.state.pending = Some(Attempt {
                    action,
                    target: target.clone(),
                });
                self.save()?;
                self.authorize(cancel)?;
                // A successful delete response alone isn't proof of removal.
                admin
                    .call("deleteInternetRadioStation", &[("id", radio.id.clone())])
                    .await
                    .map_err(|_| Error::Unverified)?;
            }
            let remaining = admin.radios().await.map_err(|_| Error::Unverified)?;
            if remaining.iter().any(|r| {
                r.manifest()
                    .is_some_and(|m| m.installation == target.manifest.installation)
                    || target.station_id.as_ref() == Some(&r.id)
            }) {
                return Err(Error::Unverified);
            }
            return Ok(None);
        }
        let mut target = target.clone();
        if selected.is_none() {
            target.station_id = None;
        }
        let mut fields = vec![
            ("name", target.manifest.name()),
            ("streamUrl", target.manifest.stream_url()),
            ("homepageUrl", target.manifest.descriptor()),
        ];
        if !selected.is_some_and(|r| r.manifest().as_ref() == Some(&target.manifest)) {
            self.state.pending = Some(Attempt {
                action,
                target: target.clone(),
            });
            self.save()?;
            self.authorize(cancel)?;
            let method = if let Some(radio) = selected {
                fields.push(("id", radio.id.clone()));
                "updateInternetRadioStation"
            } else {
                "createInternetRadioStation"
            };
            admin
                .call(method, &fields)
                .await
                .map_err(|_| Error::Unverified)?;
        }
        let verified = admin.radios().await.map_err(|_| Error::Unverified)?;
        let radio = select(&verified, &target, action, true)?.ok_or(Error::Unverified)?;
        if radio.manifest().as_ref() != Some(&target.manifest) {
            return Err(Error::Unverified);
        }
        let mut record = target.clone();
        record.station_id = Some(radio.id.clone());
        Ok(Some(record))
    }
}
fn select<'a>(
    radios: &'a [Radio],
    target: &Record,
    action: Action,
    recreate: bool,
) -> std::result::Result<Option<&'a Radio>, Error> {
    // ID alone can be reused; station name alone is never ownership proof.
    if let Some(id) = &target.station_id {
        if let Some(radio) = radios.iter().find(|r| &r.id == id) {
            if !radio.manifest().is_some_and(|m| {
                m.installation == target.manifest.installation
                    && m.spki_sha256 == target.manifest.spki_sha256
            }) {
                return Err(Error::Ownership);
            }
        }
    }
    let ours: Vec<_> = radios
        .iter()
        .filter(|r| {
            r.manifest().is_some_and(|m| {
                m.installation == target.manifest.installation
                    && m.spki_sha256 == target.manifest.spki_sha256
            })
        })
        .collect();
    if ours.len() > 1 {
        return Err(Error::Duplicate);
    }
    if action == Action::Publish
        && radios.iter().any(|r| {
            r.homepage.contains(&format!("#{MARKER}")) && !ours.iter().any(|own| own.id == r.id)
        })
    {
        return Err(Error::Duplicate);
    }
    if let Some(radio) = ours.first() {
        if target.station_id.as_ref().is_some_and(|id| id != &radio.id) {
            return Err(Error::Ownership);
        }
        return Ok(Some(radio));
    }
    if action == Action::Publish {
        if target.station_id.is_some() && !recreate {
            return Err(Error::Missing);
        }
        if radios.iter().any(|r| r.name == target.manifest.name()) {
            return Err(Error::Duplicate);
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests;
