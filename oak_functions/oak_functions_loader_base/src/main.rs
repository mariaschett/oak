//
// Copyright 2022 The Project Oak Authors
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

//! The "base" Oak Functions runtime binary, which guarantees that user data stays private.

use anyhow::Context;
use clap::Parser;
use log::Level;
use oak_functions_loader::{logger::Logger, server::Policy, LoadLookupDataConfig, Opt};
use oak_logger::OakLogger;
use serde_derive::Deserialize;
use std::fs;

/// Runtime Configuration of the Oak Functions Runtime for a Base Oak Functions Runtime with no
/// experimental features.
///
/// This struct serves as a schema for a static TOML config file provided by
/// the team using the Oak Functions Runtime for their business logic. In deployment, this
/// config is bundled with the Oak Functions Runtime binary. The config is
/// version controlled and testing requires no change. The values in the config serve
/// as a type safe version of regular command line flags and cannot contain $ENVIRONMENT
/// variables.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Configuration to load the LookupData.
    #[serde(default)]
    load_lookup_data: LoadLookupDataConfig,
    /// Security policy guaranteed by the server.
    policy: Option<Policy>,
    /// Path to a Wasm module to be loaded and executed per invocation. The Wasm module must export
    /// a function named `main` and `alloc`.
    wasm_path: String,
}

pub fn main() -> anyhow::Result<()> {
    let opt = Opt::parse();
    let config_file_bytes = fs::read(&opt.config_path)
        .with_context(|| format!("Couldn't read config file {}", &opt.config_path))?;
    let config: Config =
        toml::from_slice(&config_file_bytes).context("Couldn't parse config file")?;
    // TODO(#1971): Make maximum log level configurable.
    let logger = Logger::default();
    logger.log_public(Level::Info, &format!("parsed config file:\n{:#?}", config));

    let extension_factories = vec![];

    oak_functions_loader::lib_main(
        logger,
        config.load_lookup_data,
        config.policy,
        config.wasm_path,
        opt.http_listen_port,
        extension_factories,
    )
}
