use bs58;
use std::str::FromStr;
use solana_sdk::pubkey::Pubkey;
use colored::Colorize;
use crate::common::logger::Logger;
use lazy_static;
use yellowstone_grpc_proto::geyser::SubscribeUpdateTransaction;
use std::time::Instant;
// Import RAYDIUM_LAUNCHPAD_PROGRAM
use crate::dex::raydium_launchpad::RAYDIUM_LAUNCHPAD_PROGRAM;
// Create a static logger for this module
lazy_static::lazy_static! {
    static ref LOGGER: Logger = Logger::new("[PARSER] => ".blue().to_string());
}

// Quiet parser logs; sniper logic will log only for focus tokens
#[inline]
fn dex_log(_msg: String) {}

#[derive(Clone, Debug, PartialEq)]
pub enum DexType {
    RaydiumLaunchpad,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct TradeInfoFromToken {
    // Common fields
    pub dex_type: DexType,
    pub slot: u64,
    pub signature: String,
    pub pool_id: String,
    pub mint: String,
    pub timestamp: u64,
    pub is_buy: bool,
    pub price: u64,
    pub is_reverse: bool,
    pub coin_creator: Option<String>,
    pub sol_change: f64,
    pub token_change: f64,
    pub liquidity: f64,  // this is for filtering out small trades
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
}

/// Helper function to check if transaction contains Buy instruction
fn has_buy_instruction(txn: &SubscribeUpdateTransaction) -> bool {
    if let Some(tx_inner) = &txn.transaction {
        if let Some(meta) = &tx_inner.meta {
            return meta.log_messages.iter().any(|log| {
                log.contains("Program log: Instruction: Buy") || 
                log.contains("Program log: Instruction: Swap")
            });
        }
    }
    false
}

/// Helper function to check if transaction contains Sell instruction
fn has_sell_instruction(txn: &SubscribeUpdateTransaction) -> bool {
    if let Some(tx_inner) = &txn.transaction {
        if let Some(meta) = &tx_inner.meta {
            return meta.log_messages.iter().any(|log| {
                log.contains("Program log: Instruction: Sell") ||
                log.contains("Program log: Instruction: Swap")
            });
        }
    }
    false
}

/// Parses the transaction data buffer into a TradeInfoFromToken struct
pub fn parse_transaction_data(txn: &SubscribeUpdateTransaction, buffer: &[u8]) -> Option<TradeInfoFromToken> {
    fn parse_public_key(buffer: &[u8], offset: usize) -> Option<String> {
        if offset + 32 > buffer.len() {
            return None;
        }
        Some(bs58::encode(&buffer[offset..offset+32]).into_string())
    }

    fn parse_u64(buffer: &[u8], offset: usize) -> Option<u64> {
        if offset + 8 > buffer.len() {
            return None;
        }
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&buffer[offset..offset+8]);
        Some(u64::from_le_bytes(bytes))
    }

    fn parse_u8(buffer: &[u8], offset: usize) -> Option<u8> {
        if offset >= buffer.len() {
            return None;
        }
        Some(buffer[offset])
    }
    
    // Helper function to extract token mint from token balances
    fn extract_token_info(
        txn: &SubscribeUpdateTransaction,
    ) -> String {
        
        let mut mint = String::new();
        
        // Try to extract from token balances if txn is available
        if let Some(tx_inner) = &txn.transaction {
            if let Some(meta) = &tx_inner.meta {
                // Check post token balances
                if !meta.post_token_balances.is_empty() {
                    mint = meta.post_token_balances[0].mint.clone();
                    
                    // Skip WSOL and look for the actual token
                    if mint == "So11111111111111111111111111111111111111112" {
                        if meta.post_token_balances.len() > 1 {
                            mint = meta.post_token_balances[1].mint.clone();
                        }
                    }
                }
            }
        }
        
        // If we couldn't extract from token balances, use default
        if mint.is_empty() {
            mint = "2ivzYvjnKqA4X3dVvPKr7bctGpbxwrXbbxm44TJCpump".to_string();
        }
        
        mint
    }
    
    let start_time = Instant::now();
    
    // Extract token mint
    let mint = extract_token_info(&txn);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    
    // Determine if this is a buy or sell based on instruction logs
    let is_buy = has_buy_instruction(txn);
    
    // For Raydium Launchpad, we'll use simplified parsing
    // In a real implementation, you'd parse the specific Raydium instruction data
    let price = 1000000000; // Default price in lamports
    let sol_change = if is_buy { -0.1 } else { 0.1 }; // Example values
    let token_change = if is_buy { 1000000.0 } else { -1000000.0 }; // Example values
    let liquidity = 1000.0; // Example liquidity
    let virtual_sol_reserves = 30000000000; // 30 SOL in lamports
    let virtual_token_reserves = 1000000000000000; // 1B tokens
    
    dex_log(format!("RaydiumLaunchpad {}: {} SOL (Price: {})", 
        if is_buy { "BUY" } else { "SELL" },
        sol_change.abs(), 
        price as f64 / 1_000_000_000.0
    ).green().to_string());
    
    Some(TradeInfoFromToken {
        dex_type: DexType::RaydiumLaunchpad,
        slot: 0, // Will be set from transaction data
        signature: String::new(), // Will be set from transaction data
        pool_id: String::new(), // Will be set from transaction data
        mint: mint.clone(),
        timestamp,
        is_buy,
        price,
        is_reverse: false, // Raydium Launchpad doesn't use reverse logic
        coin_creator: None, // Will be extracted from metadata if available
        sol_change,
        token_change,
        liquidity,
        virtual_sol_reserves,
        virtual_token_reserves,
    })
}

/// Main function to process transaction and extract trade information
pub fn process_transaction(txn: &SubscribeUpdateTransaction) -> Option<TradeInfoFromToken> {
    // Check if this transaction involves the Raydium Launchpad program
    if let Some(tx_inner) = &txn.transaction {
        if let Some(transaction) = &tx_inner.transaction {
            if let Some(message) = &transaction.message {
                // Check if any of the account keys match the Raydium Launchpad program
                let raydium_program_id = match Pubkey::from_str(&RAYDIUM_LAUNCHPAD_PROGRAM) {
                    Ok(pubkey) => pubkey,
                    Err(_) => return None,
                };
                
                if message.account_keys.contains(&raydium_program_id) {
                    // Extract instruction data if available
                    if let Some(meta) = &tx_inner.meta {
                        if let Some(inner_instructions) = &meta.inner_instructions {
                            for inner_instruction in inner_instructions {
                                for instruction in &inner_instruction.instructions {
                                    if let Some(data) = &instruction.data {
                                        if let Some(trade_info) = parse_transaction_data(txn, data) {
                                            return Some(trade_info);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    
    None
}