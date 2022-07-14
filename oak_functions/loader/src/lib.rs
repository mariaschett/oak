//
// Copyright 2021 The Project Oak Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

#![feature(async_closure)]

extern crate alloc;

pub mod grpc;
pub mod logger;
pub mod lookup_data;
pub mod server;

use crate::{
    grpc::{create_and_start_grpc_server, create_wasm_handler},
    logger::Logger,
    lookup_data::{LookupDataAuth, LookupDataRefresher, LookupDataSource},
    server::Policy,
};
use anyhow::Context;
use clap::Parser;
use log::Level;
use oak_functions_extension::ExtensionFactory;
use oak_functions_lookup::{LookupDataManager, LookupFactory};
use oak_functions_workload_logging::WorkloadLoggingFactory;
use oak_logger::OakLogger;
use serde_derive::Deserialize;
use std::{
    fs,
    net::{Ipv6Addr, SocketAddr},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

#[cfg(test)]
mod tests;

// Instantiate BoxedExtensionFactory with Logger from the Oak Functions runtime.
pub type OakFunctionsBoxedExtensionFactory = Box<dyn ExtensionFactory<Logger>>;

/// Command line options specificing how to run the Oak Functions Runtime, which are set by the team
/// operating Oak Functions Runtime as the platform.
///
/// On the other hand, set by the team using the Oak Functions Runtime for their business logic,
/// is the Config.
#[derive(Parser, Clone, Debug)]
#[clap(about = "Oak Functions Loader")]
pub struct Opt {
    #[clap(
        long,
        default_value = "8080",
        help = "Port number that the server listens on."
    )]
    pub http_listen_port: u16,
    #[clap(
        long,
        help = "Path to a file containing configuration parameters in TOML format."
    )]
    pub config_path: String,
}

async fn background_refresh_lookup_data(
    lookup_data_refresher: &LookupDataRefresher,
    period: Duration,
    logger: &Logger,
) {
    // Create an interval that starts after `period`, since the data was already refreshed
    // initially.
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    loop {
        interval.tick().await;
        // If there is an error, we skip the current refresh and wait for the next tick.
        if let Err(err) = lookup_data_refresher.refresh().await {
            logger.log_public(
                Level::Error,
                &format!("error refreshing lookup data: {}", err),
            );
        }
    }
}

/// This crate is just a library so this function does not get executed directly by anything, it
/// needs to be wrapped in the "actual" `main` from a bin crate.
pub fn lib_main(
    logger: Logger,
    load_lookup_data_config: LoadLookupDataConfig,
    policy: Option<Policy>,
    wasm_path: String,
    http_listen_port: u16,
    extension_factories: Vec<Box<dyn ExtensionFactory<Logger>>>,
) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async_main(
            logger,
            load_lookup_data_config,
            policy,
            wasm_path,
            http_listen_port,
            extension_factories,
        ))
}

