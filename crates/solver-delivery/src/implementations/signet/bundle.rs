//! Signet bundle delivery implementation.
//!
//! This module implements transaction delivery for Signet L2 using bundle submission.
//! Bundles combine L1 host transactions with L2 transactions and are submitted to
//! the Signet transaction cache.
//!
//! ## Current Limitations
//!
//! This initial implementation focuses on bundle creation and submission infrastructure.
//! Full integration with Phase 1 discovered SignedOrders (correlating intents with
//! fulfillment transactions) requires additional work in the solver core to pass
//! the necessary context through the delivery pipeline.

use crate::{DeliveryError, DeliveryInterface};
use alloy_eips::eip2718::Encodable2718;
use alloy_network::{AnyNetwork, EthereumWallet};
use alloy_primitives::{hex, Address as AlloyAddress, Bytes, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types::{mev::EthSendBundle, TransactionRequest};
use alloy_signer_local::PrivateKeySigner;
use async_trait::async_trait;
use signet_bundle::SignetEthBundle;
use signet_constants::{
	HostConstants, HostTokens, RollupConstants, RollupTokens, SignetSystemConstants, UsdRecords,
};
use signet_tx_cache::client::TxCache;
use signet_types::SignedFill;
use solver_dex::{DexRouter, UniswapV4Router};
use solver_types::{
	Address, ConfigSchema, Field, FieldType, NetworksConfig, Schema, SecretString,
	Transaction as SolverTransaction, TransactionHash, TransactionReceipt,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::time::{sleep, Duration};

const DEFAULT_BLOCK_NUMBER: u64 = 1;
const NUM_TARGET_BLOCKS: u64 = 10;
/// Default gas limit for transactions.
const DEFAULT_GAS_LIMIT: u64 = 1_000_000;
/// Default priority fee multiplier for transactions.
const DEFAULT_PRIORITY_FEE_MULTIPLIER: u64 = 16;
/// Multiplier for converting gwei to wei.
const GWEI_TO_WEI: u64 = 1_000_000_000;

/// Pool configuration entry loaded from configuration.
#[derive(Debug, Clone)]
pub struct DexPoolConfig {
	pub token0: alloy_primitives::Address,
	pub token1: alloy_primitives::Address,
	pub price_ratio: f64,
}

/// DEX configuration required for host-chain swaps.
#[derive(Debug, Clone)]
pub struct DexConfig {
	pub pool_manager: alloy_primitives::Address,
	pub swap_router: alloy_primitives::Address,
	pub lp_router: alloy_primitives::Address,
	pub max_routing_hops: usize,
	pub slippage_tolerance_bps: u16,
	pub default_fee_tier: u32,
	pub tick_spacing: i32,
	/// Known token addresses indexed by symbolic name (e.g. token_a, token_b, ...)
	pub tokens: HashMap<String, alloy_primitives::Address>,
	pub pools: Vec<DexPoolConfig>,
}

fn alloy_to_solver_address(address: &AlloyAddress) -> Address {
	Address(address.as_slice().to_vec())
}

fn format_alloy_address(address: &AlloyAddress) -> String {
	format!("0x{}", hex::encode(address.as_slice()))
}

/// Signet bundle delivery implementation configuration.
#[derive(Debug, Clone)]
pub struct SignetBundleConfig {
	/// Signet chain name (e.g., "pecorino")
	pub chain_name: String,
	/// Target block number for bundles (optional, defaults to 1)
	pub target_block: Option<u64>,
	/// Rollup chain ID (L2)
	pub rollup_chain_id: u64,
	/// Host chain ID (L1) where fills are executed
	pub host_chain_id: u64,
	/// OrderOrigin contract address on L2
	pub order_origin_address: alloy_primitives::Address,
	/// OrderDestination contract address on L1
	pub order_destination_address: alloy_primitives::Address,
	/// Address where filler receives input tokens
	pub filler_recipient: alloy_primitives::Address,
	/// Optional DEX configuration for host-chain swaps
	pub dex: Option<DexConfig>,
}

/// Signet bundle delivery implementation.
///
/// Submits transactions to Signet L2 by wrapping them in bundles and sending
/// to the transaction cache.
pub struct SignetBundleDelivery {
	/// Delivery configuration
	config: SignetBundleConfig,
	/// Networks configuration
	networks: NetworksConfig,
	/// Signet cache client
	cache_client: Arc<TxCache>,
	/// Solver's signer for creating SignedFills
	signer: PrivateKeySigner,
	/// Optional DEX configuration for constructing host chain swap transactions
	dex_config: Option<DexConfig>,
}

impl SignetBundleDelivery {
	pub fn host_chain_id(&self) -> u64 {
		self.config.host_chain_id
	}

	pub async fn build_host_swap_transactions(
		&self,
		signed_order: &signet_types::SignedOrder,
	) -> Result<Vec<Bytes>, DeliveryError> {
		let tx_requests = self.prepare_host_swap_transactions(signed_order).await?;
		self.sign_and_encode_txns(tx_requests, self.config.host_chain_id).await
	}

	pub fn from_config(
		config: &toml::Value,
		networks: &NetworksConfig,
		default_private_key: &SecretString,
		network_private_keys: &HashMap<u64, SecretString>,
	) -> Result<Self, DeliveryError> {
		SignetBundleDeliverySchema::validate_config(config).map_err(|e| {
			DeliveryError::Network(format!(
				"Invalid Signet bundle delivery configuration: {}",
				e
			))
		})?;

		let chain_name = config
			.get("chain_name")
			.and_then(|v| v.as_str())
			.ok_or_else(|| DeliveryError::Network("chain_name is required".to_string()))?
			.to_string();

		let target_block = config
			.get("target_block")
			.and_then(|v| v.as_integer())
			.map(|v| v as u64);

		let rollup_chain_id = config
			.get("rollup_chain_id")
			.and_then(|v| v.as_integer())
			.ok_or_else(|| DeliveryError::Network("rollup_chain_id is required".to_string()))?
			as u64;

		let host_chain_id = config
			.get("host_chain_id")
			.and_then(|v| v.as_integer())
			.ok_or_else(|| DeliveryError::Network("host_chain_id is required".to_string()))?
			as u64;

		let order_origin_address = config
			.get("order_origin_address")
			.and_then(|v| v.as_str())
			.ok_or_else(|| DeliveryError::Network("order_origin_address is required".to_string()))?
			.parse::<alloy_primitives::Address>()
			.map_err(|e| DeliveryError::Network(format!("Invalid order_origin_address: {}", e)))?;

		let order_destination_address = config
			.get("order_destination_address")
			.and_then(|v| v.as_str())
			.ok_or_else(|| {
				DeliveryError::Network("order_destination_address is required".to_string())
			})?
			.parse::<alloy_primitives::Address>()
			.map_err(|e| {
				DeliveryError::Network(format!("Invalid order_destination_address: {}", e))
			})?;

		let filler_recipient = config
			.get("filler_recipient")
			.and_then(|v| v.as_str())
			.ok_or_else(|| DeliveryError::Network("filler_recipient is required".to_string()))?
			.parse::<alloy_primitives::Address>()
			.map_err(|e| DeliveryError::Network(format!("Invalid filler_recipient: {}", e)))?;

		let dex_config = parse_dex_config(config)?;

		let private_key = network_private_keys
			.get(&host_chain_id)
			.unwrap_or(default_private_key);

		let signer = private_key
			.expose_secret()
			.parse::<PrivateKeySigner>()
			.map_err(|e| DeliveryError::Network(format!("Invalid private key: {}", e)))?;

		let delivery_config = SignetBundleConfig {
			chain_name,
			target_block,
			rollup_chain_id,
			host_chain_id,
			order_origin_address,
			order_destination_address,
			filler_recipient,
			dex: dex_config.clone(),
		};

		SignetBundleDelivery::new(delivery_config, networks.clone(), signer, dex_config)
	}

	/// Helper method to get RPC URL for a given chain ID.
	fn get_rpc_url(&self, chain_id: u64) -> Result<String, DeliveryError> {
		let network_config = self.networks.get(&chain_id).ok_or_else(|| {
			DeliveryError::Network(format!("No network config for chain {}", chain_id))
		})?;

		network_config
			.rpc_urls
			.first()
			.and_then(|rpc| rpc.http.as_ref())
			.cloned()
			.ok_or_else(|| {
				DeliveryError::Network(format!("No HTTP RPC URL for chain {}", chain_id))
			})
	}

	/// Creates a new Signet bundle delivery instance.
	pub fn new(
		config: SignetBundleConfig,
		networks: NetworksConfig,
		signer: PrivateKeySigner,
		dex_config: Option<DexConfig>,
	) -> Result<Self, DeliveryError> {
		// Validate chain name
		if config.chain_name.is_empty() {
			return Err(DeliveryError::Network(
				"chain_name cannot be empty".to_string(),
			));
		}

		// Build cache client based on chain name
		let cache_client = if config.chain_name == "pecorino" {
			TxCache::pecorino()
		} else {
			// Construct URL for other chains
			let url = format!("https://cache.{}.signet.sh", config.chain_name);
			TxCache::new_from_string(&url).map_err(|e| {
				DeliveryError::Network(format!("Failed to create Signet cache client: {}", e))
			})?
		};

		Ok(Self {
			config,
			networks,
			cache_client: Arc::new(cache_client),
			signer,
			dex_config,
		})
	}

	/// Creates a series of bundles for subsequent blocks from a solver fill transaction.
	///
	/// Generates NUM_TARGET_BLOCKS bundles, each targeting a block from
	/// (current_block + 1) up to (current_block + NUM_TARGET_BLOCKS).
	async fn create_bundles(
		&self,
		tx: &SolverTransaction,
	) -> Result<Vec<SignetEthBundle>, DeliveryError> {
		// --- (1) SignedOrder 및 L2 Initiate Tx 생성 로직은 하나만 수행

		// Extract SignedOrder from transaction metadata
		let metadata_value = tx.metadata.clone().ok_or_else(|| {
			DeliveryError::Network(
				"No SignedOrder metadata in transaction for Signet bundle".to_string(),
			)
		})?;

		let signed_order_value = match &metadata_value {
			serde_json::Value::Object(obj) => obj
				.get("signed_order")
				.cloned()
				.unwrap_or(metadata_value.clone()),
			_ => metadata_value.clone(),
		};

		tracing::debug!("Deserializing SignedOrder from transaction metadata");
		let signed_order = serde_json::from_value::<signet_types::SignedOrder>(signed_order_value)
			.map_err(|e| {
				DeliveryError::Network(format!(
					"Failed to deserialize SignedOrder from metadata: {}",
					e
				))
			})?;
		tracing::info!(
			outputs_count = signed_order.outputs().len(),
			permit_nonce = %signed_order.permit().permit.nonce,
			"Successfully deserialized SignedOrder"
		);

		// Get current rollup block number
		let current_block = self.get_block_number(self.config.rollup_chain_id).await?;

		// Create SignedFills for all target chains (both rollup and host)
		let signed_fills = self.create_signed_fills(&signed_order).await?;

		// Build rollup transaction requests (collect all requests first, then sign together)
		let mut rollup_tx_requests = Vec::new();

		// First, add fill transaction for rollup chain if it exists
		if let Some(rollup_fill) = signed_fills.get(&self.config.rollup_chain_id) {
			let fill_tx_request = rollup_fill.to_fill_tx(self.config.order_origin_address);
			rollup_tx_requests.push(fill_tx_request);
			tracing::debug!("Added rollup fill transaction request");
		}

		// Then, add initiate transaction
		let initiate_tx_request = signed_order.to_initiate_tx(
			self.config.filler_recipient,
			self.config.order_origin_address,
		);
		rollup_tx_requests.push(initiate_tx_request);

		// Sign and encode all transactions together (ensures correct nonce ordering)
		let rollup_txs = self
			.sign_and_encode_txns(rollup_tx_requests, self.config.rollup_chain_id)
			.await?;

		tracing::info!(
			rollup_txs_count = rollup_txs.len(),
			"Created rollup transactions (fill + initiate)"
		);

		// Build host transaction requests: swaps first, then host fill, so inventory is provisioned before fill runs.
		let host_swap_requests = self.prepare_host_swap_transactions(&signed_order).await?;
		let mut host_tx_requests = host_swap_requests;
		if let Some(host_fill) = signed_fills.get(&self.config.host_chain_id) {
			let fill_tx_request = host_fill.to_fill_tx(self.config.order_destination_address);
			host_tx_requests.push(fill_tx_request);
		}

		// Sign & encode host transactions in a single call to ensure contiguous nonces
		let host_txns = self
			.sign_and_encode_txns(host_tx_requests.clone(), self.config.host_chain_id)
			.await?;

		tracing::info!(
			host_txns_count = host_txns.len(),
			host_swap_txs_count = host_tx_requests.len().saturating_sub(1),
			"Prepared host-chain transactions"
		);

		// --- (2) Target Block Number만 변경하며 10개의 Bundle 생성

		let mut bundles = Vec::with_capacity(NUM_TARGET_BLOCKS as usize);
		sleep(Duration::from_secs(2)).await;

		for i in 1..=NUM_TARGET_BLOCKS {
			let target_block = current_block + i;

			tracing::debug!(
				current_block = current_block,
				target_block = target_block,
				i = i,
				host_txns_count = host_txns.len(),
				rollup_txs_count = rollup_txs.len(),
				"Creating bundle for target block"
			);

			let bundle = SignetEthBundle {
				bundle: EthSendBundle {
					txs: rollup_txs.clone(), // All rollup transactions (fill + initiate)
					block_number: target_block,
					min_timestamp: None,
					max_timestamp: None,
					reverting_tx_hashes: vec![],
					replacement_uuid: None,
					..Default::default()
				},
				host_txs: host_txns.clone(),
			};
			bundles.push(bundle);
		}
		tracing::info!("Created bundles: {:?}", bundles[0]);
		Ok(bundles)
	}

	/// Signs and encodes multiple transaction requests into RLP bytes.
	///
	/// This follows the SDK's sign_and_encode_txns pattern:
	/// 1. Set transaction fields (from, gas_limit, priority_fee) for each tx
	/// 2. Use provider.fill() to populate remaining fields (nonce, gas price, etc.)
	/// 3. Encode each signed envelope to bytes
	///
	/// CRITICAL: This method ensures correct nonce ordering by processing all
	/// transactions sequentially with the same provider instance.
	async fn sign_and_encode_txns(
		&self,
		tx_requests: Vec<TransactionRequest>,
		chain_id: u64,
	) -> Result<Vec<Bytes>, DeliveryError> {
		// Get RPC URL for target chain
		let rpc_url = self.get_rpc_url(chain_id)?;

		// Create provider with wallet (needed for fill method)
		// IMPORTANT: Use the same provider for all transactions to ensure correct nonce ordering
		let wallet = EthereumWallet::from(self.signer.clone());
		let provider = ProviderBuilder::new().wallet(wallet).connect_http(
			rpc_url
				.parse()
				.map_err(|e| DeliveryError::Network(format!("Invalid RPC URL: {}", e)))?,
		);

		let mut encoded_txs = Vec::new();
		let signer_address = self.signer.address();
		let mut next_nonce = provider
			.get_transaction_count(signer_address)
			.await
			.map_err(|e| DeliveryError::Network(format!("Failed to fetch nonce: {}", e)))?;

		// Process each transaction sequentially to ensure correct nonce ordering
		for mut tx in tx_requests {
			// Fill out the transaction fields (following SDK pattern)
			use alloy_network::TransactionBuilder;
			tx = tx
				.with_from(signer_address)
				.with_nonce(next_nonce)
				.with_gas_limit(DEFAULT_GAS_LIMIT)
				.with_max_priority_fee_per_gas(
					(GWEI_TO_WEI * DEFAULT_PRIORITY_FEE_MULTIPLIER) as u128,
				);
			tracing::debug!(
				assigned_chain = chain_id,
				assigned_nonce = next_nonce,
				"Assigned nonce for transaction prior to signing"
			);
			next_nonce = next_nonce.saturating_add(1);

			// Use provider.fill() to populate remaining fields (nonce, gas price, chain_id, etc.)
			use alloy_provider::SendableTx;
			let sendable = provider.fill(tx).await.map_err(|e| {
				DeliveryError::Network(format!("Failed to fill transaction: {}", e))
			})?;

			let filled_envelope = match sendable {
				SendableTx::Envelope(envelope) => envelope,
				_ => {
					return Err(DeliveryError::Network(
						"Expected transaction envelope from provider.fill()".to_string(),
					));
				},
			};

			// Encode the signed transaction to RLP bytes (EIP-2718 format)
			let encoded = filled_envelope.encoded_2718();
			encoded_txs.push(Bytes::from(encoded));
		}

		Ok(encoded_txs)
	}

	async fn get_erc20_balance(
		&self,
		chain_id: u64,
		owner: &AlloyAddress,
		token: &AlloyAddress,
	) -> Result<U256, DeliveryError> {
		let rpc_url = self.get_rpc_url(chain_id)?;
		let url = rpc_url
			.parse::<reqwest::Url>()
			.map_err(|e| DeliveryError::Network(format!("Invalid RPC URL: {}", e)))?;

		let provider = ProviderBuilder::new()
			.network::<AnyNetwork>()
			.connect_http(url);

		let mut call_data = Vec::with_capacity(36);
		call_data.extend_from_slice(&[0x70, 0xa0, 0x82, 0x31]);
		call_data.extend_from_slice(&[0; 12]);
		call_data.extend_from_slice(owner.as_slice());

		let result = provider
			.call(
				TransactionRequest::default()
					.to(*token)
					.input(call_data.into())
					.into(),
			)
			.await
			.map_err(|e| DeliveryError::Network(format!("Failed to call balanceOf: {}", e)))?;

		if result.len() < 32 {
			return Err(DeliveryError::Network(
				"Invalid balanceOf response".to_string(),
			));
		}

		Ok(U256::from_be_slice(&result[..32]))
	}

	async fn prepare_host_swap_transactions(
		&self,
		signed_order: &signet_types::SignedOrder,
	) -> Result<Vec<TransactionRequest>, DeliveryError> {
		let dex_config = match &self.dex_config {
			Some(config) => config,
			None => {
				tracing::debug!("No DEX configuration present; skipping host swap generation");
				return Ok(Vec::new());
			},
		};

		let host_outputs: Vec<_> = signed_order
			.outputs()
			.iter()
			.filter(|output| output.chainId as u64 == self.config.host_chain_id)
			.collect();

		if host_outputs.is_empty() {
			tracing::debug!("SignedOrder has no host-chain outputs; skipping host swap generation");
			return Ok(Vec::new());
		}

		let host_output_summaries: Vec<String> = host_outputs
			.iter()
			.map(|output| {
				format!(
					"token={} amount={} recipient={}",
					format_alloy_address(&output.token),
					output.amount,
					format_alloy_address(&output.recipient)
				)
			})
			.collect();

		tracing::info!(
			host_outputs = ?host_output_summaries,
			"Identified host-chain outputs requiring settlement"
		);

		let primary_output = host_outputs[0];
		let mut required_amount = primary_output.amount;
		for output in host_outputs.iter().skip(1) {
			if output.token == primary_output.token {
				required_amount = required_amount.saturating_add(output.amount);
			}
		}

		let target_token = primary_output.token;
		let owner = self.signer.address();

		let current_balance = self
			.get_erc20_balance(self.config.host_chain_id, &owner, &target_token)
			.await?;

		tracing::info!(
			target_token = %format_alloy_address(&target_token),
			current_balance = %current_balance,
			required_amount = %required_amount,
			"Current host-chain balance snapshot for target token"
		);

		if current_balance >= required_amount {
			tracing::info!(
				target_token = %format_alloy_address(&target_token),
				required_amount = %required_amount,
				current_balance = %current_balance,
				"Target token balance already sufficient; skipping host swap",
			);
			return Ok(Vec::new());
		}

		let amount_needed = required_amount.saturating_sub(current_balance);

		tracing::info!(
			target_token = %format_alloy_address(&target_token),
			amount_needed = %amount_needed,
			"Preparing host-chain swap transactions",
		);

		let pool_prices: Vec<(Address, Address, f64)> = dex_config
			.pools
			.iter()
			.map(|pool| {
				(
					alloy_to_solver_address(&pool.token0),
					alloy_to_solver_address(&pool.token1),
					pool.price_ratio,
				)
			})
			.collect();

		let uniswap_router = UniswapV4Router::new(
			dex_config.swap_router,
			self.config.host_chain_id,
			dex_config.default_fee_tier,
		);

		let router = DexRouter::new(
			uniswap_router,
			dex_config.max_routing_hops,
			dex_config.slippage_tolerance_bps,
			pool_prices,
		);

		let mut candidate_quotes = Vec::new();
		for (token_label, token_address) in &dex_config.tokens {
			if *token_address == target_token {
				continue;
			}

			let balance = self
				.get_erc20_balance(self.config.host_chain_id, &owner, token_address)
				.await?;

			tracing::info!(
				candidate_token = token_label,
				token_address = %format_alloy_address(token_address),
				available_balance = %balance,
				"Inventory snapshot for potential swap input token"
			);

			if balance.is_zero() {
				tracing::debug!(
					candidate_token = token_label,
					"Skipping candidate token due to zero balance"
				);
				continue;
			}

			let token_in = alloy_to_solver_address(token_address);
			let token_out = alloy_to_solver_address(&target_token);

			match router
				.find_best_route(&token_in, &token_out, amount_needed)
				.await
			{
				Ok(quote) => {
					if quote.amount_in <= balance {
						let hop_count = quote.route.path.len().saturating_sub(1);
						let route_tokens: Vec<String> = quote
							.route
							.path
							.iter()
							.map(|addr| format!("0x{}", hex::encode(&addr.0)))
							.collect();
						tracing::debug!(
							candidate_token = token_label,
							required_input = %quote.amount_in,
							estimated_output = %quote.amount_out,
							hops = hop_count,
							route_tokens = ?route_tokens,
							"Found viable host swap candidate",
						);
						candidate_quotes.push((token_label.clone(), balance, quote));
					} else {
						tracing::debug!(
							candidate_token = token_label,
							required_input = %quote.amount_in,
							available_balance = %balance,
							"Skipping candidate route due to insufficient balance",
						);
					}
				},
				Err(err) => {
					tracing::debug!(
						candidate_token = token_label,
						error = %err,
						"Failed to derive swap route for candidate token",
					);
				},
			}
		}

		if candidate_quotes.is_empty() {
			tracing::error!(
				required_amount = %required_amount,
				target_token = %format_alloy_address(&target_token),
				"Unable to identify any viable swap route to source host token"
			);
			return Err(DeliveryError::Network(
				"No viable DEX route available to source host chain token".to_string(),
			));
		}

		candidate_quotes.sort_by(|a, b| {
			let a_hops = a.2.route.path.len();
			let b_hops = b.2.route.path.len();
			a_hops
				.cmp(&b_hops)
				.then_with(|| a.2.amount_in.cmp(&b.2.amount_in))
		});

		let (selected_label, _balance, selected_quote) = candidate_quotes.first().cloned().unwrap();
		let selected_hops = selected_quote.route.path.len().saturating_sub(1);
		let selected_route_tokens: Vec<String> = selected_quote
			.route
			.path
			.iter()
			.map(|addr| format!("0x{}", hex::encode(&addr.0)))
			.collect();

		tracing::info!(
			selected_input_token = selected_label,
			required_input = %selected_quote.amount_in,
			estimated_output = %selected_quote.amount_out,
			hops = selected_hops,
			route_tokens = ?selected_route_tokens,
			"Selected host swap route (TODO: optimize cost/gas heuristics)",
		);
		// TODO: in future iterations evaluate all candidates to choose the lowest-cost option
		// using simulated gas estimates and slippage-aware pricing.

		let swap_transactions = router
			.generate_swap_transactions(&selected_quote, self.config.host_chain_id)
			.map_err(|e| {
				DeliveryError::Network(format!("Failed to generate swap transactions: {}", e))
			})?;

		let tx_requests: Vec<TransactionRequest> =
			swap_transactions.into_iter().map(|tx| tx.into()).collect();

		Ok(tx_requests)
	}

	/// Creates SignedFills for all target chains from the order's outputs.
	///
	/// This follows the SDK's sign_fills pattern:
	/// 1. Aggregate orders
	/// 2. Create UnsignedFill with deadline
	/// 3. Configure with chain addresses
	/// 4. Sign for each target chain
	async fn create_signed_fills(
		&self,
		signed_order: &signet_types::SignedOrder,
	) -> Result<std::collections::HashMap<u64, SignedFill>, DeliveryError> {
		// Get deadline from the order's permit
		let deadline = signed_order
			.permit()
			.permit
			.deadline
			.to_string()
			.parse::<u64>()
			.map_err(|e| {
				DeliveryError::Network(format!("Invalid deadline in order permit: {}", e))
			})?;

		// Get all target chain IDs from the order
		let target_chain_ids: std::collections::HashSet<u64> = signed_order
			.outputs()
			.iter()
			.map(|output| output.chainId as u64)
			.collect();

		tracing::debug!(
			rollup_chain_id = self.config.rollup_chain_id,
			host_chain_id = self.config.host_chain_id,
			target_chain_ids = ?target_chain_ids,
			deadline = deadline,
			"Creating SignedFills for all target chains"
		);

		// 2. Create UnsignedFill with deadline and rollup/host chain config
		let mut unsigned_fill = signet_types::UnsignedFill::new()
			.fill(signed_order)
			.with_deadline(deadline);

		let host_tokens = HostTokens::new(UsdRecords::new(), AlloyAddress::ZERO, AlloyAddress::ZERO);
		let host_constants = HostConstants::new(
			self.config.host_chain_id,
			0,
			self.config.order_destination_address,
			self.config.order_destination_address,
			self.config.order_destination_address,
			self.config.order_destination_address,
			host_tokens,
		);
		let rollup_tokens = RollupTokens::new(AlloyAddress::ZERO, AlloyAddress::ZERO);
		let rollup_constants = RollupConstants::new(
			self.config.rollup_chain_id,
			self.config.order_origin_address,
			self.config.order_origin_address,
			AlloyAddress::ZERO,
			rollup_tokens,
		);

		let chain_constants = SignetSystemConstants::new(host_constants, rollup_constants);
		unsigned_fill = unsigned_fill.with_chain(chain_constants);

		// 3. Configure with order contract addresses for each chain
		for chain_id in &target_chain_ids {
			if *chain_id != self.config.rollup_chain_id && *chain_id != self.config.host_chain_id {
				tracing::warn!(
					chain_id = chain_id,
					"No order contract address configured for chain"
				);
			}
		}

		// 4. Sign the fill, producing SignedFills for each target chain
		let signed_fills = unsigned_fill
			.sign(&self.signer)
			.await
			.map_err(|e| DeliveryError::Network(format!("Failed to sign fills: {}", e)))?;

		tracing::info!(
			signed_fills_count = signed_fills.len(),
			chain_ids = ?signed_fills.keys().collect::<Vec<_>>(),
			"Successfully created SignedFills for all target chains"
		);

		Ok(signed_fills)
	}
}

/// Configuration schema for Signet bundle delivery.
pub struct SignetBundleDeliverySchema;

impl SignetBundleDeliverySchema {
	/// Static validation method for use before instance creation
	pub fn validate_config(config: &toml::Value) -> Result<(), solver_types::ValidationError> {
		let instance = Self;
		instance.validate(config)
	}
}

impl ConfigSchema for SignetBundleDeliverySchema {
	fn validate(&self, config: &toml::Value) -> Result<(), solver_types::ValidationError> {
		let schema = Schema::new(
			// Required fields
			vec![
				Field::new("chain_name", FieldType::String),
				Field::new(
					"rollup_chain_id",
					FieldType::Integer {
						min: Some(1),
						max: None,
					},
				),
				Field::new(
					"host_chain_id",
					FieldType::Integer {
						min: Some(1),
						max: None,
					},
				),
				Field::new("order_origin_address", FieldType::String),
				Field::new("order_destination_address", FieldType::String),
				Field::new("filler_recipient", FieldType::String),
			],
			// Optional fields
			vec![Field::new(
				"target_block",
				FieldType::Integer {
					min: Some(1),
					max: None,
				},
			)],
		);

		schema.validate(config)
	}
}

#[async_trait]
impl DeliveryInterface for SignetBundleDelivery {
	fn config_schema(&self) -> Box<dyn ConfigSchema> {
		Box::new(SignetBundleDeliverySchema)
	}

	async fn submit(&self, tx: SolverTransaction) -> Result<TransactionHash, DeliveryError> {
		// Create bundles from transaction
		let bundles = self.create_bundles(&tx).await?;

		let mut last_bundle_id = String::new();
		let bundles_count = bundles.len();

		tracing::info!(
			bundles_count = bundles_count,
			"Created {} bundles targeting subsequent blocks. Submitting to cache.",
			bundles_count
		);

		// Submit all generated bundles to the cache sequentially
		for (i, bundle) in bundles.into_iter().enumerate() {
			let block_number = bundle.bundle.block_number;

			tracing::debug!(
				attempt = i + 1,
				block_number = block_number,
				txs_count = bundle.bundle.txs.len(),
				host_txns_count = bundle.host_txs.len(),
				"Submitting bundle to Signet cache"
			);

			// Submit bundle to cache
			let response = self
				.cache_client
				.forward_bundle(bundle)
				.await
				.map_err(|e| {
					let error_msg =
						format!("Failed to submit bundle for block {}: {}", block_number, e);
					tracing::error!(
						error = %e,
						"Bundle submission failed"
					);
					return DeliveryError::Network(error_msg);
				})?;

			last_bundle_id = response.id.to_string();
			tracing::info!(
				bundle_id = %last_bundle_id,
				block_number = block_number,
				"Bundle successfully submitted to cache"
			);
		}

		// Return the ID of the last submitted bundle
		let bundle_id_bytes = last_bundle_id.as_bytes().to_vec();
		Ok(TransactionHash(bundle_id_bytes))
	}

	async fn wait_for_confirmation(
		&self,
		_hash: &TransactionHash,
		_chain_id: u64,
		_confirmations: u64,
	) -> Result<TransactionReceipt, DeliveryError> {
		// TODO: Implement bundle status checking
		// For now, return error as this is not yet implemented
		Err(DeliveryError::Network(
			"Bundle confirmation tracking not yet implemented for Signet".to_string(),
		))
	}

	async fn get_receipt(
		&self,
		_hash: &TransactionHash,
		_chain_id: u64,
	) -> Result<TransactionReceipt, DeliveryError> {
		// TODO: Implement bundle receipt retrieval
		Err(DeliveryError::Network(
			"Bundle receipt retrieval not yet implemented for Signet".to_string(),
		))
	}

	async fn get_gas_price(&self, _chain_id: u64) -> Result<String, DeliveryError> {
		// Signet doesn't use traditional gas pricing
		Ok("0".to_string())
	}

	async fn get_balance(
		&self,
		_address: &str,
		_token: Option<&str>,
		_chain_id: u64,
	) -> Result<String, DeliveryError> {
		// TODO: Implement balance checking if needed
		Err(DeliveryError::Network(
			"Balance checking not yet implemented for Signet".to_string(),
		))
	}

	async fn get_allowance(
		&self,
		_owner: &str,
		_spender: &str,
		_token_address: &str,
		_chain_id: u64,
	) -> Result<String, DeliveryError> {
		// TODO: Implement allowance checking if needed
		Err(DeliveryError::Network(
			"Allowance checking not yet implemented for Signet".to_string(),
		))
	}

	async fn get_nonce(&self, _address: &str, _chain_id: u64) -> Result<u64, DeliveryError> {
		// Signet doesn't use traditional nonces for bundles
		Ok(0)
	}

	async fn get_block_number(&self, chain_id: u64) -> Result<u64, DeliveryError> {
		// Get RPC URL from network config
		let network_config = self.networks.get(&chain_id);

		if let Some(config) = network_config {
			if let Some(rpc_url) = config.rpc_urls.first().and_then(|rpc| rpc.http.as_ref()) {
				// Try to fetch block number from RPC
				if let Ok(url) = rpc_url.parse::<reqwest::Url>() {
					let provider = ProviderBuilder::new()
						.network::<alloy_network::AnyNetwork>()
						.connect_http(url);

					match provider.get_block_number().await {
						Ok(block_number) => {
							tracing::debug!(
								chain_id = chain_id,
								block_number = block_number,
								"Retrieved Signet block number from RPC"
							);
							return Ok(block_number);
						},
						Err(e) => {
							tracing::warn!(
								chain_id = chain_id,
								error = %e,
								"Failed to fetch Signet block number from RPC, using fallback"
							);
						},
					}
				}
			}
		}

		// Fall back to config target_block or default if RPC fails
		Ok(self.config.target_block.unwrap_or(DEFAULT_BLOCK_NUMBER))
	}

	async fn estimate_gas(&self, _tx: SolverTransaction) -> Result<u64, DeliveryError> {
		// Signet bundles don't use traditional gas estimation
		Ok(0)
	}

	async fn eth_call(&self, _tx: SolverTransaction) -> Result<Bytes, DeliveryError> {
		// TODO: Implement contract calls if needed
		Err(DeliveryError::Network(
			"Contract calls not yet implemented for Signet".to_string(),
		))
	}
}

/// Factory function to create a Signet bundle delivery from configuration.
pub fn create_delivery(
	config: &toml::Value,
	networks: &NetworksConfig,
	default_private_key: &SecretString,
	network_private_keys: &std::collections::HashMap<u64, SecretString>,
) -> Result<Box<dyn DeliveryInterface>, DeliveryError> {
	let delivery = SignetBundleDelivery::from_config(
		config,
		networks,
		default_private_key,
		network_private_keys,
	)?;
	Ok(Box::new(delivery))
}

/// Extracts optional DEX configuration from the delivery configuration.
fn parse_dex_config(config: &toml::Value) -> Result<Option<DexConfig>, DeliveryError> {
	let dex_value = match config.get("dex") {
		Some(value) => value,
		None => return Ok(None),
	};

	let pool_manager = dex_value
		.get("pool_manager")
		.and_then(|v| v.as_str())
		.ok_or_else(|| DeliveryError::Network("dex.pool_manager is required".to_string()))?
		.parse::<alloy_primitives::Address>()
		.map_err(|e| DeliveryError::Network(format!("Invalid dex.pool_manager: {}", e)))?;

	let swap_router = dex_value
		.get("swap_router")
		.and_then(|v| v.as_str())
		.ok_or_else(|| DeliveryError::Network("dex.swap_router is required".to_string()))?
		.parse::<alloy_primitives::Address>()
		.map_err(|e| DeliveryError::Network(format!("Invalid dex.swap_router: {}", e)))?;

	let lp_router = dex_value
		.get("lp_router")
		.and_then(|v| v.as_str())
		.ok_or_else(|| DeliveryError::Network("dex.lp_router is required".to_string()))?
		.parse::<alloy_primitives::Address>()
		.map_err(|e| DeliveryError::Network(format!("Invalid dex.lp_router: {}", e)))?;

	let max_routing_hops = dex_value
		.get("max_routing_hops")
		.and_then(|v| v.as_integer())
		.unwrap_or(2);

	let slippage_tolerance_bps = dex_value
		.get("slippage_tolerance_bps")
		.and_then(|v| v.as_integer())
		.unwrap_or(100);

	let default_fee_tier = dex_value
		.get("default_fee_tier")
		.and_then(|v| v.as_integer())
		.unwrap_or(0);

	let tick_spacing = dex_value
		.get("tick_spacing")
		.and_then(|v| v.as_integer())
		.unwrap_or(60);

	let mut tokens = HashMap::new();
	if let Some(table) = dex_value.as_table() {
		for (key, value) in table {
			if !key.starts_with("token_") {
				continue;
			}

			let addr_str = value.as_str().ok_or_else(|| {
				DeliveryError::Network(format!("dex.{} must be a string address", key))
			})?;

			let address = addr_str.parse::<alloy_primitives::Address>().map_err(|e| {
				DeliveryError::Network(format!("Invalid address for dex.{}: {}", key, e))
			})?;

			tokens.insert(key.clone(), address);
		}
	}

	let pools_value = dex_value
		.get("pools")
		.and_then(|v| v.as_array())
		.ok_or_else(|| DeliveryError::Network("dex.pools array is required".to_string()))?;

	let mut pools = Vec::new();
	for pool_entry in pools_value {
		let token0 = pool_entry
			.get("token0")
			.and_then(|v| v.as_str())
			.ok_or_else(|| DeliveryError::Network("dex.pools[].token0 is required".to_string()))?;
		let token1 = pool_entry
			.get("token1")
			.and_then(|v| v.as_str())
			.ok_or_else(|| DeliveryError::Network("dex.pools[].token1 is required".to_string()))?;
		let price_ratio = pool_entry
			.get("price_ratio")
			.and_then(|v| v.as_str())
			.ok_or_else(|| {
				DeliveryError::Network("dex.pools[].price_ratio is required".to_string())
			})?
			.parse::<f64>()
			.map_err(|e| DeliveryError::Network(format!("Invalid price_ratio: {}", e)))?;

		let token0_addr = token0
			.parse::<alloy_primitives::Address>()
			.map_err(|e| DeliveryError::Network(format!("Invalid token0 in pool: {}", e)))?;
		let token1_addr = token1
			.parse::<alloy_primitives::Address>()
			.map_err(|e| DeliveryError::Network(format!("Invalid token1 in pool: {}", e)))?;

		pools.push(DexPoolConfig {
			token0: token0_addr,
			token1: token1_addr,
			price_ratio,
		});
	}

	// Ensure tokens covers all addresses present in pools
	let mut referenced_tokens = HashSet::new();
	for pool in &pools {
		referenced_tokens.insert(pool.token0);
		referenced_tokens.insert(pool.token1);
	}
	for address in referenced_tokens {
		if !tokens.values().any(|existing| *existing == address) {
			let key = format!("token_{}", hex::encode(address.as_slice()));
			tokens.insert(key, address);
		}
	}

	Ok(Some(DexConfig {
		pool_manager,
		swap_router,
		lp_router,
		max_routing_hops: max_routing_hops as usize,
		slippage_tolerance_bps: slippage_tolerance_bps as u16,
		default_fee_tier: default_fee_tier as u32,
		tick_spacing: tick_spacing as i32,
		tokens,
		pools,
	}))
}

/// Registry for the Signet bundle delivery implementation.
pub struct Registry;

impl solver_types::ImplementationRegistry for Registry {
	const NAME: &'static str = "signet_bundle";
	type Factory = crate::DeliveryFactory;

	fn factory() -> Self::Factory {
		create_delivery
	}
}

impl crate::DeliveryRegistry for Registry {}

#[cfg(test)]
mod tests {
	use super::*;
	use solver_types::utils::tests::builders::NetworksConfigBuilder;
	use std::collections::HashMap;

	fn create_test_networks() -> NetworksConfig {
		NetworksConfigBuilder::new().build()
	}

	fn create_test_config() -> HashMap<&'static str, toml::Value> {
		HashMap::from([
			("chain_name", toml::Value::String("pecorino".to_string())),
			("rollup_chain_id", toml::Value::Integer(901)),
			("host_chain_id", toml::Value::Integer(1)),
			(
				"order_origin_address",
				toml::Value::String("0x0000000000000000000000000000000000000001".to_string()),
			),
			(
				"order_destination_address",
				toml::Value::String("0x0000000000000000000000000000000000000002".to_string()),
			),
			(
				"filler_recipient",
				toml::Value::String("0x0000000000000000000000000000000000000003".to_string()),
			),
		])
	}

	#[test]
	fn test_config_schema_validation_valid() {
		let config = toml::Value::try_from(create_test_config()).unwrap();

		let result = SignetBundleDeliverySchema::validate_config(&config);
		assert!(result.is_ok());
	}

	#[test]
	fn test_config_schema_validation_with_target_block() {
		let mut config = create_test_config();
		config.insert("target_block", toml::Value::Integer(100));
		let config = toml::Value::try_from(config).unwrap();

		let result = SignetBundleDeliverySchema::validate_config(&config);
		assert!(result.is_ok());
	}

	#[test]
	fn test_config_schema_validation_missing_chain_name() {
		let mut config = create_test_config();
		config.remove("chain_name");
		let config = toml::Value::try_from(config).unwrap();

		let result = SignetBundleDeliverySchema::validate_config(&config);
		assert!(result.is_err());
	}

	#[test]
	fn test_config_schema_validation_missing_required_field() {
		let mut config = create_test_config();
		config.remove("order_origin_address");
		let config = toml::Value::try_from(config).unwrap();

		let result = SignetBundleDeliverySchema::validate_config(&config);
		assert!(result.is_err());
	}

	#[test]
	fn test_create_delivery_valid() {
		let config = toml::Value::try_from(create_test_config()).unwrap();

		let networks = create_test_networks();
		// Use a valid hex private key for testing
		let default_key = solver_types::SecretString::from(
			"0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
		);
		let network_keys = HashMap::new();

		let result = create_delivery(&config, &networks, &default_key, &network_keys);
		assert!(result.is_ok());
	}

	#[test]
	fn test_registry_name() {
		assert_eq!(
			<Registry as solver_types::ImplementationRegistry>::NAME,
			"signet_bundle"
		);
	}
}
