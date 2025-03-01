pub mod config;

use std::{path::PathBuf, sync::Arc, time::{Duration, Instant}, collections::HashMap};

use alloy_network::Network;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{U256, Address};
use alloy_provider::{Provider, ProviderBuilder, WsConnect};
use alloy_signer_local::PrivateKeySigner;
use config::CTFConfig;
use eyre::eyre::{Result, eyre};
use futures::{StreamExt, stream::FuturesUnordered};
use pbh_ctf::{
    CTFTransactionBuilder, PBH_CTF_CONTRACT, PBH_ENTRY_POINT,
    bindings::{IPBHEntryPoint::IPBHEntryPointInstance, IPBHKotH::IPBHKotHInstance},
    world_id::WorldID,
};
use reqwest::Url;
use tokio::time::sleep;

// Constants for optimization
const GAS_PRICE_MULTIPLIER: f64 = 1.5; // Multiply network gas price by this factor
const MAX_PRIORITY_FEE: u64 = 5_000_000_000; // 5 gwei max priority fee
const BLOCK_TIME_BUFFER_MS: u64 = 200; // Time buffer before expected next block
const TRANSACTION_TIMEOUT_MS: u64 = 5000; // Timeout for transaction confirmation
const MAX_CONCURRENT_TXS: usize = 3; // Maximum number of concurrent transactions
const MEMPOOL_MONITORING_INTERVAL_MS: u64 = 100; // How often to check mempool

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Load configuration
    let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bin/pbh_koth.toml");
    let config = CTFConfig::load(Some(config_path.as_path()))?;
    let private_key = std::env::var("PRIVATE_KEY")?;
    let signer = private_key.parse::<PrivateKeySigner>()?;
    
    // Create multiple provider connections for redundancy and parallel operations
    let main_provider = Arc::new(
        ProviderBuilder::new()
            .on_ws(WsConnect::new(config.provider_uri.parse::<Url>()?))
            .await?,
    );
    
    // Create a second provider for monitoring
    let monitor_provider = Arc::new(
        ProviderBuilder::new()
            .on_ws(WsConnect::new(config.provider_uri.parse::<Url>()?))
            .await?,
    );

    // Initialize the WorldID
    let world_id = WorldID::new(&config.semaphore_secret)?;

    // Initialize the King of the Hill contract
    let pbh_koth = IPBHKotHInstance::new(PBH_CTF_CONTRACT, main_provider.clone());
    let game_start = pbh_koth.latestBlock().call().await?._0;
    let game_end = pbh_koth.gameEnd().call().await?._0;

    // Initialize the PBHEntrypoint contract and get the PBH nonce limit
    let pbh_entrypoint = IPBHEntryPointInstance::new(PBH_ENTRY_POINT, main_provider.clone());
    let pbh_nonce_limit = pbh_entrypoint.numPbhPerMonth().call().await?._0;

    // Get all available PBH nonces upfront
    let available_pbh_nonces = get_all_available_pbh_nonces(&world_id, main_provider.clone(), pbh_nonce_limit).await?;
    println!("Available PBH nonces: {:?}", available_pbh_nonces);
    
    // Track block times to predict next block
    let mut block_times = Vec::with_capacity(10);
    let mut last_block_time = Instant::now();
    let mut avg_block_time_ms = 12000; // Start with 12 seconds as default
    
    // Track successful transactions
    let mut successful_blocks = HashMap::new();
    
    // Subscribe to new blocks
    let mut block_stream = main_provider.subscribe_blocks().await?.into_stream();
    let mut pending_txs = FuturesUnordered::new();

    let player = signer.address();
    let mut pbh_nonce_index = 0;
    let mut wallet_nonce = main_provider.get_transaction_count(signer.address()).await?;

    // Start a background task to monitor mempool for competing transactions
    let mempool_monitor = tokio::spawn(monitor_mempool(monitor_provider.clone(), PBH_CTF_CONTRACT));

    while let Some(header) = block_stream.next().await {
        let block_number = header.number;
        
        // Update block timing metrics
        let now = Instant::now();
        let elapsed = now.duration_since(last_block_time).as_millis() as u64;
        block_times.push(elapsed);
        if block_times.len() > 10 {
            block_times.remove(0);
        }
        avg_block_time_ms = block_times.iter().sum::<u64>() / block_times.len() as u64;
        last_block_time = now;
        
        println!("Block {}: Average block time: {}ms", block_number, avg_block_time_ms);

        if block_number > game_end.to() {
            println!("The game has ended, thanks for playing!");
            break;
        }

        if block_number < game_start.to() {
            println!("The game has not started yet, please wait...");
            continue;
        }

        // Check if we already scored in this block
        if successful_blocks.contains_key(&block_number) {
            println!("Already scored in block {}", block_number);
            continue;
        }

        // Get current network conditions
        let base_fee = main_provider.get_gas_price().await?;
        let priority_fee = std::cmp::min(
            (base_fee.to::<u64>() as f64 * 0.1) as u64, 
            MAX_PRIORITY_FEE
        );
        let gas_price = (base_fee.to::<u64>() as f64 * GAS_PRICE_MULTIPLIER) as u64 + priority_fee;
        
        println!("Current gas price: {} gwei", gas_price / 1_000_000_000);

        // Prepare for the next block
        let next_block_number = block_number + 1;
        
        // If we have PBH nonces available, use them
        if pbh_nonce_index < available_pbh_nonces.len() {
            let pbh_nonce = available_pbh_nonces[pbh_nonce_index];
            pbh_nonce_index += 1;
            
            println!("Preparing PBH transaction for block {} with nonce {}", next_block_number, pbh_nonce);
            
            // Prepare PBH transaction
            let calls = pbh_ctf::king_of_the_hill_multicall(player, PBH_CTF_CONTRACT);
            let tx = CTFTransactionBuilder::new()
                .to(PBH_ENTRY_POINT)
                .nonce(wallet_nonce)
                .from(signer.address())
                .gas_price(U256::from(gas_price))
                .with_pbh_multicall(&world_id, pbh_nonce, signer.address(), calls)
                .await?
                .build(signer.clone())
                .await?;
            
            wallet_nonce += 1;
            
            // Schedule transaction to be sent just before the next block
            let tx_encoded = tx.encoded_2718();
            let provider_clone = main_provider.clone();
            let block_target = next_block_number;
            
            pending_txs.push(tokio::spawn(async move {
                // Wait until just before the expected next block
                let wait_time = avg_block_time_ms.saturating_sub(BLOCK_TIME_BUFFER_MS);
                sleep(Duration::from_millis(wait_time)).await;
                
                // Send transaction
                match provider_clone.send_raw_transaction(&tx_encoded).await {
                    Ok(pending_tx) => {
                        println!("Sent PBH transaction for block {}: {:?}", block_target, pending_tx.tx_hash());
                        
                        // Monitor transaction status
                        let start_time = Instant::now();
                        while start_time.elapsed() < Duration::from_millis(TRANSACTION_TIMEOUT_MS) {
                            match provider_clone.get_transaction_receipt(pending_tx.tx_hash()).await {
                                Ok(Some(receipt)) => {
                                    if receipt.status == 1.into() {
                                        println!("Transaction successful for block {}", block_target);
                                        return (block_target, true);
                                    } else {
                                        println!("Transaction failed for block {}", block_target);
                                        return (block_target, false);
                                    }
                                }
                                _ => sleep(Duration::from_millis(100)).await,
                            }
                        }
                        println!("Transaction timeout for block {}", block_target);
                        (block_target, false)
                    }
                    Err(e) => {
                        println!("Failed to send transaction for block {}: {:?}", block_target, e);
                        (block_target, false)
                    }
                }
            }));
        } else {
            // If we've used all PBH nonces, send regular transactions
            println!("Preparing regular transaction for block {}", next_block_number);
            
            let calldata = pbh_ctf::king_of_the_hill_calldata(player);
            let tx = CTFTransactionBuilder::new()
                .to(PBH_CTF_CONTRACT)
                .nonce(wallet_nonce)
                .from(signer.address())
                .gas_price(U256::from(gas_price))
                .input(calldata.into())
                .build(signer.clone())
                .await?;
            
            wallet_nonce += 1;
            
            // Schedule transaction to be sent just before the next block
            let tx_encoded = tx.encoded_2718();
            let provider_clone = main_provider.clone();
            let block_target = next_block_number;
            
            pending_txs.push(tokio::spawn(async move {
                // Wait until just before the expected next block
                let wait_time = avg_block_time_ms.saturating_sub(BLOCK_TIME_BUFFER_MS);
                sleep(Duration::from_millis(wait_time)).await;
                
                // Send transaction
                match provider_clone.send_raw_transaction(&tx_encoded).await {
                    Ok(pending_tx) => {
                        println!("Sent regular transaction for block {}: {:?}", block_target, pending_tx.tx_hash());
                        
                        // Monitor transaction status
                        let start_time = Instant::now();
                        while start_time.elapsed() < Duration::from_millis(TRANSACTION_TIMEOUT_MS) {
                            match provider_clone.get_transaction_receipt(pending_tx.tx_hash()).await {
                                Ok(Some(receipt)) => {
                                    if receipt.status == 1.into() {
                                        println!("Transaction successful for block {}", block_target);
                                        return (block_target, true);
                                    } else {
                                        println!("Transaction failed for block {}", block_target);
                                        return (block_target, false);
                                    }
                                }
                                _ => sleep(Duration::from_millis(100)).await,
                            }
                        }
                        println!("Transaction timeout for block {}", block_target);
                        (block_target, false)
                    }
                    Err(e) => {
                        println!("Failed to send transaction for block {}: {:?}", block_target, e);
                        (block_target, false)
                    }
                }
            }));
        }
        
        // Process completed transactions
        while let Some(Some(result)) = pending_txs.next().now_or_never() {
            match result {
                Ok((block, success)) => {
                    if success {
                        successful_blocks.insert(block, true);
                    }
                }
                Err(e) => println!("Transaction task error: {:?}", e),
            }
        }
        
        // Limit concurrent transactions
        while pending_txs.len() > MAX_CONCURRENT_TXS {
            if let Some(result) = pending_txs.next().await {
                match result {
                    Ok((block, success)) => {
                        if success {
                            successful_blocks.insert(block, true);
                        }
                    }
                    Err(e) => println!("Transaction task error: {:?}", e),
                }
            }
        }
        
        // Periodically check and update wallet nonce to prevent nonce issues
        if block_number % 10 == 0 {
            let current_nonce = main_provider.get_transaction_count(signer.address()).await?;
            if current_nonce > wallet_nonce {
                println!("Updating wallet nonce from {} to {}", wallet_nonce, current_nonce);
                wallet_nonce = current_nonce;
            }
        }
    }

    // Clean up
    mempool_monitor.abort();

    Ok(())
}

