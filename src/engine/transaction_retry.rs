use std::sync::Arc;
use std::time::{Duration, Instant};
use anyhow::{anyhow, Result};
use anchor_client::solana_sdk::{
    pubkey::Pubkey, 
    signature::{Signature, Keypair}, 
    instruction::Instruction,
    transaction::{VersionedTransaction, Transaction},
    signer::Signer,
    hash::Hash,
};
use spl_associated_token_account::get_associated_token_address;
use colored::Colorize;
use tokio::time::sleep;
use base64;

use crate::common::{
    config::{AppState, SwapConfig},
    logger::Logger,
};
use crate::engine::swap::SwapDirection;
use crate::services::jupiter_api::JupiterClient;
use crate::engine::transaction_parser::TradeInfoFromToken;
use crate::core::tx;

/// Maximum number of retry attempts for selling transactions
const MAX_RETRIES: u32 = 3;

/// Delay between retry attempts
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Timeout for transaction verification
const VERIFICATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Result of a selling transaction attempt
#[derive(Debug)]
pub struct SellTransactionResult {
    pub success: bool,
    pub signature: Option<Signature>,
    pub error: Option<String>,
    pub used_jupiter_fallback: bool,
    pub attempt_count: u32,
}

/// Enhanced transaction verification with retry logic
pub async fn verify_transaction_with_retry(
    signature: &Signature,
    app_state: Arc<AppState>,
    logger: &Logger,
    max_retries: u32,
) -> Result<bool> {
    let mut retry_count = 0;
    
    while retry_count < max_retries {
        match app_state.rpc_client.get_signature_status(signature).await {
            Ok(Some(status)) => {
                if status.confirmation_status == Some(solana_transaction_status::TransactionConfirmationStatus::Confirmed) {
                    return Ok(true);
                } else if status.confirmation_status == Some(solana_transaction_status::TransactionConfirmationStatus::Finalized) {
                    return Ok(true);
                } else {
                    logger.log(format!("Transaction not confirmed yet, retry {}/{}", retry_count + 1, max_retries));
                    retry_count += 1;
                    sleep(RETRY_DELAY).await;
                }
            }
            Ok(None) => {
                logger.log(format!("Transaction not found, retry {}/{}", retry_count + 1, max_retries));
                retry_count += 1;
                sleep(RETRY_DELAY).await;
            }
            Err(e) => {
                logger.log(format!("Error verifying transaction: {}, retry {}/{}", e, retry_count + 1, max_retries));
                retry_count += 1;
                sleep(RETRY_DELAY).await;
            }
        }
    }
    
    Err(anyhow!("Transaction verification failed after {} retries", max_retries))
}

