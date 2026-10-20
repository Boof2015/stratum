//! Shared owner setup model. These are owner-entered addresses, not discovered
//! redirects. A successful unauthenticated probe does not grant music access.
use anyhow::{ensure, Result};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, error::Error, net::IpAddr, time::Duration};

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub navidrome_url: Option<String>,
    #[serde(default)]
    pub listeners: Vec<Endpoint>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SetupState {
    pub revision: String,
    pub settings: Settings,
}
impl Default for SetupState {
    fn default() -> Self {
        Self {
            revision: "initial".into(),
            settings: Settings::default(),
        }
    }
}
impl SetupState {
    pub fn stage(&self) -> &'static str {
        if self.settings.navidrome_url.is_none() {
            "navidrome"
        } else if self.settings.listeners.is_empty() {
            "listeners"
        } else {
            "connections-saved"
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.revision == "initial"
                || (self.revision.len() == 64
                    && self.revision.bytes().all(|byte| byte.is_ascii_hexdigit())),
            "Invalid setup revision."
        );
        self.settings.normalized()?;
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub url: String,
    pub trust: EndpointTrust,
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EndpointTrust {
    SidecarKey,
    HttpsProxy,
}

impl Settings {
    pub fn normalized(&self) -> Result<Self> {
        let navidrome_url = self
            .navidrome_url
            .as_deref()
            .map(normalize_navidrome)
            .transpose()?;
        ensure!(
            self.listeners.len() <= 16,
            "Use at most 16 listener addresses."
        );
        let mut seen = HashSet::new();
        let mut listeners = Vec::new();
        for endpoint in &self.listeners {
            let url = normalize_url(&endpoint.url, true)?;
            ensure!(
                seen.insert(url.clone()),
                "Each listener address must be different."
            );
            listeners.push(Endpoint {
                url,
                trust: endpoint.trust,
            });
        }
        Ok(Self {
            navidrome_url,
            listeners,
        })
    }
}
pub fn normalize_navidrome(value: &str) -> Result<String> {
    normalize_url(value, false)
}
fn normalize_url(value: &str, listener: bool) -> Result<String> {
    let value = value.trim();
    ensure!(
        !value.is_empty()
            && value.len() <= 1024
            && !value.chars().any(char::is_control)
            && !value.contains('\\'),
        "Enter a complete server address."
    );
    let mut url = Url::parse(value).map_err(|_| {
        anyhow::anyhow!("Enter a complete server address, starting with http:// or https://.")
    })?;
    ensure!(
        url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Server addresses cannot include credentials, query parameters or fragments."
    );
    ensure!(
        url.scheme() == "https" || (!listener && url.scheme() == "http"),
        if listener {
            "Listener addresses must use HTTPS."
        } else {
            "Navidrome addresses must use HTTP or HTTPS."
        }
    );
    if let Ok(ip) = url
        .host_str()
        .unwrap()
        .trim_matches(['[', ']'])
        .parse::<IpAddr>()
    {
        ensure!(
            !ip.is_unspecified(),
            "Use a reachable host address, not 0.0.0.0 or ::."
        );
    }
    ensure!(
        url.port() != Some(0),
        "Use a nonzero port in the server address."
    );
    // Preserve a reverse-proxy prefix. Removing trailing slashes makes duplicate
    // detection deterministic without treating a leading /rest path as a base.
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&path);
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

#[derive(Clone, Serialize)]
pub struct Detection {
    pub url: String,
    pub server_version: Option<String>,
    pub authenticated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeFailure {
    Address,
    Connect,
    Timeout,
    SecureConnection,
    Redirect,
    Access,
    Http(u16),
    NotNavidrome,
    Version,
    Response,
}
impl ProbeFailure {
    pub fn message(self) -> &'static str {
        match self {
            Self::Address => "Check the Navidrome address, including http:// or https:// and any path.",
            Self::Connect => "Couldn't connect to Navidrome. Check its address, your network, and that the server is running.",
            Self::Timeout => "Navidrome didn't answer in time. Try again when the server is reachable.",
            Self::SecureConnection => "Couldn't establish a secure connection to Navidrome. Check that the address uses HTTPS and that its certificate is trusted.",
            Self::Redirect => "That address redirects elsewhere. Use the final Navidrome address, including any path.",
            Self::Access => "The server requires sign-in before its API can be reached. Use an address Sidecar can reach directly.",
            Self::Http(_) => "The server answered with an HTTP error. Check that Navidrome and its proxy are working.",
            Self::NotNavidrome => "The server answered, but didn't identify itself as Navidrome. Check the address, including any path such as /music.",
            Self::Version => "Navidrome reported an incompatible API version. Check the server version before continuing.",
            Self::Response => "Navidrome's response couldn't be read. Check the address and proxy configuration.",
        }
    }
}
impl std::fmt::Display for ProbeFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}
impl Error for ProbeFailure {}

