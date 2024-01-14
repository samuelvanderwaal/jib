//! # Jib
//!
//! Jib is a simple Rust library that efficiently packs a vector of Solana instructions into maximum size and account length transactions
//! and then uses the TPU client to send them directly to the current leader, rather than via a RPC node.
//!
//! It still uses a RPC client to determine the current leader, but it can be used with default public nodes as the single operation required
//! does not need a high-throughput private RPC node.
//!
//! ## Example Usage
//!
//! In this example we load a list of mint accounts we want to update the metadata for, and then we create a vector of instructions with the update_metadata_accounts_v2 instruction for each mint.
//! We then pass this vector of instructions to Jib and it will pack them into the most efficient transactions possible and send them to the current leader.
//!
//! ```ignore
//! fn main() -> Result<()> {
//!     // Load our keypair file.
//!     let keypair = solana_sdk::signature::read_keypair_file("keypair.json").unwrap();
//!
//!     // Initialize Jib with our keypair and desired network. Devnet is also the default value: Network::default().
//!     let mut jib = Jib::new(vec![keypair], Network::Devnet)?;
//!
//!     let mut instructions = vec![];
//!
//!     // Load mint addresses from a file.
//!     let addresses: Vec<String> =
//!         serde_json::from_reader(std::fs::File::open("collection_mints.json")?)?;
//!
//!     // Create an instruction for each mint.
//!     for address in addresses {
//!         let metadata = derive_metadata_pda(&Pubkey::from_str(&address).unwrap());
//!
//!         let ix = update_metadata_accounts_v2(
//!             mpl_token_metadata::ID,
//!             metadata,
//!             // Jib takes the payer by value but we can access it via this fn.
//!             jib.payer().pubkey(),
//!             None,
//!             None,
//!             Some(true),
//!             None,
//!         );
//!
//!         instructions.push(ix);
//!     }
//!
//!     // Set the instructions to be executed.
//!     jib.set_instructions(instructions);
//!
//!     // Run it.
//!     let results = jib.hoist()?;
//!
//!     // Do something with the results.
//!     for result in results {
//!         if result.is_success() {
//!             println!("Success: {}", result.signature().unwrap());
//!         } else {
//!             println!("Failure: {}", result.error().unwrap());
//!         }
//!     }
//!
//!     Ok(())
//! }
//! ```

use std::{str::FromStr, sync::Arc, time::Duration};

#[cfg(feature = "tpu")]
use {
    bincode::config,
    indicatif::{MultiProgress, ProgressStyle},
    solana_client::{
        rpc_client::RpcClient as BlockingRpcClient,
        rpc_request::MAX_GET_SIGNATURE_STATUSES_QUERY_ITEMS,
        tpu_client::{TpuClient, TpuClientConfig, TpuSenderError},
    },
    solana_quic_client::{QuicConfig, QuicConnectionManager, QuicPool},
    solana_sdk::{signature::Signature, signers::Signers},
    std::{collections::HashMap, thread::sleep, time::Instant},
};

use futures_util::{lock::Mutex, stream::FuturesOrdered, StreamExt};
use indicatif::ProgressBar;
use ratelimit::Ratelimiter;
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::Message,
    signature::{Keypair, Signature},
    signer::Signer,
    transaction::Transaction,
};
use tracing::debug;

mod error;

use error::JibError;

const MAX_TX_LEN: usize = 1232;

// Send at ~100 TPS
#[cfg(feature = "tpu")]
const SEND_TRANSACTION_INTERVAL: Duration = Duration::from_millis(10);
// Retry batch send after 4 seconds
#[cfg(feature = "tpu")]
const TRANSACTION_RESEND_INTERVAL: Duration = Duration::from_secs(4);

/// The Network enum is used to set the RPC URL to use for finding the current leader.
/// The default value is Devnet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Network {
    #[default]
    Devnet,
    MainnetBeta,
    Testnet,
    Localnet,
}

impl FromStr for Network {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "devnet" => Ok(Network::Devnet),
            "mainnet" => Ok(Network::MainnetBeta),
            "testnet" => Ok(Network::Testnet),
            "localnet" => Ok(Network::Localnet),
            _ => Err(()),
        }
    }
}

