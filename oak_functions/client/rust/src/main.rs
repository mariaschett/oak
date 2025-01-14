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

//! Sends a gRPC request to an Oak Functions application and checks that the response has the
//! correct format.

use anyhow::Context;
use clap::Parser;
use oak_functions_abi::Request;
use oak_functions_client::Client;
use regex::Regex;

const TWO_MIB: usize = (2 * 1024) ^ 2;
const LARGE_MESSAGE: [u8; TWO_MIB] = [0; TWO_MIB];

#[derive(Parser, Clone)]
#[clap(about = "Oak Functions Client")]
pub struct Opt {
    #[clap(
        long,
        help = "URI of the Oak Functions application to connect to",
        default_value = "http://localhost:8080"
    )]
    uri: String,

    #[clap(
        long,
        help = "request payload",
        required_unless_present = "test-large-message"
    )]
    request: Option<String>,

    /// Optional, only for testing.
    #[clap(long, help = "expected response body, for testing")]
    expected_response_pattern: Option<String>,

    /// Number of times the request should be sent, and the expected response validated.
    #[clap(long, requires_all = &["request", "expected-response-pattern"])]
    iterations: Option<usize>,

    /// Test sending a large message
    #[clap(long, conflicts_with_all = &["request", "expected-response-pattern", "iterations"])]
    test_large_message: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let opt = Opt::parse();

    let mut client = Client::new(&opt.uri)
        .await
        .context("Could not create Oak Functions client")?;

    if opt.test_large_message {
        // The client should be a able to send a large message without
        // crashing or hanging.
        let response = client
            .invoke(Request {
                body: LARGE_MESSAGE.to_vec(),
            })
            .await
            .context("Error invoking Oak Functions instance");
        assert!(response.is_ok());
        return Ok(());
    }

    let iterations = opt.iterations.unwrap_or(1);

    let request = opt.request.unwrap();

    println!(
        "req: {:?}",
        Request {
            body: request.as_bytes().to_vec(),
        }
    );

    for _ in 0..iterations {
        let response = client
            .invoke(Request {
                body: request.as_bytes().to_vec(),
            })
            .await
            .context("Could not invoke Oak Functions")?;

        println!("Response: {:?}", response);
        let response_body = std::str::from_utf8(response.body().unwrap()).unwrap();
        println!("Response: {:?}", response_body);
        if let Some(ref expected) = opt.expected_response_pattern {
            let re = Regex::new(expected).unwrap();
            assert!(re.is_match(response_body));
        }
    }

    Ok(())
}
