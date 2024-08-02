use anyhow::Context;
use chrono::{DateTime, Utc};
use concordium_rust_sdk::{
    base::hashes::{IncorrectLength, TransactionHash},
    id::types::AccountAddress,
    types::{hashes::BlockHash, AbsoluteBlockHeight},
};
use deadpool_postgres::{GenericClient, Object};
use hex_serde;
use serde::Serialize;
use sha2::Digest;
use std::string::FromUtf8Error;
use thiserror::Error;
use tokio_postgres::{types::ToSql, NoTls};

/// Current version of the verification logic used for the ZK proof.
/// Update this version if you want to introduce a new ZK proof-verification logic.
const CURRENT_ZK_PROOF_VERIFICATION_VERSION: u16 = 1;
/// Current version of the verification logic used for the twitter post link.
/// Update this version if you want to introduce a new twitter post link verification logic.
const CURRENT_TWITTER_POST_LINK_VERIFICATION_VERSION: u16 = 1;
/// All versions that should be considered valid for the ZK proof verification.
const VALID_ZK_PROOF_VERIFICATION_VERSIONS: [u16; 1] = [1];
/// All versions that should be considered valid for the twiter post link verification.
const VALID_TWITTER_POST_LINK_VERIFICATION_VERSIONS: [u16; 1] = [1];

/// TODO: check version during the get endpoints returns
/// TODO: add logic when accounts are too old they can not claim anymore.
/// TODO: add check that you can only add twitter link/zk proof if account is in database.

