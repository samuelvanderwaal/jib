# Jib

Jib is a simple Rust library that efficiently packs a vector of Solana instructions into maximum size and account length transactions
and then uses the TPU client to send them directly to the current leader, rather than via a RPC node.

It still uses a RPC client to determine the current leader, but it defaults to using the public RPC nodes since this single operation 
isn't a high intensity operation.

## Example Usage

In this example we load a list of mint accounts we want to update the metadata for, and then we create a vector of instructions with the update_metadata_accounts_v2 instruction for each mint. 
We then pass this vector of instructions to Jib and it will pack them into the most efficient transactions possible and send them to the current leader.

```rust
fn main() -> Result<()> {
    // Load our keypair file.
    let keypair = solana_sdk::signature::read_keypair_file("keypair.json").unwrap();

    // Initialize Jib with our keypair.
    let mut jib = Jib::new(vec![keypair])?;

    let mut instructions = vec![];

    // Load mint addresses from a file.
    let addresses: Vec<String> =
        serde_json::from_reader(std::fs::File::open("collection_mints.json")?)?;

    // Create an instruction for each mint.
    for address in addresses {
        let metadata = derive_metadata_pda(&Pubkey::from_str(&address).unwrap());

        let ix = update_metadata_accounts_v2(
            mpl_token_metadata::ID,
            metadata,
            // Jib takes the payer by value but we can access it via this fn.
            jib.payer().pubkey(),
            None,
            None,
            Some(true),
            None,
        );

        instructions.push(ix);
    }

    // Set the instructions to be executed.
    jib.set_instructions(instructions);
    
    // Run it.
    jib.hoist()?;

    Ok(())
}
```