// Get all available PBH nonces at once
async fn get_all_available_pbh_nonces<P: Provider<N>, N: Network>(
    world_id: &WorldID,
    provider: P,
    max_pbh_nonce: u16,
) -> Result<Vec<u16>> {
    let mut available_nonces = Vec::new();
    let pbh_entrypoint_instance = IPBHEntryPointInstance::new(PBH_ENTRY_POINT, provider);
    
    for i in 0..=max_pbh_nonce {
        let nullifier_hash = world_id.pbh_ext_nullifier(i).2;
        let is_used = pbh_entrypoint_instance
            .nullifierHashes(nullifier_hash)
            .call()
            .await?
            ._0;
        if !is_used {
            available_nonces.push(i);
        }
    }

    if available_nonces.is_empty() {
        return Err(eyre!("No available PBH nonce"));
    }

    Ok(available_nonces)
}

// Monitor mempool for competing transactions
async fn monitor_mempool<P: Provider<N>, N: Network>(
    provider: Arc<P>,
    target_contract: Address,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(MEMPOOL_MONITORING_INTERVAL_MS));
    
    loop {
        interval.tick().await;
        
        // This is a placeholder - actual mempool monitoring depends on the provider's capabilities
        // Some providers offer pending transaction subscriptions
        // For now, we'll just log that we're monitoring
        println!("Monitoring mempool for competing transactions...");
        
        // In a real implementation, you would:
        // 1. Get pending transactions from the mempool
        // 2. Filter for transactions targeting our contract
        // 3. If competitors are detected, adjust our strategy (e.g., increase gas price)
    }
}