impl ToString for Network {
    fn to_string(&self) -> String {
        match self {
            Network::Devnet => "devnet".to_string(),
            Network::MainnetBeta => "mainnet".to_string(),
            Network::Testnet => "testnet".to_string(),
            Network::Localnet => "localnet".to_string(),
        }
    }
}

impl Network {
    pub fn url(&self) -> &'static str {
        match self {
            Network::Devnet => "https://api.devnet.solana.com",
            Network::MainnetBeta => "https://api.mainnet-beta.solana.com",
            Network::Testnet => "https://api.testnet.solana.com",
            Network::Localnet => "http://127.0.0.1:8899",
        }
    }
}

/// The Jib struct is the main entry point for the library.
/// It is used to create a new Jib instance, set the RPC URL, set the instructions, pack the instructions into transactions
/// and finally submit the transactions to the network.
pub struct Jib {
    #[cfg(feature = "tpu")]
    tpu_client: Option<TpuClient<QuicPool, QuicConnectionManager, QuicConfig>>,
    client: Arc<RpcClient>,
    signers: Vec<Keypair>,
    ixes: Vec<Instruction>,
    compute_budget: u32,
    priority_fee: u64,
    batch_size: usize,
    rate_limit: u64,
}

/// A library Result value indicating Success or Failure and containing information about each result type.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JibResult {
    /// The transaction was successful and contains the signature of the transaction.
    Success(String),
    /// The transaction failed and contains the transaction and error message.
    Failure(JibFailedTransaction),
}

impl JibResult {
    /// Returns true if the result is a success.
    pub fn is_success(&self) -> bool {
        match self {
            JibResult::Success(_) => true,
            JibResult::Failure(_) => false,
        }
    }

    /// Returns true if the result is a failure.
    pub fn is_failure(&self) -> bool {
        !self.is_success()
    }

    pub fn get_failure(self) -> Option<JibFailedTransaction> {
        match self {
            JibResult::Success(_) => None,
            JibResult::Failure(f) => Some(f),
        }
    }

    /// Parses the message from a failure or returns None if the result is a success.
    pub fn message(&self) -> Option<Message> {
        match self {
            JibResult::Success(_) => None,
            JibResult::Failure(f) => Some(f.message.clone()),
        }
    }

    /// Parses the error message from a failure or returns None if the result is a success.
    pub fn error(&self) -> Option<String> {
        match self {
            JibResult::Success(_) => None,
            JibResult::Failure(f) => Some(f.error.clone()),
        }
    }

    /// Parses the signature from a success or returns None if the result is a failure.
    pub fn signature(&self) -> Option<String> {
        match self {
            JibResult::Success(s) => Some(s.clone()),
            JibResult::Failure(_) => None,
        }
    }
}

/// A failed transaction with the error message.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JibFailedTransaction {
    pub signature: Signature,
    pub message: Message,
    pub error: String,
}

impl Jib {
    /// Create a new Jib instance. You should pass in all the signers you want to use for the transactions.
    pub fn new(signers: Vec<Keypair>, rpc_url: String) -> Result<Self, JibError> {
        let client = Arc::new(RpcClient::new_with_commitment(
            rpc_url,
            CommitmentConfig::confirmed(),
        ));

        Ok(Self {
            client,
            signers,
            ixes: Vec::new(),
            compute_budget: 200_000,
            priority_fee: 0,
            batch_size: 10,
            rate_limit: 10,
        })
    }

    #[cfg(feature = "tpu")]
    pub fn create_tpu_client(&mut self, url: &str) -> Result<(), JibError> {
        let rpc_client = Arc::new(BlockingRpcClient::new_with_commitment(
            url.to_string(),
            CommitmentConfig::confirmed(),
        ));
        let wss = url.replace("http", "ws");

        let tpu_config = TpuClientConfig { fanout_slots: 1 };
        let tpu_client = TpuClient::new(rpc_client, &wss, tpu_config)
            .map_err(|e| JibError::FailedToCreateTpuClient(e.to_string()))?;

        self.tpu_client = Some(tpu_client);
        Ok(())
    }

