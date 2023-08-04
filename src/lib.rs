use std::sync::Arc;

use solana_client::{
    rpc_client::RpcClient,
    tpu_client::{TpuClient, TpuClientConfig},
};
use solana_sdk::{
    commitment_config::CommitmentConfig, instruction::Instruction, signature::Keypair,
    signer::Signer, transaction::Transaction,
};

mod error;

use error::JibError;

const MAX_TX_LEN: usize = 1232;

pub struct Jib {
    tpu_client: TpuClient,
    signers: Vec<Keypair>,
    ixes: Vec<Instruction>,
}

impl Jib {
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

    pub fn set_rpc_url(&mut self, url: &str) {
        self.tpu_client = Self::create_tpu_client(url).unwrap();
    }

    pub fn set_instructions(&mut self, ixes: Vec<Instruction>) {
        self.ixes = ixes;
    }

    pub fn rpc_client(&self) -> &RpcClient {
        self.tpu_client.rpc_client()
    }

    pub fn payer(&self) -> &Keypair {
        self.signers.first().unwrap()
    }

    pub fn hoist(mut self) -> Result<(), JibError> {
        println!(
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

            println!("tx_len: {}", tx_len);

            if tx_len > MAX_TX_LEN || tx.message.account_keys.len() > 64 {
                packed_transactions.push(current_transaction.clone());
                println!("Packed instructions: {}", instructions.len());

                // clear instructions except for last one
                instructions = vec![ix.clone()];
            } else {
                current_transaction = tx;
            }
        }
        packed_transactions.push(current_transaction);

        println!("Packed transactions: {}", packed_transactions.len());

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
