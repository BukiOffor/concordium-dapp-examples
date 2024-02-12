//! A tool for indexing event data from the track and trace contract into a
//! postgres database. The database is configured with the tables from the file
//! `../rescourcs/schema.sql`. The events `ItemStatusChangedEvent` and
//! `ItemCreatedEvent` are indexed in their respective tables. A third table
//! `settings` exists to store global configurations and checkpoints.
//!
//! Each event can be uniquely identified by the triple (`block_height`,
//! `transaction_hash`, and `event_inex`) and will be uniquely inserted into the
//! table. Meaning even after restarting the indexer, an event will only be
//! inserted into the database if it does not exist in the database yet.
//! Whenever an event is inserted into the database, the checkpoint in the
//! `settings` table is updated to reflect the latest block height, the latest
//! transaction hash, and the latest event index processed by the indexer.
//!
//! The indexer has some retry logic to re-connect to the database in case
//! connection is lost.
use anyhow::Context;
use clap::Parser;
use concordium_rust_sdk::{
    indexer::{self},
    types::{hashes::TransactionHash, AbsoluteBlockHeight, ContractAddress},
    v2 as sdk,
};
use contract::{ItemCreatedEvent, ItemStatusChangedEvent};
use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use track_and_trace as contract;
mod db;
use crate::db::*;

/// Command line configuration of the application.
#[derive(Debug, clap::Parser)]
#[command(author, version, about)]
struct Args {
    #[arg(
        long = "node",
        short = 'n',
        help = "The node endpoint.",
        default_value = "https://grpc.testnet.concordium.com:20000",
        global = true,
        env = "CCD_INDEXER_NODE"
    )]
    node_endpoint:    concordium_rust_sdk::v2::Endpoint,
    #[arg(
        long = "start",
        short = 's',
        help = "The start block height when the track and trace contract was initialized.",
        env = "CCD_INDEXER_START"
    )]
    start:            AbsoluteBlockHeight,
    #[arg(
        long = "contract",
        short = 'c',
        help = "The track and trace contract address.",
        env = "CCD_INDEXER_CONTRACT"
    )]
    contract_address: ContractAddress,
    /// Database connection string.
    #[arg(
        long = "db-connection",
        default_value = "host=localhost dbname=indexer user=postgres password=password port=5432",
        help = "A connection string detailing the connection to the database used by the \
                application.",
        env = "CCD_INDEXER_DB_CONNECTION"
    )]
    db_connection:    tokio_postgres::config::Config,
    /// Maximum log level
    #[clap(
        long = "log-level",
        default_value = "info",
        help = "The maximum log level. Possible values are: `trace`, `debug`, `info`, `warn`, and \
                `error`.",
        env = "CCD_INDEXER_LOG_LEVEL"
    )]
    log_level:        tracing_subscriber::filter::LevelFilter,
}

/// Construct a future for shutdown signals (for unix: SIGINT and SIGTERM) (for
/// windows: ctrl c and ctrl break). The signal handler is set when the future
/// is polled and until then the default signal handler.
async fn set_shutdown(flag: Arc<AtomicBool>) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix as unix_signal;
        let mut terminate_stream = unix_signal::signal(unix_signal::SignalKind::terminate())?;
        let mut interrupt_stream = unix_signal::signal(unix_signal::SignalKind::interrupt())?;
        let terminate = Box::pin(terminate_stream.recv());
        let interrupt = Box::pin(interrupt_stream.recv());
        futures::future::select(terminate, interrupt).await;
        flag.store(true, Ordering::Release);
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows as windows_signal;
        let mut ctrl_break_stream = windows_signal::ctrl_break()?;
        let mut ctrl_c_stream = windows_signal::ctrl_c()?;
        let ctrl_break = Box::pin(ctrl_break_stream.recv());
        let ctrl_c = Box::pin(ctrl_c_stream.recv());
        futures::future::select(ctrl_break, ctrl_c).await;
        flag.store(true, Ordering::Release);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let app: Args = Args::parse();

    // Tracing configuration.
    {
        use tracing_subscriber::prelude::*;
        let log_filter = tracing_subscriber::filter::Targets::new()
            .with_target(module_path!(), app.log_level)
            .with_target("indexer", app.log_level)
            .with_target("ccd_indexer", app.log_level);

        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer())
            .with(log_filter)
            .init();
    }

    // Set up endpoint to the node.
    let endpoint = if app
        .node_endpoint
        .uri()
        .scheme()
        .map_or(false, |x| x == &sdk::Scheme::HTTPS)
    {
        app.node_endpoint
            .tls_config(tonic::transport::channel::ClientTlsConfig::new())
            .context("Unable to construct TLS configuration for the Concordium API.")?
    } else {
        app.node_endpoint
    }
    .connect_timeout(std::time::Duration::from_secs(5))
    .timeout(std::time::Duration::from_secs(10));

    // Establish connection to the postgres database.
    let db_pool = DatabasePool::create(app.db_connection.clone(), 2, true)
        .await
        .context("Could not create database pool")?;
    let db = db_pool
        .get()
        .await
        .context("Could not get database connection from pool")?;
    db.init_settings(&app.contract_address)
        .await
        .context("Could not init settings for database")?;
    let settings = db
        .get_settings()
        .await
        .context("Could not get settings from database")?;

    anyhow::ensure!(
        settings.contract_address == app.contract_address,
        "Contract address does not match the contract address found in the database"
    );

    tracing::info!("Settings: {:?}", settings);

    handle_indexing(db, db_pool, endpoint, app.start, app.contract_address).await
}