/// Execute sell transaction with comprehensive retry logic
pub async fn execute_sell_with_retry(
    trade_info: &TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<SellTransactionResult> {
    let mut attempt_count = 0;
    let mut last_error = None;
    
    // Try Raydium Launchpad first
    while attempt_count < MAX_RETRIES {
        attempt_count += 1;
        logger.log(format!("Sell attempt {}/{} for token {}", attempt_count, MAX_RETRIES, trade_info.mint).yellow().to_string());
        
        match execute_raydium_sell_attempt(trade_info, sell_config.clone(), app_state.clone(), logger).await {
            Ok(signature) => {
                logger.log(format!("Raydium sell transaction sent: {}", signature).green().to_string());
                
                // Verify the transaction
                match verify_transaction_with_retry(&signature, app_state.clone(), logger, 3).await {
                    Ok(verified) => {
                        if verified {
                            logger.log("Raydium sell transaction verified successfully".green().to_string());
                            return Ok(SellTransactionResult {
                                success: true,
                                signature: Some(signature),
                                error: None,
                                used_jupiter_fallback: false,
                                attempt_count,
                            });
                        } else {
                            last_error = Some("Transaction verification failed".to_string());
                        }
                    }
                    Err(e) => {
                        last_error = Some(format!("Transaction verification error: {}", e));
                    }
                }
            }
            Err(e) => {
                last_error = Some(format!("Raydium sell failed: {}", e));
                logger.log(format!("Raydium sell attempt {} failed: {}", attempt_count, e).red().to_string());
            }
        }
        
        if attempt_count < MAX_RETRIES {
            sleep(RETRY_DELAY).await;
        }
    }
    
    // If Raydium failed, try Jupiter as fallback
    logger.log("Raydium sell failed, trying Jupiter fallback...".yellow().to_string());
    
    match execute_jupiter_sell_attempt(trade_info, sell_config, app_state.clone(), logger).await {
        Ok(signature) => {
            logger.log(format!("Jupiter sell transaction sent: {}", signature).green().to_string());
            
            // Verify the transaction
            match verify_transaction_with_retry(&signature, app_state.clone(), logger, 3).await {
                Ok(verified) => {
                    if verified {
                        logger.log("Jupiter sell transaction verified successfully".green().to_string());
                        return Ok(SellTransactionResult {
                            success: true,
                            signature: Some(signature),
                            error: None,
                            used_jupiter_fallback: true,
                            attempt_count,
                        });
                    } else {
                        last_error = Some("Jupiter transaction verification failed".to_string());
                    }
                }
                Err(e) => {
                    last_error = Some(format!("Jupiter transaction verification error: {}", e));
                }
            }
        }
        Err(e) => {
            last_error = Some(format!("Jupiter sell failed: {}", e));
            logger.log(format!("Jupiter sell failed: {}", e).red().to_string());
        }
    }
    
    Ok(SellTransactionResult {
        success: false,
        signature: None,
        error: last_error,
        used_jupiter_fallback: true,
        attempt_count,
    })
}

/// Execute Raydium sell attempt
async fn execute_raydium_sell_attempt(
    trade_info: &TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<Signature> {
    let raydium = crate::dex::raydium_launchpad::RaydiumLaunchpad::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );

    let (keypair, instructions, _price) = raydium.build_swap_from_parsed_data(trade_info, sell_config).await
        .map_err(|e| anyhow!("Failed to build Raydium swap: {}", e))?;

    let recent_blockhash = crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await
        .ok_or_else(|| anyhow!("Failed to get recent blockhash"))?;

    let signature = crate::core::tx::new_signed_and_send_zeroslot(
        app_state.zeroslot_rpc_client.clone(),
        recent_blockhash,
        &keypair,
        instructions,
        logger,
    ).await
    .map_err(|e| anyhow!("Failed to send Raydium transaction: {}", e))?;

    if signature.is_empty() {
        return Err(anyhow!("No signature returned from Raydium transaction"));
    }

    let signature = Signature::from_str(&signature[0])
        .map_err(|e| anyhow!("Failed to parse signature: {}", e))?;
    Ok(signature)
}

/// Execute Jupiter sell attempt as fallback
async fn execute_jupiter_sell_attempt(
    trade_info: &TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<Signature> {
    let jupiter_client = JupiterClient::new();
    
    // Get wallet public key
    let wallet_pubkey = app_state.wallet.try_pubkey()
        .map_err(|_| anyhow!("Failed to get wallet public key"))?;
    
    // Get token mint
    let token_mint = Pubkey::from_str(&trade_info.mint)
        .map_err(|_| anyhow!("Invalid token mint"))?;
    
    // Get associated token account
    let token_account = get_associated_token_address(&wallet_pubkey, &token_mint);
    
    // Get SOL mint for WSOL
    let sol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112")
        .map_err(|_| anyhow!("Invalid SOL mint"))?;
    
    // Get WSOL account
    let wsol_account = get_associated_token_address(&wallet_pubkey, &sol_mint);
    
    // Calculate amount to sell (use a percentage of the token amount)
    let amount_to_sell = (trade_info.token_change * 0.5) as u64; // Sell 50% of tokens
    
    // Get quote from Jupiter
    let quote = jupiter_client.get_quote(
        &token_mint,
        &sol_mint,
        amount_to_sell,
        sell_config.slippage_bps,
    ).await
    .map_err(|e| anyhow!("Failed to get Jupiter quote: {}", e))?;
    
    // Get swap transaction from Jupiter
    let swap_transaction = jupiter_client.get_swap_transaction(
        &quote,
        &wallet_pubkey,
        &token_account,
        &wsol_account,
    ).await
    .map_err(|e| anyhow!("Failed to get Jupiter swap transaction: {}", e))?;
    
    // Send the transaction
    let signature = app_state.rpc_client.send_and_confirm_transaction(&swap_transaction)
        .await
        .map_err(|e| anyhow!("Failed to send Jupiter transaction: {}", e))?;
    
    Ok(signature)
}