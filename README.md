# Jib

Jib is a simple Rust library that efficiently packs a vector of Solana instructions into maximum size and account length transactions
and then uses the TPU client to send them directly to the current leader, rather than via a RPC node.

It still uses a RPC client to determine the current leader, but it can be used with default public nodes as the single operation required
does not need a high-throughput private RPC node.

## Example Usage

In this example we load a list of mint accounts we want to update the metadata for, and then we create a vector of instructions with the update_metadata_accounts_v2 instruction for each mint. 
We then pass this vector of instructions to Jib and it will pack them into the most efficient transactions possible and send them to the current leader.

```rust
fn main() -> Result<()> {
    // Load our keypair file.
    let keypair = solana_sdk::signature::read_keypair_file("keypair.json").unwrap();

    // Initialize Jib with our keypair and desired network. Devnet is also the default value: Network::default().
    let mut jib = Jib::new(vec![keypair], Network::Devnet)?;

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
    let results = jib.hoist()?;

    // Do something with the results.
    for result in results {
        if result.is_success() {
            println!("Success: {}", result.signature().unwrap());
        } else {
            println!("Failure: {}", result.error().unwrap());
        }
    }

    Ok(())
}
```