/// Main execution point for the Oak Functions Loader.
async fn async_main(
    logger: Logger,
    load_lookup_data_config: LoadLookupDataConfig,
    policy: Option<Policy>,
    wasm_path: String,
    http_listen_port: u16,
    extension_factories: Vec<Box<dyn ExtensionFactory<Logger>>>,
) -> anyhow::Result<()> {
    let (notify_sender, notify_receiver) = tokio::sync::oneshot::channel::<()>();

    let wasm_module_bytes =
        fs::read(&wasm_path).with_context(|| format!("Couldn't read Wasm file {}", wasm_path))?;
    let mut extensions =
        create_base_extension_factories(load_lookup_data_config, logger.clone()).await?;

    for extension_factory in extension_factories {
        extensions.push(extension_factory);
    }

    let wasm_handler = create_wasm_handler(&wasm_module_bytes, extensions, logger.clone())?;

    // Make sure that a policy is specified and is valid.
    let policy = policy
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("a valid policy must be provided"))
        .and_then(|policy| policy.validate())?;

    let address = SocketAddr::from((Ipv6Addr::UNSPECIFIED, http_listen_port));

    // Start server.
    let server_handle = tokio::spawn(async move {
        create_and_start_grpc_server(
            &address,
            wasm_handler,
            policy.clone(),
            async { notify_receiver.await.unwrap() },
            logger,
        )
        .await
        .context("error while waiting for the server to terminate")
    });

    // Wait for the termination signal.
    let done = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::signal::SIGINT, Arc::clone(&done))
        .context("could not register signal handler")?;

    // The server is started in its own thread, so just block the current thread until a signal
    // arrives. This is needed for getting the correct status code when running with `xtask`.
    while !done.load(Ordering::Relaxed) {
        // There are few synchronization mechanisms that are allowed to be used in a signal
        // handler context, so use a primitive sleep loop to watch for the termination
        // notification (rather than something more accurate like `std::sync::Condvar`).
        // See e.g.: http://man7.org/linux/man-pages/man7/signal-safety.7.html
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    notify_sender
        .send(())
        .expect("Couldn't send completion signal.");

    server_handle
        .await
        .context("error while waiting for the server to terminate")?
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub enum Data {
    /// Download data file via HTTP GET.
    /// Supported URL schemes: `http`, `https`.
    Url(String),
    /// Read data file from the local file system.
    /// File path is relative to the current `$PWD` (*not* relative to the config file).
    File(String),
}

/// Configuration to load the LookupData.
#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct LoadLookupDataConfig {
    /// URL of a file containing key / value entries in protobuf binary format for lookup.
    ///
    /// If empty or not provided, no data is available for lookup.
    #[serde(default)]
    lookup_data: Option<Data>,
    /// How often to refresh the lookup data.
    ///
    /// If empty or not provided, data is only loaded once at startup.
    #[serde(default, with = "humantime_serde")]
    lookup_data_download_period: Option<Duration>,
    /// Whether to use the GCP metadata service to obtain an authentication token for downloading
    /// the lookup data.
    #[serde(default = "LookupDataAuth::default")]
    lookup_data_auth: LookupDataAuth,
}

/// Creates LookupDataManager and sets up LookupDataRefresher.
pub async fn load_lookup_data(
    config: LoadLookupDataConfig,
    logger: Logger,
) -> anyhow::Result<Arc<LookupDataManager<Logger>>> {
    // Allow lookup data to be loaded by an untrusted launcher.
    let lookup_data_source = match &config.lookup_data {
        Some(lookup_data) => match &lookup_data {
            Data::Url(url_string) => {
                let url = url::Url::parse(url_string).context("Couldn't parse lookup data URL")?;
                match url.scheme() {
                    "http" | "https" => Some(LookupDataSource::Http {
                        url: url_string.clone(),
                        auth: config.lookup_data_auth,
                    }),
                    scheme => anyhow::bail!(
                        "Unknown URL scheme in lookup data: expected 'http' or 'https', found {}",
                        scheme
                    ),
                }
            }
            Data::File(path) => Some(LookupDataSource::File(path.clone().into())),
        },
        None => None,
    };
    let lookup_data_manager = Arc::new(LookupDataManager::new_empty(logger.clone()));
    if lookup_data_source.is_some() {
        let lookup_data_refresher = LookupDataRefresher::new(
            lookup_data_source,
            lookup_data_manager.clone(),
            logger.clone(),
        );
        // First load the lookup data upfront in a blocking fashion.
        // TODO(#1930): Retry the initial lookup a few times if it fails.
        lookup_data_refresher
            .refresh()
            .await
            .context("Couldn't perform initial load of lookup data")?;
        if let Some(lookup_data_download_period) = config.lookup_data_download_period {
            // Create background task to periodically refresh the lookup data.
            tokio::spawn(async move {
                background_refresh_lookup_data(
                    &lookup_data_refresher,
                    lookup_data_download_period,
                    &logger,
                )
                .await
            });
        };
    }
    Ok(lookup_data_manager)
}

pub async fn create_base_extension_factories(
    load_lookup_data_config: LoadLookupDataConfig,
    logger: Logger,
) -> anyhow::Result<Vec<Box<dyn ExtensionFactory<Logger>>>> {
    let mut extensions = Vec::new();

    // For Base we add the Logging extension factory
    let workload_logging_factory =
        WorkloadLoggingFactory::new_boxed_extension_factory(logger.clone())?;
    extensions.push(workload_logging_factory);

    // For Base we add the Lookup extension factory
    let lookup_data_manager = load_lookup_data(load_lookup_data_config, logger.clone()).await?;
    let lookup_factory = LookupFactory::new_boxed_extension_factory(lookup_data_manager)?;
    extensions.push(lookup_factory);

    Ok(extensions)
}
