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
//! ```rust
//! fn main() -> Result<()> {
//!     // Load our keypair file.
//!     let keypair = solana_sdk::signature::read_keypair_file("keypair.json").unwrap();
//!
//!     // Initialize Jib with our keypair.
//!     let mut jib = Jib::new(vec![keypair])?;
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

use std::sync::Arc;

use solana_client::{
    rpc_client::RpcClient,
    tpu_client::{TpuClient, TpuClientConfig},
};
use solana_sdk::{
    commitment_config::CommitmentConfig, instruction::Instruction, signature::Keypair,
    signer::Signer, transaction::Transaction,
};
use tracing::debug;

mod error;

use error::JibError;

const MAX_TX_LEN: usize = 1232;

pub enum Network {
    Devnet,
    MainnetBeta,
    Testnet,
}

impl Network {
    pub fn url(&self) -> &'static str {
        match self {
            Network::Devnet => "https://api.devnet.solana.com",
            Network::MainnetBeta => "https://api.mainnet-beta.solana.com",
            Network::Testnet => "https://api.testnet.solana.com",
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

impl Jib {
    /// Create a new Jib instance. You should pass in all the signers you want to use for the transactions.
    pub fn new(signers: Vec<Keypair>) -> Result<Self, JibError> {
        let tpu_client = Self::create_tpu_client("https://api.devnet.solana.com")?;

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
    pub fn hoist(mut self) -> Result<(), JibError> {
        let packed_transactions = self.pack()?;

        let signers: Vec<&Keypair> = self.signers.iter().map(|k| k as &Keypair).collect();

        let messages = packed_transactions
            .as_slice()
            .iter()
            .map(|tx| tx.message.clone())
            .collect::<Vec<_>>();

        self.tpu_client
            .send_and_confirm_messages_with_spinner(messages.as_slice(), &signers)
            .map_err(|_| JibError::TransactionError)?;

        Ok(())
    }
}