#[derive(Debug, Error)]
pub enum ConversionError {
    #[error("Incorrect length")]
    IncorrectLength(#[from] IncorrectLength),
    #[error("UTF-8 conversion error: {0}")]
    FromUtf8Error(#[from] FromUtf8Error),
}

/// Represents possible errors returned from [`Database`] or [`DatabasePool`]
/// functions
#[derive(Error, Debug)]
pub enum DatabaseError {
    /// An error happened while interacting with the postgres DB.
    #[error("{0}")]
    Postgres(#[from] tokio_postgres::Error),
    /// Failed to perform conversion from DB representation of type.
    #[error("Failed to convert type `{0}`: {1}")]
    TypeConversion(String, #[source] ConversionError),
    /// Failed to configure database
    #[error("Could not configure database: {0}")]
    Configuration(#[from] anyhow::Error),
}

/// Alias for returning results with [`DatabaseError`]s as the `Err` variant.
type DatabaseResult<T> = Result<T, DatabaseError>;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Sha256(#[serde(with = "hex_serde")] [u8; 32]);

impl TryFrom<&[u8]> for Sha256 {
    type Error = IncorrectLength;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        if value.len() == 32 {
            let mut array = [0u8; 32];
            array.copy_from_slice(value);
            Ok(Sha256(array))
        } else {
            Err(IncorrectLength)
        }
    }
}

/// The database configuration stored in the database.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct StoredAccountData {
    /// The row in the database.
    pub id: u64,
    /// The timestamp of the block the event was included in.
    pub block_time: DateTime<Utc>,
    /// The transaction hash that the event was recorded in.
    pub transaction_hash: TransactionHash,
    /// A boolean specifying if the account has already claimed its rewards (got a reward payout).
    /// Every account can only claim rewards once.
    pub claimed: bool,
    /// A boolean specifying if this account address has submitted all tasks
    /// and the regulatory conditions have been proven via a ZK proof.
    /// A manual check of the completed tasks is required now before releasing the reward.
    pub pending_approval: bool,
    /// A link to a twitter post submitted by the above account address (task 1).
    pub twitter_post_link: Option<String>,
    /// A boolean specifying if the text content of the twitter post link is eligible for the reward.
    /// The content of the text was verified by this backend before this flag is set (or will be verified manually).
    pub twitter_post_link_valid: Option<bool>,
    /// A version that specifies the setting of the twitter post link verification. This enables us
    /// to update the twitter post link verification logic in the future and invalidate older versions.
    pub twitter_post_link_verification_version: Option<u64>,
    /// A hash of the concatenated revealed `national_id_number` and `nationality` to prevent
    /// claiming with different accounts for the same identity.
    pub uniqueness_hash: Option<Sha256>,
    /// A boolean specifying if the identity associated with the account is eligible for the reward (task 2).
    /// An associated ZK proof was verified by this backend before this flag is set.
    pub zk_proof_valid: Option<bool>,
    /// A version that specifies the setting of the ZK proof during the verification. This enables us
    /// to update the ZK proof-verification logic in the future and invalidate older proofs.
    pub zk_proof_verification_version: Option<u64>,
}

impl TryFrom<tokio_postgres::Row> for StoredAccountData {
    type Error = DatabaseError;

    fn try_from(value: tokio_postgres::Row) -> DatabaseResult<Self> {
        let raw_id: i64 = value.try_get("id")?;
        let raw_twitter_post_link: Option<&[u8]> = value.try_get("twitter_post_link")?;
        let raw_uniqueness_hash: Option<&[u8]> = value.try_get("uniqueness_hash")?;
        let raw_zk_proof_verification_version: Option<i64> =
            value.try_get("zk_proof_verification_version")?;
        let raw_twitter_post_link_verification_version: Option<i64> =
            value.try_get("twitter_post_link_verification_version")?;
        let raw_transaction_hash: &[u8] = value.try_get("transaction_hash")?;

        let events = Self {
            id: raw_id as u64,
            block_time: value.try_get("block_time")?,
            claimed: value.try_get("claimed")?,
            pending_approval: value.try_get("pending_approval")?,
            zk_proof_valid: value.try_get("zk_proof_valid")?,
            zk_proof_verification_version: raw_zk_proof_verification_version.map(|i| i as u64),
            uniqueness_hash: raw_uniqueness_hash
                .map(|hash| {
                    Sha256::try_from(hash).map_err(|e| {
                        DatabaseError::TypeConversion(
                            "uniqueness_hash".to_string(),
                            ConversionError::IncorrectLength(e),
                        )
                    })
                })
                .transpose()?,
            twitter_post_link_valid: value.try_get("twitter_post_link_valid")?,
            twitter_post_link_verification_version: raw_twitter_post_link_verification_version
                .map(|i| i as u64),
            transaction_hash: raw_transaction_hash.try_into().map_err(|e| {
                DatabaseError::TypeConversion(
                    "transaction_hash".to_string(),
                    ConversionError::IncorrectLength(e),
                )
            })?,
            twitter_post_link: raw_twitter_post_link.and_then(|link| {
                String::from_utf8(link.to_vec())
                    .map(Some)
                    .map_err(|e| {
                        DatabaseError::TypeConversion(
                            "twitter_post_link".to_string(),
                            ConversionError::FromUtf8Error(e),
                        )
                    })
                    .ok()?
            }),
        };

        Ok(events)
    }
}

/// The database configuration stored in the database.
#[derive(Debug, Serialize)]
pub struct StoredConfiguration {
    /// The genesis block hash of the network monitored.
    pub genesis_block_hash: BlockHash,
    /// The last block height that was processed.
    pub latest_processed_block_height: Option<AbsoluteBlockHeight>,
    /// The start block height that was indexed.
    pub start_block_height: AbsoluteBlockHeight,
}

impl TryFrom<tokio_postgres::Row> for StoredConfiguration {
    type Error = DatabaseError;

    fn try_from(value: tokio_postgres::Row) -> DatabaseResult<Self> {
        let raw_genesis_block_hash: &[u8] = value.try_get("genesis_block_hash")?;
        let raw_latest_processed_block_height: Option<i64> =
            value.try_get("latest_processed_block_height")?;
        let raw_start_block_height: i64 = value.try_get("start_block_height")?;

        let latest_processed_block_height =
            raw_latest_processed_block_height.map(|raw_latest_processed_block_height| {
                AbsoluteBlockHeight::from(raw_latest_processed_block_height as u64)
            });
        let start_block_height = AbsoluteBlockHeight::from(raw_start_block_height as u64);

        let settings = Self {
            latest_processed_block_height,
            genesis_block_hash: raw_genesis_block_hash.try_into().map_err(|e| {
                DatabaseError::TypeConversion(
                    "genesis_block_hash".to_string(),
                    ConversionError::IncorrectLength(e),
                )
            })?,
            start_block_height,
        };
        Ok(settings)
    }
}

/// Database client wrapper
pub struct Database {
    /// The database client
    pub client: Object,
}

impl From<Object> for Database {
    fn from(client: Object) -> Self {
        Self { client }
    }
}

impl AsRef<Object> for Database {
    fn as_ref(&self) -> &Object {
        &self.client
    }
}

impl Database {
    /// Inserts a row in the settings table holding the application
    /// configuration if row does not exist already. The table is constrained to
    /// only hold a single row.
    pub async fn init_settings(
        &self,
        genesis_block_hash: &BlockHash,
        start_block_height: AbsoluteBlockHeight,
    ) -> DatabaseResult<()> {
        let init_settings = self
            .client
            .prepare_cached(
                "INSERT INTO settings (genesis_block_hash, start_block_height) \
                VALUES ($1, $2) ON \
                CONFLICT DO NOTHING",
            )
            .await?;
        let params: [&(dyn ToSql + Sync); 2] = [
            &genesis_block_hash.as_ref(),
            &(start_block_height.height as i64),
        ];
        self.client.execute(&init_settings, &params).await?;
        Ok(())
    }

    // Inserts a row in the settings table holding the application
    // configuration if row does not exist already. The table is constrained to
    // only hold a single row.
    pub async fn set_zk_proof(
        &self,
        national_id: String,
        nationality: String,
        account_address: AccountAddress,
    ) -> DatabaseResult<()> {
        // Create an `uniqueness_hash` to identify the identity associated with the account
        // by hashing the concatenating string of `national_id` and `nationality`.
        // Every identity should only be allowed to receive rewards once
        // (with one of their accounts). The `nationality` is a two-letter country code
        // (ISO 3166-1 alpha-2).
        // Note: Concatenating a fixed-size string (`nationality`) with a non-fixed-size
        // string (`national_id`) is safe. Two non-fixed-size strings would be unsafe.
        // E.g. `format!("{}{}", "AA", "BB")` and `format!("{}{}", "A", "ABB")`
        // would produce the same hash even if the strings are different.
        let concatenated = format!("{}{}", national_id, nationality);
        let mut hasher = sha2::Sha256::new();
        hasher.update(concatenated);
        let uniqueness_hash = hasher.finalize();

        // TODO check hash not used in database so far
        // TODO if `hash` and `twitter` is set, make `pending_approval` to true

        let set_zk_proof = self
            .client
            .prepare_cached(
                "UPDATE accounts \
                SET zk_proof_valid = $1, zk_proof_verification_version = $2, uniqueness_hash = $3 \
                WHERE account_address = $4 ",
            )
            .await?;
        let params: [&(dyn ToSql + Sync); 4] = [
            &true,
            &(CURRENT_ZK_PROOF_VERIFICATION_VERSION as i64),
            &uniqueness_hash.as_slice(),
            &account_address.0.as_ref(),
        ];
        self.client.execute(&set_zk_proof, &params).await?;
        Ok(())
    }

    // Inserts a row in the settings table holding the application
    // configuration if row does not exist already. The table is constrained to
    // only hold a single row.
    pub async fn set_twitter_post_link(
        &self,
        tweet_post_link: String,
        account_address: AccountAddress,
    ) -> DatabaseResult<()> {
        let set_twitter_post_link = self
            .client
            .prepare_cached(
                "UPDATE accounts \
                SET twitter_post_link_valid = $1, twitter_post_link_verification_version = $2, twitter_post_link = $3 \
                WHERE account_address = $4 ",
            )
            .await?;
        let params: [&(dyn ToSql + Sync); 4] = [
            &true,
            &(CURRENT_TWITTER_POST_LINK_VERIFICATION_VERSION as i64),
            &tweet_post_link.as_bytes(),
            &account_address.0.as_ref(),
        ];
        self.client.execute(&set_twitter_post_link, &params).await?;
        Ok(())
    }

    /// Get the settings recorded in the database.
    pub async fn get_settings(&self) -> DatabaseResult<Option<StoredConfiguration>> {
        let get_settings = self
            .client
            .prepare_cached(
                "SELECT genesis_block_hash, start_block_height, latest_processed_block_height \
                FROM settings",
            )
            .await?;

        let opt_row = self.client.query_opt(&get_settings, &[]).await?;

        opt_row.map(StoredConfiguration::try_from).transpose()
    }

    /// Get the settings recorded in the database.
    pub async fn get_account_data(
        &self,
        account_address: AccountAddress,
    ) -> DatabaseResult<Option<StoredAccountData>> {
        let get_account_data = self
            .client
            .prepare_cached(
                "SELECT id, block_time, transaction_hash, claimed, pending_approval, twitter_post_link, twitter_post_link_valid, twitter_post_link_verification_version, zk_proof_valid, zk_proof_verification_version, uniqueness_hash
                FROM accounts
                WHERE account_address = $1",
            ).await?;

        let params: [&(dyn ToSql + Sync); 1] = [&(account_address.0.as_ref())];

        let opt_row = self.client.query_opt(&get_account_data, &params).await?;

        opt_row.map(StoredAccountData::try_from).transpose()
    }

    /// Get the settings recorded in the database.
    pub async fn get_pending_approvals(
        &self,
        limit: u32,
        offset: u32,
    ) -> DatabaseResult<Vec<StoredAccountData>> {
        let get_pending_approvals = self
            .client
            .prepare_cached(
                "SELECT id, block_time, transaction_hash, claimed, pending_approval, twitter_post_link, twitter_post_link_valid, twitter_post_link_verification_version, uniqueness_hash, zk_proof_valid, zk_proof_verification_version \
                FROM accounts \
                WHERE pending_approval = true \
                LIMIT $1 \
                OFFSET $2"
            ).await?;
        let params: [&(dyn ToSql + Sync); 2] = [&(limit as i64), &(offset as i64)];

        let rows = self.client.query(&get_pending_approvals, &params).await?;

        let result: Vec<StoredAccountData> = rows
            .into_iter()
            .map(StoredAccountData::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(result)
    }

    /// Get the settings recorded in the database.
    pub async fn can_claim(&self, account_address: AccountAddress) -> DatabaseResult<bool> {
        let get_account_data = self
            .client
            .prepare_cached(
                "SELECT claimed \
                    FROM accounts \
                    WHERE account_address = $1",
            )
            .await?;

        let params: [&(dyn ToSql + Sync); 1] = [&(account_address.0.as_ref())];

        let opt_row = self.client.query_opt(&get_account_data, &params).await?;

        let claimed: Option<bool> = opt_row.map(|value| value.try_get("claimed")).transpose()?;

        Ok(claimed.unwrap_or(false))
    }
}

/// Representation of a database pool
#[derive(Debug, Clone)]
pub struct DatabasePool {
    /// The inner pool value.
    pool: deadpool_postgres::Pool,
}

impl DatabasePool {
    /// Create a new [`DatabasePool`] from [`tokio_postgres::Config`] of size
    /// `pool_size`. If `try_create_tables` is true, database tables are
    /// created using `../resources/schema.sql`.
    pub async fn create(
        db_config: tokio_postgres::Config,
        pool_size: usize,
        try_create_tables: bool,
    ) -> DatabaseResult<Self> {
        let manager_config = deadpool_postgres::ManagerConfig {
            recycling_method: deadpool_postgres::RecyclingMethod::Verified,
        };

        let manager = deadpool_postgres::Manager::from_config(db_config, NoTls, manager_config);
        let pool = deadpool_postgres::Pool::builder(manager)
            .create_timeout(Some(std::time::Duration::from_secs(5)))
            .recycle_timeout(Some(std::time::Duration::from_secs(5)))
            .wait_timeout(Some(std::time::Duration::from_secs(5)))
            .max_size(pool_size)
            .runtime(deadpool_postgres::Runtime::Tokio1)
            .build()
            .context("Failed to build database pool")?;

        if try_create_tables {
            let client = pool
                .get()
                .await
                .context("Could not get database connection from pool")?;
            client
                .batch_execute(include_str!("../resources/schema.sql"))
                .await
                .context("Failed to execute create statements")?;
        }
        Ok(Self { pool })
    }

    /// Get a [`Database`] connection from the pool.
    pub async fn get(&self) -> DatabaseResult<Database> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get connection from pool")?;
        Ok(client.into())
    }
}