/// Handle indexing events.
async fn handle_indexing(
    mut db: Database,
    db_pool: DatabasePool,
    endpoint: sdk::Endpoint,
    start: AbsoluteBlockHeight,
    contract_address: ContractAddress,
) -> anyhow::Result<()> {
    // Establish connection to the node.

    // Database process runs until the stop flag is triggered.
    let stop_flag = Arc::new(AtomicBool::new(false));
    let shutdown_handle = tokio::spawn(set_shutdown(stop_flag.clone()));

    tracing::info!("Indexing from block height {}.", start);

    let contract_set = BTreeSet::from([contract_address]);

    // Start indexer.
    let traverse_config = indexer::TraverseConfig::new_single(endpoint, start);
    let (sender, mut receiver) = tokio::sync::mpsc::channel(20);
    let indexer_handle = tokio::spawn(traverse_config.traverse(
        indexer::AffectedContractIndexer {
            addresses: contract_set,
            all:       true,
        },
        sender,
    ));

    // The indexer starts processing historical events and then listens for new
    // events that are coming in as the blockchain progresses.
    while let Some((block, contract_update_infos)) = receiver.recv().await {
        // Stop indexer when triggered.
        if stop_flag.load(Ordering::Acquire) {
            break;
        }

        for tx in contract_update_infos {
            for (contract_invoked, _entry_point_name, events) in tx.0.execution_tree.events() {
                anyhow::ensure!(
                    contract_invoked == contract_address,
                    "The event picked up by the indexer should be from contract `{}` but \
                     following contract address was found while indexing `{}`.",
                    contract_address,
                    contract_invoked
                );

                for (event_index, event) in events.iter().enumerate() {
                    let parsed_event: contract::Event = event.parse()?;

                    if let contract::Event::ItemStatusChanged(item_status_change_event) =
                        parsed_event
                    {
                        // In case of DB errors, we will reconnect and retry to insert the event
                        // into the database.
                        let mut retry = true;
                        // How many successive insertion errors were encountered.
                        // This is used to slow down attempts to not spam the database.
                        let mut reconnecting_db_errors_count = 0;

                        while retry {
                            retry = match db_insert_item_status_changed_event(
                                &mut db,
                                block.block_height,
                                tx.0.transaction_hash,
                                event_index,
                                item_status_change_event.clone(),
                            )
                            .await
                            {
                                Ok(time) => {
                                    reconnecting_db_errors_count = 0;
                                    tracing::info!(
                                        "Processed `item_status_change_event` at event index {} \
                                         in transaction {} in block {}. Database transaction took \
                                         {}ms.",
                                        event_index,
                                        tx.0.transaction_hash,
                                        block.block_height,
                                        time.num_milliseconds()
                                    );
                                    false
                                }
                                Err(e) => {
                                    reconnecting_db_errors_count += 1;
                                    // wait for 500 * 2^(min(successive_errors - 1, 7))
                                    // seconds before attempting.
                                    // The reason for the min is that we bound the
                                    // time between reconnects.
                                    let delay = std::time::Duration::from_millis(
                                        500 * (1 << std::cmp::min(reconnecting_db_errors_count, 8)),
                                    );
                                    tracing::warn!(
                                        "Database connection lost due to {:#}. Will
                            attempt to reconnect in {}ms. Reconnecting database error count: {}",
                                        e,
                                        delay.as_millis(),
                                        reconnecting_db_errors_count
                                    );
                                    tokio::time::sleep(delay).await;

                                    // Get new db connection from the pool
                                    db = match db_pool.get().await.context(
                                        "Failed to get new database
                            connection from pool",
                                    ) {
                                        Ok(db) => db,
                                        Err(e) => {
                                            receiver.close();
                                            return Err(e);
                                        }
                                    };
                                    true
                                }
                            };
                        }
                    } else if let contract::Event::ItemCreated(item_created_event) = parsed_event {
                        // In case of DB errors, we will reconnect and retry to insert the event
                        // into the database.
                        let mut retry = true;
                        // How many successive insertion errors were encountered.
                        // This is used to slow down attempts to not spam the database.
                        let mut reconnecting_db_errors_count = 0;

                        while retry {
                            retry = match db_insert_created_event(
                                &mut db,
                                block.block_height,
                                tx.0.transaction_hash,
                                event_index,
                                item_created_event.clone(),
                            )
                            .await
                            {
                                Ok(time) => {
                                    reconnecting_db_errors_count = 0;
                                    tracing::info!(
                                        "Processed `item_created_event` at event index {} in \
                                         transaction {} in block {}. Database transaction took \
                                         {}ms.",
                                        event_index,
                                        tx.0.transaction_hash,
                                        block.block_height,
                                        time.num_milliseconds()
                                    );
                                    false
                                }
                                Err(e) => {
                                    reconnecting_db_errors_count += 1;
                                    // wait for 500 * 2^(min(successive_errors - 1, 7))
                                    // seconds before attempting.
                                    // The reason for the min is that we bound the
                                    // time between reconnects.
                                    let delay = std::time::Duration::from_millis(
                                        500 * (1 << std::cmp::min(reconnecting_db_errors_count, 8)),
                                    );
                                    tracing::warn!(
                                        "Database connection lost due to {:#}. Will
                            attempt to reconnect in {}ms. Reconnecting database error count: {}",
                                        e,
                                        delay.as_millis(),
                                        reconnecting_db_errors_count
                                    );
                                    tokio::time::sleep(delay).await;

                                    // Get new db connection from the pool
                                    db = match db_pool.get().await.context(
                                        "Failed to get new database
                            connection from pool",
                                    ) {
                                        Ok(db) => db,
                                        Err(e) => {
                                            receiver.close();
                                            return Err(e);
                                        }
                                    };
                                    true
                                }
                            };
                        }
                    }
                }
            }
        }
    }

    indexer_handle.abort();
    shutdown_handle.abort();

    Ok(())
}

