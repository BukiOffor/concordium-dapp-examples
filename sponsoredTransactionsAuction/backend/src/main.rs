mod handlers;
mod types;
use crate::{handlers::*, types::*};
use anyhow::Context;
use clap::Parser;
use concordium_rust_sdk::{
    common::{self as crypto_common},
    types::WalletAccount,
};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;
use tonic::transport::ClientTlsConfig;
use warp::{http, Filter};

/// Structure used to receive the correct command line arguments.
#[derive(clap::Parser, Debug)]
#[clap(arg_required_else_help(true))]
#[clap(version, author)]
struct IdVerifierConfig {
    #[clap(
        long = "node",
        default_value = "http://localhost:20000",
        env = "NODE",
        help = "GRPC V2 interface of the node."
    )]
    endpoint: concordium_rust_sdk::v2::Endpoint,
    #[clap(
        long = "port",
        default_value = "8100",
        env = "PORT",
        help = "Port on which the server will listen on."
    )]
    port: u16,
    #[clap(
        long = "cis2-token-smart-contract-index",
        default_value = "7370",
        env = "CIS2_TOKEN_CONTRACT_INDEX",
        help = "The cis2 token smart contract index which the sponsored transaction is submitted \
                to."
    )]
    cis2_token_smart_contract_index: u64,
    #[clap(
        long = "auction-smart-contract-index",
        default_value = "7399",
        env = "AUCTION_CONTRACT_INDEX",
        help = "The auction smart contract index which the sponsored transaction is submitted to."
    )]
    auction_smart_contract_index: u64,
    #[structopt(
        long = "log-level",
        default_value = "debug",
        env = "LOG_LEVEL",
        help = "Maximum log level."
    )]
    log_level: log::LevelFilter,
    #[structopt(
        long = "public-folder",
        default_value = "public",
        env = "PUBLIC_FOLDER",
        help = "location of the folder to serve"
    )]
    public_folder: String,
    #[structopt(
        long = "account-key-file",
        env = "ACCOUNT_KEY_FILE",
        help = "Path to the account key file."
    )]
    keys_path: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let app = IdVerifierConfig::parse();
    let mut log_builder = env_logger::Builder::new();
    // only log the current module (main).
    log_builder.filter_level(app.log_level); // filter filter_module(module_path!(), app.log_level);
    log_builder.init();

    let endpoint = if app
        .endpoint
        .uri()
        .scheme()
        .map_or(false, |x| x == &http::uri::Scheme::HTTPS)
    {
        app.endpoint.tls_config(ClientTlsConfig::new())?
    } else {
        app.endpoint
    };

    let mut client_transfer = concordium_rust_sdk::v2::Client::new(endpoint).await?;

    let cors = warp::cors()
        .allow_any_origin()
        .allow_header("Content-Type")
        .allow_methods(vec!["POST", "GET"]);

    // load account keys and sender address from a file
    let keys: WalletAccount = serde_json::from_str(
        &std::fs::read_to_string(app.keys_path).context("Could not read the keys file.")?,
    )
    .context("Could not parse the keys file.")?;

    let key_transfer = Arc::new(keys);

    log::debug!("Acquire nonce of wallet account.");

    let nonce_response = client_transfer
        .get_next_account_sequence_number(&key_transfer.address)
        .await
        .map_err(|e| {
            log::warn!("NonceQueryError {:#?}.", e);
            LogError::NonceQueryError
        })?;

    let state_transfer = Server {
        nonce:       Arc::new(Mutex::new(nonce_response.nonce)),
        rate_limits: Arc::new(Mutex::new(HashMap::new())),
    };

    // 1. Provide submit update operator
    let provide_submit_bid = warp::post()
        .and(warp::filters::body::content_length_limit(50 * 1024))
        .and(warp::path!("api" / "bid"))
        .and(warp::body::json())
        .and_then(move |request: BidParams| {
            log::debug!("Process bid transaction.");

            handle_signature_bid(
                client_transfer.clone(),
                key_transfer.clone(),
                request,
                app.cis2_token_smart_contract_index,
                app.auction_smart_contract_index,
                state_transfer.clone(),
            )
        });

    let serve_public_files = warp::get().and(warp::fs::dir(app.public_folder));

    let server = provide_submit_bid
        .or(serve_public_files)
        .recover(handle_rejection)
        .with(cors)
        .with(warp::trace::request());
    warp::serve(server).run(([0, 0, 0, 0], app.port)).await;
    Ok(())
}