#[derive(Clone)]
pub struct Probe {
    client: Client,
}
impl Probe {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: Self::builder().build()?,
        })
    }
    fn builder() -> reqwest::ClientBuilder {
        Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(6))
            .pool_max_idle_per_host(1)
            .user_agent("Astra-Sidecar/0.1")
    }
    pub async fn check(&self, input: &str) -> std::result::Result<Detection, ProbeFailure> {
        let url = normalize_navidrome(input).map_err(|_| ProbeFailure::Address)?;
        let target = format!("{url}/rest/ping.view");
        // No user, token, password, authorization header, cookies or credentials
        // from the dashboard. A normal Navidrome rejection identifies the API.
        let mut response = self
            .client
            .post(target)
            .form(&[("f", "json"), ("v", "1.16.1"), ("c", "AstraSidecar")])
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        if status.is_redirection() {
            return Err(ProbeFailure::Redirect);
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(ProbeFailure::Access);
        }
        if !status.is_success() {
            return Err(ProbeFailure::Http(status.as_u16()));
        }
        if !response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
            })
        {
            return Err(ProbeFailure::NotNavidrome);
        }
        if response
            .content_length()
            .is_some_and(|length| length > 65536)
        {
            return Err(ProbeFailure::Response);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(network_error)? {
            if bytes.len() + chunk.len() > 65536 {
                return Err(ProbeFailure::Response);
            }
            bytes.extend_from_slice(&chunk);
        }
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| ProbeFailure::Response)?;
        let envelope = body
            .get("subsonic-response")
            .ok_or(ProbeFailure::NotNavidrome)?;
        if !envelope
            .get("type")
            .and_then(|v| v.as_str())
            .is_some_and(|v| v.eq_ignore_ascii_case("navidrome"))
        {
            return Err(ProbeFailure::NotNavidrome);
        }
        match envelope.get("status").and_then(|v| v.as_str()) {
            Some("ok") => {}
            Some("failed") => match envelope
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_u64())
            {
                Some(10 | 40 | 41 | 50) => {}
                Some(20 | 30) => return Err(ProbeFailure::Version),
                _ => return Err(ProbeFailure::Response),
            },
            _ => return Err(ProbeFailure::Response),
        }
        let server_version = envelope
            .get("serverVersion")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty() && v.len() <= 80 && !v.chars().any(char::is_control))
            .map(str::to_owned);
        Ok(Detection {
            url,
            server_version,
            authenticated: false,
        })
    }
}
fn network_error(error: reqwest::Error) -> ProbeFailure {
    if error.is_timeout() {
        return ProbeFailure::Timeout;
    }
    let mut source: Option<&(dyn Error + 'static)> = Some(&error);
    while let Some(error) = source {
        if error.downcast_ref::<rustls::Error>().is_some() {
            return ProbeFailure::SecureConnection;
        }
        // io::Error::source() can skip the wrapped error itself, including
        // rustls::Error. Inspect the inner value before following its source.
        source = error
            .downcast_ref::<std::io::Error>()
            .and_then(|io| io.get_ref().map(|inner| inner as &(dyn Error + 'static)))
            .or_else(|| error.source());
    }
    ProbeFailure::Connect
}

#[cfg(test)]
mod tests;