    /// Set the RPC URL to use for the transactions. This defaults to the public devnet URL.
    // pub fn set_rpc_url(&mut self, url: &str) {
    //     self.tpu_client = Self::create_tpu_client(url).unwrap();
    // }

    /// Set the instructions to use for the transactions. This should be a vector of instructions that you wish to submit to the network.
    pub fn set_instructions(&mut self, ixes: Vec<Instruction>) {
        self.ixes = ixes;
    }

    /// Set the signers to use for the transactions. This should be a vector of signers that you wish to use to sign the transactions.
    pub fn set_signers(&mut self, signers: Vec<Keypair>) {
        self.signers = signers;
    }

    /// Set the compute budget to use for the transactions. If not set
    /// no compute budget is set which defaults to the system standard of 200,000.
    pub fn set_compute_budget(&mut self, compute_budget: u32) {
        self.compute_budget = compute_budget;
    }

    /// Set the priority fee to use for the transactions. If not set no priority fee
    /// is set. Units are micro-lamports per compute unit.
    pub fn set_priority_fee(&mut self, priority_fee: u64) {
        self.priority_fee = priority_fee;
    }

    /// Set the batch size to use for the transactions. This defaults to 10.
    pub fn set_batch_size(&mut self, batch_size: usize) {
        self.batch_size = batch_size;
    }

    /// Set the rate limit to use for the transactions. This defaults to 10.
    pub fn set_rate_limit(&mut self, rate_limit: u64) {
        self.rate_limit = rate_limit;
    }

    /// Get the first signer that is being used by the Jib instance, the transaction fee payer.
    pub fn payer(&self) -> &Keypair {
        self.signers.first().unwrap()
    }

