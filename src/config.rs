use std::{collections::BTreeMap, env, net::SocketAddr, path::PathBuf, time::Duration};

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

const DEFAULT_CONFIG_PATH: &str = "cleanrr.toml";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub listen_addr: SocketAddr,
    #[serde(with = "humantime_serde")]
    pub poll_interval: Duration,
    #[serde(with = "humantime_serde")]
    pub minimum_age: Duration,
    pub dry_run: bool,
    pub remove_from_client: bool,
    pub servers: BTreeMap<String, ServerConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:8080".parse().expect("default address is valid"),
            poll_interval: Duration::from_secs(60),
            minimum_age: Duration::from_secs(30 * 60),
            dry_run: false,
            remove_from_client: false,
            servers: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServerKind {
    Radarr,
    Sonarr,
}

impl ServerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Radarr => "radarr",
            Self::Sonarr => "sonarr",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServerConfig {
    pub kind: ServerKind,
    pub url: Url,
    pub api_key: String,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not load configuration: {0}")]
    Load(#[source] Box<figment::Error>),
    #[error("configuration is invalid: {0}")]
    Invalid(String),
}

impl From<figment::Error> for ConfigError {
    fn from(error: figment::Error) -> Self {
        Self::Load(Box::new(error))
    }
}

impl Config {
    pub fn load() -> Result<Self, ConfigError> {
        let path = env::var_os("CLEANRR_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
        Self::load_from(path)
    }

    pub fn load_from(path: PathBuf) -> Result<Self, ConfigError> {
        let mut figment = Figment::from(Serialized::defaults(Self::default()));

        if path.exists() {
            figment = figment.merge(Toml::file(path));
        } else if env::var_os("CLEANRR_CONFIG").is_some() {
            return Err(ConfigError::Invalid(format!(
                "CLEANRR_CONFIG points to missing file {}",
                path.display()
            )));
        }

        let config: Self = figment
            .merge(Env::prefixed("CLEANRR_").ignore(&["CONFIG"]).split("__"))
            .extract()?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.poll_interval.is_zero() {
            return Err(ConfigError::Invalid(
                "poll_interval must be greater than zero".to_owned(),
            ));
        }
        if self.servers.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one [servers.<name>] entry is required".to_owned(),
            ));
        }
        for (name, server) in &self.servers {
            if name.trim().is_empty() {
                return Err(ConfigError::Invalid(
                    "server names cannot be empty".to_owned(),
                ));
            }
            if server.api_key.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "servers.{name}.api_key cannot be empty"
                )));
            }
            if !matches!(server.url.scheme(), "http" | "https") {
                return Err(ConfigError::Invalid(format!(
                    "servers.{name}.url must use http or https"
                )));
            }
            if server.url.host_str().is_none() {
                return Err(ConfigError::Invalid(format!(
                    "servers.{name}.url must include a host"
                )));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> Config {
        Config {
            servers: BTreeMap::from([(
                "movies".to_owned(),
                ServerConfig {
                    kind: ServerKind::Radarr,
                    url: Url::parse("http://radarr:7878").unwrap(),
                    api_key: "secret".to_owned(),
                },
            )]),
            ..Config::default()
        }
    }

    #[test]
    fn defaults_are_safe() {
        let config = valid_config();
        config.validate().unwrap();
        assert!(!config.remove_from_client);
        assert!(!config.dry_run);
    }

    #[test]
    fn rejects_non_http_server_url() {
        let mut config = valid_config();
        config.servers.get_mut("movies").unwrap().url = Url::parse("ftp://radarr.example").unwrap();
        assert!(matches!(config.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn toml_overrides_defaults() {
        let config: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(
                r#"
                poll_interval = "5m"
                minimum_age = "2h"

                [servers.tv]
                kind = "sonarr"
                url = "https://sonarr.example/base/"
                api_key = "secret"
                "#,
            ))
            .extract()
            .unwrap();

        assert_eq!(config.poll_interval, Duration::from_secs(300));
        assert_eq!(config.minimum_age, Duration::from_secs(7200));
        assert_eq!(config.servers["tv"].kind, ServerKind::Sonarr);
    }
}
