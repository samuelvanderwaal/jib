use thiserror::Error;

#[derive(Error, Debug)]
pub enum JibError {
    #[error("No instructions to hoist")]
    NoInstructions,
    #[error("Failed to create the TPU client: {0}")]
    FailedToCreateTpuClient(String),
    #[error("No recent blockhash")]
    NoRecentBlockhash,
    #[error("Send batch transaction failed: {0}")]
    BatchTransactionError(String),
    #[error("Transaction Error")]
    TransactionError,
    #[error("No signers found")]
    NoSigners,
}
