[![Crate](https://img.shields.io/crates/v/jib)](https://crates.io/crates/jib)
[![Downloads](https://img.shields.io/crates/d/jib)](https://crates.io/crates/jib)
[![License](https://img.shields.io/crates/l/jib)](https://github.com/samuelvanderwaal/jib/blob/main/LICENSE)


 # Jib

 Jib is a simple Rust library that efficiently packs a vector of Solana instructions into maximum size and account length transactions
 and then sends them over the network. It can be used with a RPC node to send the transactions async using Tokio green threads or with a TPU client
 which will send the transactions to the current leader and confirm them. It also has the ability to retry failed transactions and pack them into new transactions.

 ## Example Usage

 In this example we load a list of mint accounts we want to update the metadata for, and then we create a vector of instructions with the update_metadata_accounts_v2 instruction for each mint.
 We then pass this vector of instructions to Jib and it will pack them into the most efficient transactions possible and send them to the network.

 ```rust
 fn main() -> Result<()> {
     // Load our keypair file.
     let keypair = solana_sdk::signature::read_keypair_file("keypair.json").unwrap();

     // Initialize Jib with our keypair and desired network.
     let mut jib = Jib::new(vec![keypair], "https://frosty-forest-fields.solana-mainnet.quiknode.pro")?;

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
 