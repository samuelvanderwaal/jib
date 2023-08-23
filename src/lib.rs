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
//!     jib.hoist()?;
//!
//!     Ok(())
//! }
//! ```

use std::{
    collections::HashMap,
    str::FromStr,
    sync::Arc,
    thread::sleep,
    time::{Duration, Instant},
};

use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use solana_client::{
    nonblocking::tpu_client::TpuSenderError,
    rpc_client::RpcClient,
    rpc_request::MAX_GET_SIGNATURE_STATUSES_QUERY_ITEMS,
    tpu_client::{TpuClient, TpuClientConfig},
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::Instruction,
    message::Message,
    signature::{Keypair, Signature},
    signer::Signer,
    signers::Signers,
    transaction::Transaction,
};
use tracing::debug;

mod error;

use error::JibError;

const MAX_TX_LEN: usize = 1232;

/// Send at ~100 TPS
const SEND_TRANSACTION_INTERVAL: Duration = Duration::from_millis(10);
/// Retry batch send after 4 seconds
const TRANSACTION_RESEND_INTERVAL: Duration = Duration::from_secs(4);

/// The Network enum is used to set the RPC URL to use for the transactions.
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
    tpu_client: TpuClient,
    signers: Vec<Keypair>,
    ixes: Vec<Instruction>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JibResult {
    Success(String),
    Failure(JibFailedTransaction),
}

impl JibResult {
    pub fn is_success(&self) -> bool {
        match self {
            JibResult::Success(_) => true,
            JibResult::Failure(_) => false,
        }
    }

    pub fn is_failure(&self) -> bool {
        !self.is_success()
    }

    pub fn transaction(&self) -> Option<Transaction> {
        match self {
            JibResult::Success(_) => None,
            JibResult::Failure(f) => Some(f.transaction.clone()),
        }
    }