/// Inserts data related to the `ItemCreatedEvent` into the database.
/// Everything is commited as a single transactions allowing
/// for easy restoration from the last recorded checkpoint into
/// the database. Returns the duration it took to process the transaction.
#[tracing::instrument(skip(db))]
async fn db_insert_created_event<'a>(
    db: &mut Database,
    block_height: AbsoluteBlockHeight,
    tx_hash: TransactionHash,
    event_index: usize,
    event: ItemCreatedEvent,
) -> anyhow::Result<chrono::Duration> {
    let start = chrono::Utc::now();
    let transaction = db
        .client
        .transaction()
        .await
        .context("Failed to build DB transaction")?;

    let transaction = Transaction::from(transaction);

    transaction
        .set_latest_checkpoint(block_height, tx_hash, event_index)
        .await?;

    transaction
        .insert_item_created_event(block_height, tx_hash, event_index, event)
        .await?;

    let now = tokio::time::Instant::now();
    transaction
        .inner
        .commit()
        .await
        .context("Failed to commit DB transaction.")?;

    tracing::debug!("Commit completed in {}ms.", now.elapsed().as_millis());

    let end = chrono::Utc::now().signed_duration_since(start);
    Ok(end)
}

/// Inserts data related to the `ItemStatusChangedEvent` into the database.
/// Everything is commited as a single transactions allowing
/// for easy restoration from the last checkpoint inserted into
/// the database. Returns the duration it took to process the transaction.
#[tracing::instrument(skip(db))]
async fn db_insert_item_status_changed_event<'a>(
    db: &mut Database,
    block_height: AbsoluteBlockHeight,
    tx_hash: TransactionHash,
    event_index: usize,
    event: ItemStatusChangedEvent,
) -> anyhow::Result<chrono::Duration> {
    let start = chrono::Utc::now();
    let transaction = db
        .client
        .transaction()
        .await
        .context("Failed to build DB transaction")?;

    let transaction = Transaction::from(transaction);

    transaction
        .set_latest_checkpoint(block_height, tx_hash, event_index)
        .await?;

    transaction
        .insert_item_status_changed_event(block_height, tx_hash, event_index, event)
        .await?;

    let now = tokio::time::Instant::now();
    transaction
        .inner
        .commit()
        .await
        .context("Failed to commit DB transaction.")?;

    tracing::debug!("Commit completed in {}ms.", now.elapsed().as_millis());

    let end = chrono::Utc::now().signed_duration_since(start);
    Ok(end)
}
