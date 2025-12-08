use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use proof_api_legacy::api::{ApiConfig, build_api};
use proof_api_legacy::client::LegacyClient;
use proof_api_util::api::Api;
use serde::{Deserialize, Serialize};
use tycho_util::cli::logger::LoggerConfig;

/// Run the API.
#[derive(Parser)]
pub struct Cmd {
    /// dump the template of the config
    #[clap(
        short = 'i',
        long,
        conflicts_with_all = ["config", "logger_config"]
    )]
    pub init_config: Option<PathBuf>,

    /// overwrite the existing config
    #[clap(short, long)]
    pub force: bool,

    /// path to the node config
    #[clap(long, required_unless_present = "init_config")]
    pub config: Option<PathBuf>,

    /// path to the logger config
    #[clap(long)]
    pub logger_config: Option<PathBuf>,
}

impl Cmd {
    pub async fn run(self) -> Result<()> {
        tycho_util::cli::logger::set_abort_with_tracing();

        if let Some(config_path) = self.init_config {
            if config_path.exists() && !self.force {
                anyhow::bail!("config file already exists, use --force to overwrite");
            }

            let config = Config::default();
            std::fs::write(config_path, serde_json::to_string_pretty(&config).unwrap())?;
            return Ok(());
        }

        let config = Config::load_from_file(self.config.as_ref().context("no config")?)?;
        tycho_util::cli::logger::init_logger(&config.logger_config, self.logger_config)?;

        let client =
            LegacyClient::new(config.client.url).context("failed to create legacy client")?;
        let api = Api::bind(
            config.api.listen_addr,
            build_api(&config.api, client).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .context("failed to bind API service")?;
        tracing::info!("created api");

        api.serve().await.map_err(Into::into)
    }
}

#[derive(Default, Debug, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    api: ApiConfig,
    client: LegacyClientConfig,
    logger_config: LoggerConfig,
}

impl Config {
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let data = std::fs::read(path).context("failed to read config")?;
        serde_json::from_slice(&data).context("failed to deserialize config")
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct LegacyClientConfig {
    url: String,
}

impl Default for LegacyClientConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost/graphql".to_owned(),
        }
    }
}