    /// Pack the instructions into transactions. This will return a vector of transactions that can be submitted to the network.
    pub async fn pack(&mut self) -> Result<Vec<Transaction>, JibError> {
        if self.ixes.is_empty() {
            return Err(JibError::NoInstructions);
        }

        let mut packed_transactions = Vec::new();

        let mut instructions = Vec::new();
        let payer_pubkey = self.signers.first().ok_or(JibError::NoSigners)?.pubkey();

        if self.compute_budget != 200_000 {
            instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(
                self.compute_budget,
            ));
        }
        if self.priority_fee != 0 {
            instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
                self.priority_fee,
            ));
        }

        let mut current_transaction =
            Transaction::new_with_payer(&instructions, Some(&payer_pubkey));
        let signers: Vec<&Keypair> = self.signers.iter().map(|k| k as &Keypair).collect();

        let latest_blockhash = self
            .client
            .get_latest_blockhash()
            .await
            .map_err(|_| JibError::NoRecentBlockhash)?;

        for ix in self.ixes.iter_mut() {
            instructions.push(ix.clone());
            let mut tx = Transaction::new_with_payer(&instructions, Some(&payer_pubkey));
            tx.sign(&signers, latest_blockhash);

            let tx_len = bincode::serialize(&tx).unwrap().len();

            debug!("tx_len: {}", tx_len);

            if tx_len > MAX_TX_LEN || tx.message.account_keys.len() > 64 {
                packed_transactions.push(current_transaction.clone());
                debug!("Packed instructions: {}", instructions.len());

                // Clear instructions except for the last one that pushed the transaction over the size limit.
                // Check for compute budget and priority fees again and add them to the front of the instructions.
                instructions = vec![];
                if self.compute_budget != 200_000 {
                    instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(
                        self.compute_budget,
                    ));
                }
                if self.priority_fee != 0 {
                    instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
                        self.priority_fee,
                    ));
                }
                instructions.push(ix.clone());
            } else {
                current_transaction = tx;
            }
        }
        packed_transactions.push(current_transaction);

        debug!("Packed transactions: {}", packed_transactions.len());

        Ok(packed_transactions)
    }

    /// Used to repack failed transactions into new transactions with a fresh blockhash and signatures.
    /// These transacations can be resubmitted with `hoist_with_transactions`.
    pub async fn repack_failed(
        &mut self,
        failed_transactions: Vec<JibFailedTransaction>,
    ) -> Result<Vec<Transaction>, JibError> {
        let mut packed_transactions = Vec::new();

        let mut tasks = FuturesOrdered::new();
        let retries = Arc::new(Mutex::new(Vec::new()));

        let signers: Vec<&Keypair> = self.signers.iter().map(|k| k as &Keypair).collect();

        let ratelimiter = Ratelimiter::builder(self.rate_limit, Duration::from_secs(1))
            .max_tokens(self.rate_limit)
            .initial_available(self.rate_limit)
            .build()
            .unwrap();

        let pb = ProgressBar::new(failed_transactions.len() as u64);
        pb.set_message("Checking failed transaction statuses...");
        for tx in failed_transactions {
            // First we check the status of all the failed transactions to filter out any that have already been confirmed.
            let pb = pb.clone();
            let client = Arc::clone(&self.client);
            let retries = Arc::clone(&retries);

            // Wait for the ratelimiter to allow the transaction to be sent.
            if let Err(sleep) = ratelimiter.try_wait() {
                tokio::time::sleep(sleep).await;
                continue;
            }

            let task = tokio::spawn(async move {
                let res = client.confirm_transaction(&tx.signature).await;

                // Retry any errors or unconfirmed transactions.
                if res.is_err() || !res.unwrap() {
                    let mut retries = retries.lock().await;
                    retries.push(tx);
                }
            });
            tasks.push_back(task);

            pb.inc(1);
        }
        pb.finish();

        while let Some(result) = tasks.next().await {
            // We just need to wait for all the tasks to complete.
            match result {
                Ok(_) => {}
                Err(e) => {
                    // Log here? Tasks shouldn't panic.
                    return Err(JibError::TransactionError(e.to_string()));
                }
            }
        }

        let retries = retries.lock().await;

        println!("Found {} failed transactions to retry", retries.len());

        // TODO: just repack and send right away so the blockhashes don't expire.
        let pb = ProgressBar::new(retries.len() as u64);
        pb.set_message("Repacking transactions...");
        for tx in retries.iter() {
            let mut tx = Transaction::new_unsigned(tx.message.clone());
            let blockhash = self
                .client
                .get_latest_blockhash()
                .await
                .map_err(|_| JibError::NoRecentBlockhash)?;
            tx.sign(&signers, blockhash);

            packed_transactions.push(tx);
            pb.inc(1);
        }
        pb.finish();

        Ok(packed_transactions)
    }

    /// Pack the instructions and submit them to the network. This will return a vector of results.
    pub async fn hoist(&mut self) -> Result<Vec<JibResult>, JibError> {
        let packed_transactions = self.pack().await?;

        let results = self._hoist(packed_transactions).await?;

        Ok(results)
    }

    pub async fn hoist_with_transactions(
        &mut self,
        packed_transactions: Vec<Transaction>,
    ) -> Result<Vec<JibResult>, JibError> {
        let results = self._hoist(packed_transactions).await?;

        Ok(results)
    }

    async fn _hoist(
        &mut self,
        packed_transactions: Vec<Transaction>,
    ) -> Result<Vec<JibResult>, JibError> {
        let mut tasks = FuturesOrdered::new();

        let ratelimiter = Ratelimiter::builder(self.rate_limit, Duration::from_secs(1))
            .max_tokens(self.rate_limit)
            .initial_available(self.rate_limit)
            .build()
            .unwrap();

        let pb = ProgressBar::new(packed_transactions.len() as u64);
        pb.set_message("Sending transactions...");
        for tx in packed_transactions {
            let pb = pb.clone();
            let client = Arc::clone(&self.client);

            // Wait for the ratelimiter to allow the transaction to be sent.
            if let Err(sleep) = ratelimiter.try_wait() {
                tokio::time::sleep(sleep).await;
                continue;
            }

            let task = tokio::spawn(async move {
                let res = client.send_and_confirm_transaction(&tx.clone()).await;
                pb.inc(1);
                match res {
                    Ok(signature) => JibResult::Success(signature.to_string()),
                    Err(e) => JibResult::Failure(JibFailedTransaction {
                        signature: tx.signatures[0],
                        message: tx.message.clone(),
                        error: e.to_string(),
                    }),
                }
            });
            tasks.push_back(task);
        }
        pb.finish();

        let mut results = Vec::new();

        while let Some(result) = tasks.next().await {
            match result {
                Ok(result) => results.push(result),
                Err(e) => {
                    return Err(JibError::TransactionError(e.to_string()));
                }
            }
        }

        Ok(results)
    }

    /// Pack the instructions into transactions and submit them to the network via the TPU client.
    /// This will display a spinner while the transactions are being submitted.
    #[cfg(feature = "tpu")]
    pub async fn hoist_via_tpu(&mut self) -> Result<Vec<JibResult>, JibError> {
        let packed_transactions = self.pack().await?;

        let results = self.submit_packed_transactions(packed_transactions).await?;

        Ok(results)
    }

    /// Submit pre-packed transactions to the network via the TPU client. This will display a spinner while the transactions are being submitted.
    #[cfg(feature = "tpu")]
    pub async fn submit_packed_transactions(
        &mut self,
        transactions: Vec<Transaction>,
    ) -> Result<Vec<JibResult>, JibError> {
        let signers: Vec<&Keypair> = self.signers.iter().map(|k| k as &Keypair).collect();

        let messages = transactions
            .as_slice()
            .iter()
            .map(|tx| tx.message.clone())
            .collect::<Vec<_>>();

        let mut results = vec![];

        let mpb = MultiProgress::new();
        let pb = mpb.add(ProgressBar::new(messages.len() as u64));
        pb.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.blue} {msg} {wide_bar:.cyan/blue} {pos:>7}/{len:7} {eta_precise}",
                )
                .unwrap(),
        );
        pb.set_message("Sending transactions");

        for chunk in messages.chunks(self.batch_size) {
            let res = self
                .send_and_confirm_messages_with_spinner(chunk, &signers, &mpb)
                .await
                .map_err(|e| JibError::TransactionError(e.to_string()))?;

            results.extend(res);

            pb.inc(chunk.len() as u64);
        }

        Ok(results)
    }

    // Pulled from tpu_client code and modified to return Jib results for better error handling and retrying failed transactions.
    #[cfg(feature = "tpu")]
    async fn send_and_confirm_messages_with_spinner<T: Signers>(
        &self,
        messages: &[Message],
        signers: &T,
        mpb: &MultiProgress,
    ) -> Result<Vec<JibResult>, TpuSenderError> {
        let tpu_client = self.tpu_client.as_ref().ok_or(TpuSenderError::Custom(
            "TPU client not initialized".to_string(),
        ))?;

        let mut expired_blockhash_retries = 5;

        let spinner = mpb.add(ProgressBar::new(42));
        spinner.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} {wide_msg}")
                .unwrap(),
        );
        spinner.enable_steady_tick(Duration::from_millis(100));

        let mut jib_results = Vec::with_capacity(messages.len());

        let mut transactions = messages
            .iter()
            .enumerate()
            .map(|(i, message)| (i, Transaction::new_unsigned(message.clone())))
            .collect::<Vec<_>>();

        let total_transactions = transactions.len();

        let mut transaction_errors = vec![None; transactions.len()];
        let mut confirmed_transactions = 0;
        let mut block_height = self.client.get_block_height().await?;

        while expired_blockhash_retries > 0 {
            let (blockhash, last_valid_block_height) = self
                .client
                .get_latest_blockhash_with_commitment(self.client.commitment())
                .await?;

            let mut pending_transactions: HashMap<Signature, (usize, Transaction)> = HashMap::new();
            for (i, ref mut transaction) in &mut transactions {
                transaction.try_sign(signers, blockhash)?;
                pending_transactions.insert(transaction.signatures[0], (*i, transaction.clone()));
            }

            let mut last_resend = Instant::now() - TRANSACTION_RESEND_INTERVAL;
            while block_height <= last_valid_block_height {
                let num_transactions = pending_transactions.len();

                // Periodically re-send all pending transactions
                if Instant::now().duration_since(last_resend) > TRANSACTION_RESEND_INTERVAL {
                    for (index, (_i, transaction)) in pending_transactions.values().enumerate() {
                        if !tpu_client.send_transaction(transaction) {
                            let _result = self.client.send_transaction(transaction).await.ok();
                        }
                        set_message_for_confirmed_transactions(
                            &spinner,
                            confirmed_transactions,
                            total_transactions,
                            None, //block_height,
                            last_valid_block_height,
                            &format!("Sending {}/{} transactions", index + 1, num_transactions,),
                        );
                        sleep(SEND_TRANSACTION_INTERVAL);
                    }
                    last_resend = Instant::now();
                }

                // Wait for the next block before checking for transaction statuses
                let mut block_height_refreshes = 10;
                set_message_for_confirmed_transactions(
                    &spinner,
                    confirmed_transactions,
                    total_transactions,
                    Some(block_height),
                    last_valid_block_height,
                    &format!("Waiting for next block, {} pending...", num_transactions),
                );
                let mut new_block_height = block_height;
                while block_height == new_block_height && block_height_refreshes > 0 {
                    sleep(Duration::from_millis(500));
                    new_block_height = self.client.get_block_height().await?;
                    block_height_refreshes -= 1;
                }
                block_height = new_block_height;

                // Collect statuses for the transactions, drop those that are confirmed
                let pending_signatures = pending_transactions.keys().cloned().collect::<Vec<_>>();
                for pending_signatures_chunk in
                    pending_signatures.chunks(MAX_GET_SIGNATURE_STATUSES_QUERY_ITEMS)
                {
                    if let Ok(result) = self
                        .client
                        .get_signature_statuses(pending_signatures_chunk)
                        .await
                    {
                        let statuses = result.value;
                        for (signature, status) in
                            pending_signatures_chunk.iter().zip(statuses.into_iter())
                        {
                            if let Some(status) = status {
                                if status.satisfies_commitment(self.client.commitment()) {
                                    if let Some((i, _)) = pending_transactions.remove(signature) {
                                        confirmed_transactions += 1;
                                        if status.err.is_some() {
                                            spinner.println(format!(
                                                "Failed transaction: {:?}",
                                                status
                                            ));
                                            jib_results.push(JibResult::Failure(
                                                JibFailedTransaction {
                                                    message: transactions[i].1.message.clone(),
                                                    error: status.err.clone().unwrap().to_string(),
                                                },
                                            ));
                                        } else {
                                            jib_results
                                                .push(JibResult::Success(signature.to_string()));
                                        }
                                        transaction_errors[i] = status.err;
                                    }
                                }
                            }
                        }
                    }
                    set_message_for_confirmed_transactions(
                        &spinner,
                        confirmed_transactions,
                        total_transactions,
                        Some(block_height),
                        last_valid_block_height,
                        "Checking transaction status...",
                    );
                }

                if pending_transactions.is_empty() {
                    return Ok(jib_results);
                }
            }

            transactions = pending_transactions.into_values().collect();
            spinner.println(format!(
                "Blockhash expired. {} retries remaining",
                expired_blockhash_retries
            ));
            expired_blockhash_retries -= 1;
        }
        Err(TpuSenderError::Custom("Max retries exceeded".into()))
    }
}

/* Pulled from tpu_client to support 'send_and_confirm_messages_with_spinner' function. */
#[cfg(feature = "tpu")]
fn set_message_for_confirmed_transactions(
    progress_bar: &ProgressBar,
    confirmed_transactions: u32,
    total_transactions: usize,
    block_height: Option<u64>,
    last_valid_block_height: u64,
    status: &str,
) {
    progress_bar.set_message(format!(
        "{:>5.1}% | {:<40}{}",
        confirmed_transactions as f64 * 100. / total_transactions as f64,
        status,
        match block_height {
            Some(block_height) => format!(
                " [block height {}; re-sign in {} blocks]",
                block_height,
                last_valid_block_height.saturating_sub(block_height),
            ),
            None => String::new(),
        },
    ));
}
