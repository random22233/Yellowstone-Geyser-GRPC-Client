use clap::{App, Arg};
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    system_instruction,
    transaction::Transaction,
};
use std::{fs::File, io::Read, str::FromStr, time::Instant};
use tokio::time::Duration;
use tonic::{metadata::MetadataValue, transport::Channel, Request};
use yaml_rust::YamlLoader;

// Import the Yellowstone Geyser gRPC client
// This is a simplified version, you'll need to generate the actual client from .proto files
pub mod yellowstone {
    tonic::include_proto!("yellowstone");
}

use yellowstone::{geyser_client::GeyserClient, BlockSubscribeRequest, UpdateFilter};

#[derive(Debug, Serialize, Deserialize)]
struct Config {
    api_key: String,
    grpc_endpoint: String,
    from_wallet: WalletInfo,
    to_wallet: WalletInfo,
    solana_network: String,
    sol_amount: f64,
}

#[derive(Debug, Serialize, Deserialize)]
struct WalletInfo {
    address: String,
    keypair_path: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let matches = App::new("Yellowstone Geyser Block Watcher")
        .version("1.0")
        .author("Your Name")
        .about("Watches for new blocks and sends SOL transactions")
        .arg(
            Arg::with_name("config")
                .short("c")
                .long("config")
                .value_name("FILE")
                .help("Sets a custom config file (default: config.yaml)")
                .takes_value(true)
                .default_value("config.yaml"),
        )
        .get_matches();

    let config_path = matches.value_of("config").unwrap();

    // Read and parse config file
    let mut file = File::open(config_path)?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    
    let docs = YamlLoader::load_from_str(&content)?;
    let doc = &docs[0];
    
    let config = Config {
        api_key: doc["api_key"].as_str().unwrap_or_default().to_string(),
        grpc_endpoint: doc["grpc_endpoint"].as_str().unwrap_or_default().to_string(),
        from_wallet: WalletInfo {
            address: doc["from_wallet"]["address"].as_str().unwrap_or_default().to_string(),
            keypair_path: doc["from_wallet"]["keypair_path"].as_str().map(|s| s.to_string()),
        },
        to_wallet: WalletInfo {
            address: doc["to_wallet"]["address"].as_str().unwrap_or_default().to_string(),
            keypair_path: None,
        },
        solana_network: doc["solana_network"].as_str().unwrap_or("devnet").to_string(),
        sol_amount: doc["sol_amount"].as_f64().unwrap_or(0.001),
    };

    println!("Starting Yellowstone Geyser Block Watcher");
    println!("GRPC Endpoint: {}", config.grpc_endpoint);
    println!("Source wallet: {}", config.from_wallet.address);
    println!("Destination wallet: {}", config.to_wallet.address);
    println!("SOL amount per transaction: {}", config.sol_amount);
    println!("Connecting to Solana network: {}", config.solana_network);

    // Setup Solana RPC client
    let url = match config.solana_network.as_str() {
        "mainnet-beta" => "https://api.mainnet-beta.solana.com",
        "testnet" => "https://api.testnet.solana.com",
        "devnet" => "https://api.devnet.solana.com",
        _ => "https://api.devnet.solana.com",
    };
    
    let solana_client = RpcClient::new_with_commitment(url.to_string(), CommitmentConfig::confirmed());
    
    // Connect to Yellowstone Geyser gRPC
    let mut client = create_grpc_client(&config).await?;
    
    // Subscribe to new blocks
    let mut filter = UpdateFilter::default();
    filter.blocks = true;
    filter.transactions = false;
    filter.accounts = vec![];
    filter.slots = false;

    let request = Request::new(BlockSubscribeRequest {
        filter: Some(filter),
    });

    println!("Subscribing to block updates...");
    let mut stream = client.block_subscribe(request).await?.into_inner();

    println!("Waiting for new blocks...");
    let mut blocks_seen = 0;
    
    // Process block updates
    while let Some(update) = stream.next().await {
        match update {
            Ok(block_update) => {
                blocks_seen += 1;
                let block_info = block_update.block.unwrap_or_default();
                let slot = block_info.slot;
                
                println!("Block received at slot: {}", slot);
                
                // Send SOL transaction
                match send_sol_transaction(&solana_client, &config).await {
                    Ok(signature) => {
                        println!("Transaction sent successfully! Signature: {}", signature);
                    }
                    Err(e) => {
                        eprintln!("Failed to send transaction: {}", e);
                    }
                }
                
                // Log stats occasionally
                if blocks_seen % 10 == 0 {
                    println!("Total blocks seen: {}", blocks_seen);
                }
            }
            Err(e) => {
                eprintln!("Error receiving block update: {}", e);
            }
        }
    }

    Ok(())
}

async fn create_grpc_client(config: &Config) -> Result<GeyserClient<Channel>, Box<dyn std::error::Error>> {
    // Create metadata with API key
    let mut metadata = tonic::metadata::MetadataMap::new();
    let api_key_value = MetadataValue::from_str(&config.api_key)?;
    metadata.insert("x-api-key", api_key_value);
    
    // Connect to the gRPC endpoint with API key
    let channel = Channel::from_shared(config.grpc_endpoint.clone())?
        .timeout(Duration::from_secs(30))
        .connect()
        .await?;
    
    let client = GeyserClient::with_interceptor(channel, move |mut req: Request<()>| {
        req.metadata_mut().extend(metadata.clone());
        Ok(req)
    });
    
    Ok(client)
}

async fn send_sol_transaction(
    client: &RpcClient,
    config: &Config,
) -> Result<String, Box<dyn std::error::Error>> {
    let start_time = Instant::now();
    
    // Convert SOL to lamports
    let amount = (config.sol_amount * 1_000_000_000.0) as u64;
    
    // Load keypair from file
    let keypair_path = match &config.from_wallet.keypair_path {
        Some(path) => path,
        None => return Err("No keypair path provided for source wallet".into()),
    };
    
    let from_keypair = read_keypair_file(keypair_path)
        .map_err(|e| format!("Failed to read keypair file: {}", e))?;
    
    // Parse public keys
    let from_pubkey = Pubkey::from_str(&config.from_wallet.address)
        .map_err(|e| format!("Invalid source address: {}", e))?;
    let to_pubkey = Pubkey::from_str(&config.to_wallet.address)
        .map_err(|e| format!("Invalid destination address: {}", e))?;
    
    // Verify the loaded keypair matches the expected pubkey
    if from_keypair.pubkey() != from_pubkey {
        return Err(format!(
            "Keypair public key ({}) doesn't match the specified from address ({})",
            from_keypair.pubkey(), from_pubkey
        )
        .into());
    }
    
    // Create transfer instruction
    let instruction = system_instruction::transfer(&from_pubkey, &to_pubkey, amount);
    
    // Get recent blockhash
    let recent_blockhash = client.get_latest_blockhash()?;
    
    // Create and sign transaction
    let transaction = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&from_pubkey),
        &[&from_keypair],
        recent_blockhash,
    );
    
    // Send transaction
    let signature = client.send_transaction(&transaction)?;
    
    let elapsed = start_time.elapsed();
    println!("Transaction processed in {:?}", elapsed);
    
    Ok(signature.to_string())
}