    pub fn error(&self) -> Option<String> {
        match self {
            JibResult::Success(_) => None,
            JibResult::Failure(f) => Some(f.error.clone()),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JibFailedTransaction {
    pub transaction: Transaction,
    pub error: String,
}

impl Jib {
    /// Create a new Jib instance. You should pass in all the signers you want to use for the transactions.
    pub fn new(signers: Vec<Keypair>, network: Network) -> Result<Self, JibError> {
        let tpu_client = Self::create_tpu_client(network.url())?;

        Ok(Self {
            tpu_client,
            signers,
            ixes: Vec::new(),
        })
    }

    fn create_tpu_client(url: &str) -> Result<TpuClient, JibError> {
        let rpc_client = Arc::new(RpcClient::new_with_commitment(
            url.to_string(),
            CommitmentConfig::confirmed(),
        ));
        let wss = url.replace("http", "ws");

        let tpu_config = TpuClientConfig { fanout_slots: 1 };
        let tpu_client = TpuClient::new(rpc_client, &wss, tpu_config)
            .map_err(|e| JibError::FailedToCreateTpuClient(e.to_string()))?;

        Ok(tpu_client)
    }

    /// Set the RPC URL to use for the transactions. This defaults to the public devnet URL.
    pub fn set_rpc_url(&mut self, url: &str) {
        self.tpu_client = Self::create_tpu_client(url).unwrap();
    }

    /// Set the instructions to use for the transactions. This should be a vector of instructions that you wish to submit to the network.
    pub fn set_instructions(&mut self, ixes: Vec<Instruction>) {
        self.ixes = ixes;
    }

    /// Set the signers to use for the transactions. This should be a vector of signers that you wish to use to sign the transactions.
    pub fn set_signers(&mut self, signers: Vec<Keypair>) {
        self.signers = signers;
    }

    /// Get the RPC client that is being used by the Jib instance.
    pub fn rpc_client(&self) -> &RpcClient {
        self.tpu_client.rpc_client()
    }

    /// Get the first signer that is being used by the Jib instance, the transaction fee payer.
    pub fn payer(&self) -> &Keypair {
        self.signers.first().unwrap()
    }

    /// Pack the instructions into transactions. This will return a vector of transactions that can be submitted to the network.
    pub fn pack(&mut self) -> Result<Vec<Transaction>, JibError> {
        debug!(
            "Commitment level: {:?}",
            self.tpu_client.rpc_client().commitment()
        );
        if self.ixes.is_empty() {
            return Err(JibError::NoInstructions);
        }

        let mut packed_transactions = Vec::new();

        let mut instructions = Vec::new();
        let payer_pubkey = self.signers.first().ok_or(JibError::NoSigners)?.pubkey();

        let mut current_transaction =
            Transaction::new_with_payer(&instructions, Some(&payer_pubkey));
        let signers: Vec<&Keypair> = self.signers.iter().map(|k| k as &Keypair).collect();

        let latest_blockhash = self
            .tpu_client
            .rpc_client()
            .get_latest_blockhash()
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

                // clear instructions except for last one
                instructions = vec![ix.clone()];
            } else {
                current_transaction = tx;
            }
        }
        packed_transactions.push(current_transaction);

        debug!("Packed transactions: {}", packed_transactions.len());

        Ok(packed_transactions)
    }

    /// Pack the instructions into transactions and submit them to the network via the TPU client. This will return a spinner while the transactions are being submitted.
    pub fn hoist(&mut self) -> Result<Vec<JibResult>, JibError> {
        let packed_transactions = self.pack()?;

        let signers: Vec<&Keypair> = self.signers.iter().map(|k| k as &Keypair).collect();

        let messages = packed_transactions
            .as_slice()
            .iter()
            .map(|tx| tx.message.clone())
            .collect::<Vec<_>>();

        let results = self
            .send_and_confirm_messages_with_spinner(messages.as_slice(), &signers)
            .map_err(|e| JibError::TransactionError(e.to_string()))?;

        Ok(results)
    }

    /// Submit pre-packed transactions to the network via the TPU client. This will return a spinner while the transactions are being submitted.
    pub fn submit_packed_transactions(
        self,
        transactions: Vec<Transaction>,
    ) -> Result<Vec<JibResult>, JibError> {
        let signers: Vec<&Keypair> = self.signers.iter().map(|k| k as &Keypair).collect();

        let messages = transactions
            .as_slice()
            .iter()
            .map(|tx| tx.message.clone())
            .collect::<Vec<_>>();

        let results = self
            .send_and_confirm_messages_with_spinner(messages.as_slice(), &signers)
            .map_err(|e| JibError::TransactionError(e.to_string()))?;

        Ok(results)
    }

    // Pulled from tpu_client code and modified to return Jib results for better error handling and retrying failed transactions.
    fn send_and_confirm_messages_with_spinner<T: Signers>(
        &self,
        messages: &[Message],
        signers: &T,
    ) -> Result<Vec<JibResult>, TpuSenderError> {
        let mut expired_blockhash_retries = 5;

        let progress_bar = new_progress_bar();
        progress_bar.set_message("Setting up...");

        let mut jib_results = Vec::with_capacity(messages.len());

        let mut transactions = messages
            .iter()
            .enumerate()
            .map(|(i, message)| (i, Transaction::new_unsigned(message.clone())))
            .collect::<Vec<_>>();

        let total_transactions = transactions.len();

        let mut transaction_errors = vec![None; transactions.len()];
        let mut confirmed_transactions = 0;
        let mut block_height = self.rpc_client().get_block_height()?;

        while expired_blockhash_retries > 0 {
            let (blockhash, last_valid_block_height) = self
                .rpc_client()
                .get_latest_blockhash_with_commitment(self.rpc_client().commitment())?;

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
                        if !self.tpu_client.send_transaction(transaction) {
                            let _result = self.rpc_client().send_transaction(transaction).ok();
                        }
                        set_message_for_confirmed_transactions(
                            &progress_bar,
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
                    &progress_bar,
                    confirmed_transactions,
                    total_transactions,
                    Some(block_height),
                    last_valid_block_height,
                    &format!("Waiting for next block, {} pending...", num_transactions),
                );
                let mut new_block_height = block_height;
                while block_height == new_block_height && block_height_refreshes > 0 {
                    sleep(Duration::from_millis(500));
                    new_block_height = self.rpc_client().get_block_height()?;
                    block_height_refreshes -= 1;
                }
                block_height = new_block_height;

                // Collect statuses for the transactions, drop those that are confirmed
                let pending_signatures = pending_transactions.keys().cloned().collect::<Vec<_>>();
                for pending_signatures_chunk in
                    pending_signatures.chunks(MAX_GET_SIGNATURE_STATUSES_QUERY_ITEMS)
                {
                    if let Ok(result) = self
                        .rpc_client()
                        .get_signature_statuses(pending_signatures_chunk)
                    {
                        let statuses = result.value;
                        for (signature, status) in
                            pending_signatures_chunk.iter().zip(statuses.into_iter())
                        {
                            if let Some(status) = status {
                                if status.satisfies_commitment(self.rpc_client().commitment()) {
                                    if let Some((i, _)) = pending_transactions.remove(signature) {
                                        confirmed_transactions += 1;
                                        if status.err.is_some() {
                                            progress_bar.println(format!(
                                                "Failed transaction: {:?}",
                                                status
                                            ));
                                            jib_results.push(JibResult::Failure(
                                                JibFailedTransaction {
                                                    transaction: transactions[i].1.clone(),
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
                        &progress_bar,
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
            progress_bar.println(format!(
                "Blockhash expired. {} retries remaining",
                expired_blockhash_retries
            ));
            expired_blockhash_retries -= 1;
        }
        Err(TpuSenderError::Custom("Max retries exceeded".into()))
    }
}

// Pulled from tpu_client to support 'send_and_confirm_messages_with_spinner' function.
fn new_progress_bar() -> ProgressBar {
    let progress_bar = ProgressBar::new(42);
    progress_bar.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {wide_msg}")
            .unwrap(),
    );
    progress_bar.enable_steady_tick(Duration::from_millis(100));
    progress_bar
}

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
