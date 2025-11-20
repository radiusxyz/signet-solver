//! Signet Cache Intent Discovery Implementation
//!
//! This module implements intent discovery from Signet's transaction cache.
//! It polls the cache API for signed Permit2 orders and converts them into
//! the internal Intent format used by the solver.
//!
//! ## Overview
//!
//! The Signet cache discovery service:
//! - Polls the Signet transaction cache at regular intervals
//! - Retrieves SignedOrder(s) from the cache
//! - Filters orders based on whitelist (if configured)
//! - Converts Permit2 orders to the internal Intent format
//! - Broadcasts discovered intents to the solver system
//!
//! ## Configuration
//!
//! The service requires the following configuration:
//! - `chain_name` - Signet chain name (e.g., "pecorino")
//! - `polling_interval_secs` - Polling interval in seconds (default: 5)
//! - `whitelist_addresses` - Optional list of user addresses to filter (default: None)

use crate::{DiscoveryError, DiscoveryInterface};
use async_trait::async_trait;
use signet_tx_cache::client::TxCache;
use signet_types::SignedOrder;
use solver_types::{
	current_timestamp, ConfigSchema, Field, FieldType, Intent, IntentMetadata, NetworksConfig,
	Schema,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::task::JoinHandle;

const DEFAULT_POLLING_INTERVAL_SECS: u64 = 5;
const MAX_POLLING_INTERVAL_SECS: u64 = 300;

/// Signet cache discovery implementation configuration.
#[derive(Debug, Clone)]
pub struct SignetCacheConfig {
	/// Signet chain name (e.g., "pecorino")
	pub chain_name: String,
	/// Polling interval in seconds
	pub polling_interval_secs: u64,
	/// Optional whitelist of user addresses to filter
	pub whitelist_addresses: Option<Vec<String>>,
}

/// Signet cache discovery implementation.
///
/// This implementation polls the Signet transaction cache for new Permit2 orders
/// and converts them into intents for the solver to process.
pub struct SignetCacheDiscovery {
	/// Discovery configuration
	config: SignetCacheConfig,
	/// Flag indicating if monitoring is active
	is_monitoring: Arc<AtomicBool>,
	/// Handle for the monitoring task
	monitoring_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
	/// Channel for signaling monitoring shutdown
	stop_signal: Arc<Mutex<Option<broadcast::Sender<()>>>>,
}

impl SignetCacheDiscovery {
	/// Creates a new Signet cache discovery instance.
	pub fn new(
		config: SignetCacheConfig,
		_networks: NetworksConfig,
	) -> Result<Self, DiscoveryError> {
		// Validate chain name
		if config.chain_name.is_empty() {
			return Err(DiscoveryError::ValidationError(
				"chain_name cannot be empty".to_string(),
			));
		}

		// Validate polling interval
		if config.polling_interval_secs == 0
			|| config.polling_interval_secs > MAX_POLLING_INTERVAL_SECS
		{
			return Err(DiscoveryError::ValidationError(format!(
				"polling_interval_secs must be between 1 and {}",
				MAX_POLLING_INTERVAL_SECS
			)));
		}

		Ok(Self {
			config,
			is_monitoring: Arc::new(AtomicBool::new(false)),
			monitoring_handle: Arc::new(Mutex::new(None)),
			stop_signal: Arc::new(Mutex::new(None)),
		})
	}

	/// Converts a Signed Order to an Intent.
	fn order_to_intent(order: &SignedOrder) -> Result<Intent, DiscoveryError> {
		// Generate a simple ID from permit nonce
		let id = format!("signet-{}", order.permit().permit.nonce);

		// Create intent with Permit2 data (SignedOrder)
		let data = serde_json::to_value(order)
			.map_err(|e| DiscoveryError::ParseError(format!("Failed to serialize order: {}", e)))?;

		// Store SignedOrder JSON as order_bytes
		// The Signet order implementation will extract it from intent_data
		let order_bytes = serde_json::to_vec(order)
			.map_err(|e| DiscoveryError::ParseError(format!("Failed to encode order: {}", e)))?;

		Ok(Intent {
			id,
			source: "signet-cache".to_string(),
			standard: "signet".to_string(), // Use Signet standard (not eip7683)
			metadata: IntentMetadata {
				requires_auction: false,
				exclusive_until: None,
				discovered_at: current_timestamp(),
			},
			data,
			order_bytes: order_bytes.into(),
			quote_id: None,
			lock_type: "permit2_escrow".to_string(), // Use proper LockType format
		})
	}

	/// Checks if an order matches the whitelist.
	fn matches_whitelist(order: &SignedOrder, whitelist: &Option<Vec<String>>) -> bool {
		match whitelist {
			None => true, // No whitelist = accept all
			Some(addresses) => {
				// Get the owner address from the order
				let owner = order.permit().owner;

				// Check if the owner is in the whitelist (case-insensitive comparison)
				addresses.iter().any(|addr| {
					// Parse whitelist address and compare
					if let Ok(whitelist_addr) = addr.parse::<alloy_primitives::Address>() {
						whitelist_addr == owner
					} else {
						tracing::warn!("Invalid address in whitelist: {}", addr);
						false
					}
				})
			},
		}
	}

	/// Polling loop that fetches and processes orders.
	async fn polling_loop(
		config: SignetCacheConfig,
		sender: mpsc::UnboundedSender<Intent>,
		mut stop_rx: broadcast::Receiver<()>,
	) {
		// Build cache client based on chain name
		let client = if config.chain_name == "pecorino" {
			TxCache::pecorino()
		} else {
			// Construct URL for other chains
			let url = format!("https://cache.{}.signet.sh", config.chain_name);
			match TxCache::new_from_string(&url) {
				Ok(c) => c,
				Err(e) => {
					tracing::error!("Failed to create cache client: {}", e);
					return;
				},
			}
		};

		let mut interval =
			tokio::time::interval(std::time::Duration::from_secs(config.polling_interval_secs));
		interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

		// Track processed order IDs to avoid re-sending same orders
		// Signet cache may return already-processed orders until they're fulfilled on-chain
		let mut processed_orders = std::collections::HashSet::new();

		loop {
			tokio::select! {
				_ = interval.tick() => {
					match client.get_orders(None).await {
						Ok(orders_response) => {
							let orders: Vec<SignedOrder> = orders_response.inner().orders.clone();
							tracing::debug!("Fetched {} orders from Signet cache", orders.len());

							for order in orders {
								// Generate intent ID to check if already processed
								let intent_id = format!("signet-{}", order.permit().permit.nonce);

								// Skip if already processed
								if processed_orders.contains(&intent_id) {
									continue;
								}

								// Apply whitelist filter
								if !Self::matches_whitelist(&order, &config.whitelist_addresses) {
									continue;
								}

								// Convert to intent
								match Self::order_to_intent(&order) {
									Ok(intent) => {
										if let Err(e) = sender.send(intent) {
											tracing::error!("Failed to send intent: {}", e);
										} else {
											// Mark as processed to avoid re-sending
											processed_orders.insert(intent_id);
											tracing::debug!("Marked order as processed: {}", processed_orders.len());
										}
									},
									Err(e) => {
										tracing::warn!("Failed to convert order to intent: {}", e);
									},
								}
							}
						},
						Err(e) => {
							tracing::error!("Failed to fetch orders from Signet cache: {}", e);
						},
					}
				}
				_ = stop_rx.recv() => {
					tracing::info!("Stopping Signet cache polling");
					break;
				}
			}
		}
	}
}

/// Configuration schema for Signet cache discovery.
pub struct SignetCacheDiscoverySchema;

impl SignetCacheDiscoverySchema {
	/// Static validation method for use before instance creation
	pub fn validate_config(config: &toml::Value) -> Result<(), solver_types::ValidationError> {
		let instance = Self;
		instance.validate(config)
	}
}

impl ConfigSchema for SignetCacheDiscoverySchema {
	fn validate(&self, config: &toml::Value) -> Result<(), solver_types::ValidationError> {
		let schema = Schema::new(
			// Required fields
			vec![Field::new("chain_name", FieldType::String)],
			// Optional fields
			vec![
				Field::new(
					"polling_interval_secs",
					FieldType::Integer {
						min: Some(1),
						max: Some(MAX_POLLING_INTERVAL_SECS as i64),
					},
				),
				Field::new(
					"whitelist_addresses",
					FieldType::Array(Box::new(FieldType::String)),
				),
			],
		);

		schema.validate(config)
	}
}

#[async_trait]
impl DiscoveryInterface for SignetCacheDiscovery {
	fn config_schema(&self) -> Box<dyn ConfigSchema> {
		Box::new(SignetCacheDiscoverySchema)
	}

	async fn start_monitoring(
		&self,
		sender: mpsc::UnboundedSender<Intent>,
	) -> Result<(), DiscoveryError> {
		if self.is_monitoring.load(Ordering::SeqCst) {
			return Err(DiscoveryError::AlreadyMonitoring);
		}

		// Create broadcast channel for shutdown
		let (stop_tx, stop_rx) = broadcast::channel(1);
		*self.stop_signal.lock().await = Some(stop_tx);

		// Spawn polling task
		let config = self.config.clone();
		let handle = tokio::spawn(async move {
			Self::polling_loop(config, sender, stop_rx).await;
		});

		*self.monitoring_handle.lock().await = Some(handle);
		self.is_monitoring.store(true, Ordering::SeqCst);

		tracing::info!(
			chain_name = %self.config.chain_name,
			polling_interval = self.config.polling_interval_secs,
			whitelist_enabled = self.config.whitelist_addresses.is_some(),
			"Signet cache discovery monitoring started"
		);

		Ok(())
	}

	async fn stop_monitoring(&self) -> Result<(), DiscoveryError> {
		if !self.is_monitoring.load(Ordering::SeqCst) {
			return Ok(());
		}

		// Send shutdown signal if exists
		if let Some(stop_tx) = self.stop_signal.lock().await.take() {
			let _ = stop_tx.send(());
		}

		// Wait for monitoring task to complete
		if let Some(handle) = self.monitoring_handle.lock().await.take() {
			let _ = handle.await;
		}

		self.is_monitoring.store(false, Ordering::SeqCst);
		tracing::info!("Stopped Signet cache discovery monitoring");
		Ok(())
	}
}

/// Factory function to create a Signet cache discovery from configuration.
pub fn create_discovery(
	config: &toml::Value,
	networks: &NetworksConfig,
) -> Result<Box<dyn DiscoveryInterface>, DiscoveryError> {
	// Validate configuration first
	SignetCacheDiscoverySchema::validate_config(config)
		.map_err(|e| DiscoveryError::ValidationError(format!("Invalid configuration: {}", e)))?;

	// Parse chain_name (required)
	let chain_name = config
		.get("chain_name")
		.and_then(|v| v.as_str())
		.ok_or_else(|| DiscoveryError::ValidationError("chain_name is required".to_string()))?
		.to_string();

	// Parse polling_interval_secs (optional, default to 5)
	let polling_interval_secs = config
		.get("polling_interval_secs")
		.and_then(|v| v.as_integer())
		.map(|v| v as u64)
		.unwrap_or(DEFAULT_POLLING_INTERVAL_SECS);

	// Parse whitelist_addresses (optional)
	let whitelist_addresses = config
		.get("whitelist_addresses")
		.and_then(|v| v.as_array())
		.map(|arr| {
			arr.iter()
				.filter_map(|v| v.as_str().map(|s| s.to_string()))
				.collect::<Vec<_>>()
		});

	let discovery_config = SignetCacheConfig {
		chain_name,
		polling_interval_secs,
		whitelist_addresses,
	};

	let discovery = SignetCacheDiscovery::new(discovery_config, networks.clone())?;
	Ok(Box::new(discovery))
}

/// Registry for the Signet cache discovery implementation.
pub struct Registry;

impl solver_types::ImplementationRegistry for Registry {
	const NAME: &'static str = "signet_cache";
	type Factory = crate::DiscoveryFactory;

	fn factory() -> Self::Factory {
		create_discovery
	}
}

impl crate::DiscoveryRegistry for Registry {}

#[cfg(test)]
mod tests {
	use super::*;
	use solver_types::utils::tests::builders::NetworksConfigBuilder;
	use std::collections::HashMap;

	fn create_test_networks() -> NetworksConfig {
		NetworksConfigBuilder::new().build()
	}

	#[test]
	fn test_config_schema_validation_valid() {
		let config = toml::Value::try_from(HashMap::from([
			("chain_name", toml::Value::String("pecorino".to_string())),
			("polling_interval_secs", toml::Value::Integer(5)),
		]))
		.unwrap();

		let result = SignetCacheDiscoverySchema::validate_config(&config);
		assert!(result.is_ok());
	}

	#[test]
	fn test_config_schema_validation_with_whitelist() {
		let config = toml::Value::try_from(HashMap::from([
			("chain_name", toml::Value::String("pecorino".to_string())),
			("polling_interval_secs", toml::Value::Integer(5)),
			(
				"whitelist_addresses",
				toml::Value::Array(vec![toml::Value::String(
					"0x21c10426fa5101ab80042ac6cf89f65a7d9e7bcb".to_string(),
				)]),
			),
		]))
		.unwrap();

		let result = SignetCacheDiscoverySchema::validate_config(&config);
		assert!(result.is_ok());
	}

	#[test]
	fn test_config_schema_validation_missing_chain_name() {
		let config = toml::Value::try_from(HashMap::from([(
			"polling_interval_secs",
			toml::Value::Integer(5),
		)]))
		.unwrap();

		let result = SignetCacheDiscoverySchema::validate_config(&config);
		assert!(result.is_err());
	}

	#[test]
	fn test_create_discovery_valid() {
		let config = toml::Value::try_from(HashMap::from([
			("chain_name", toml::Value::String("pecorino".to_string())),
			("polling_interval_secs", toml::Value::Integer(5)),
		]))
		.unwrap();

		let networks = create_test_networks();
		let result = create_discovery(&config, &networks);
		assert!(result.is_ok());
	}

	#[test]
	fn test_registry_name() {
		assert_eq!(
			<Registry as solver_types::ImplementationRegistry>::NAME,
			"signet_cache"
		);
	}

	// Helper function to create a test SignedOrder
	fn create_test_order(owner: alloy_primitives::Address, nonce: u64) -> SignedOrder {
		use alloy_primitives::{Signature, U256};
		use signet_zenith::HostOrders::{PermitBatchTransferFrom, TokenPermissions};
		use signet_zenith::RollupOrders::{Output, Permit2Batch};

		SignedOrder::new(
			Permit2Batch {
				permit: PermitBatchTransferFrom {
					permitted: vec![TokenPermissions {
						token: alloy_primitives::Address::ZERO,
						amount: U256::ZERO,
					}],
					nonce: U256::from(nonce),
					deadline: U256::from(1000000000u64),
				},
				owner,
				signature: Signature::test_signature().as_bytes().into(),
			},
			vec![Output {
				token: alloy_primitives::Address::ZERO,
				amount: U256::ZERO,
				recipient: alloy_primitives::Address::ZERO,
				chainId: 0,
			}],
		)
	}

	#[test]
	fn test_order_to_intent_conversion() {
		let test_address = "0x21c10426fa5101ab80042ac6cf89f65a7d9e7bcb"
			.parse::<alloy_primitives::Address>()
			.unwrap();
		let order = create_test_order(test_address, 12345);

		let intent = SignetCacheDiscovery::order_to_intent(&order).unwrap();

		// Verify basic fields
		assert_eq!(intent.id, "signet-12345");
		assert_eq!(intent.source, "signet-cache");
		assert_eq!(intent.standard, "eip7683");
		assert_eq!(intent.lock_type, "permit2");
		assert!(!intent.metadata.requires_auction);
		assert!(intent.metadata.exclusive_until.is_none());
		assert!(intent.quote_id.is_none());

		// Verify data can be deserialized back to SignedOrder
		let deserialized: SignedOrder = serde_json::from_value(intent.data).unwrap();
		assert_eq!(deserialized.permit.owner, test_address);
		assert_eq!(
			deserialized.permit.permit.nonce,
			alloy_primitives::U256::from(12345)
		);
	}

	#[test]
	fn test_matches_whitelist_no_whitelist() {
		let test_address = "0x21c10426fa5101ab80042ac6cf89f65a7d9e7bcb"
			.parse::<alloy_primitives::Address>()
			.unwrap();
		let order = create_test_order(test_address, 1);

		// No whitelist should accept all orders
		assert!(SignetCacheDiscovery::matches_whitelist(&order, &None));
	}

	#[test]
	fn test_matches_whitelist_matching_address() {
		let test_address = "0x21c10426fa5101ab80042ac6cf89f65a7d9e7bcb"
			.parse::<alloy_primitives::Address>()
			.unwrap();
		let order = create_test_order(test_address, 1);

		let whitelist = Some(vec![
			"0x21c10426fa5101ab80042ac6cf89f65a7d9e7bcb".to_string(),
			"0x1234567890123456789012345678901234567890".to_string(),
		]);

		assert!(SignetCacheDiscovery::matches_whitelist(&order, &whitelist));
	}

	#[test]
	fn test_matches_whitelist_case_insensitive() {
		let test_address = "0x21c10426fa5101ab80042ac6cf89f65a7d9e7bcb"
			.parse::<alloy_primitives::Address>()
			.unwrap();
		let order = create_test_order(test_address, 1);

		// Test with uppercase address in whitelist
		let whitelist = Some(vec![
			"0x21C10426FA5101AB80042AC6CF89F65A7D9E7BCB".to_string()
		]);

		assert!(SignetCacheDiscovery::matches_whitelist(&order, &whitelist));
	}

	#[test]
	fn test_matches_whitelist_non_matching_address() {
		let test_address = "0x21c10426fa5101ab80042ac6cf89f65a7d9e7bcb"
			.parse::<alloy_primitives::Address>()
			.unwrap();
		let order = create_test_order(test_address, 1);

		let whitelist = Some(vec![
			"0x1234567890123456789012345678901234567890".to_string(),
			"0xabcdefabcdefabcdefabcdefabcdefabcdefabcd".to_string(),
		]);

		assert!(!SignetCacheDiscovery::matches_whitelist(&order, &whitelist));
	}

	#[test]
	fn test_matches_whitelist_invalid_address() {
		let test_address = "0x21c10426fa5101ab80042ac6cf89f65a7d9e7bcb"
			.parse::<alloy_primitives::Address>()
			.unwrap();
		let order = create_test_order(test_address, 1);

		// Whitelist with invalid address should not match
		let whitelist = Some(vec!["invalid_address".to_string()]);

		assert!(!SignetCacheDiscovery::matches_whitelist(&order, &whitelist));
	}

	#[test]
	fn test_matches_whitelist_mixed_valid_invalid() {
		let test_address = "0x21c10426fa5101ab80042ac6cf89f65a7d9e7bcb"
			.parse::<alloy_primitives::Address>()
			.unwrap();
		let order = create_test_order(test_address, 1);

		// Whitelist with one valid matching address and one invalid
		let whitelist = Some(vec![
			"invalid_address".to_string(),
			"0x21c10426fa5101ab80042ac6cf89f65a7d9e7bcb".to_string(),
		]);

		assert!(SignetCacheDiscovery::matches_whitelist(&order, &whitelist));
	}

	#[test]
	fn test_config_validation_invalid_polling_interval_zero() {
		let config = toml::Value::try_from(HashMap::from([
			("chain_name", toml::Value::String("pecorino".to_string())),
			("polling_interval_secs", toml::Value::Integer(0)),
		]))
		.unwrap();

		let result = SignetCacheDiscoverySchema::validate_config(&config);
		assert!(result.is_err());
	}

	#[test]
	fn test_config_validation_invalid_polling_interval_too_large() {
		let config = toml::Value::try_from(HashMap::from([
			("chain_name", toml::Value::String("pecorino".to_string())),
			("polling_interval_secs", toml::Value::Integer(301)),
		]))
		.unwrap();

		let result = SignetCacheDiscoverySchema::validate_config(&config);
		assert!(result.is_err());
	}

	#[test]
	fn test_new_discovery_validation_empty_chain_name() {
		let config = SignetCacheConfig {
			chain_name: "".to_string(),
			polling_interval_secs: 5,
			whitelist_addresses: None,
		};

		let networks = create_test_networks();
		let result = SignetCacheDiscovery::new(config, networks);
		assert!(result.is_err());
	}

	#[test]
	fn test_new_discovery_validation_polling_interval_bounds() {
		let networks = create_test_networks();

		// Test zero interval
		let config_zero = SignetCacheConfig {
			chain_name: "pecorino".to_string(),
			polling_interval_secs: 0,
			whitelist_addresses: None,
		};
		assert!(SignetCacheDiscovery::new(config_zero, networks.clone()).is_err());

		// Test too large interval
		let config_large = SignetCacheConfig {
			chain_name: "pecorino".to_string(),
			polling_interval_secs: MAX_POLLING_INTERVAL_SECS + 1,
			whitelist_addresses: None,
		};
		assert!(SignetCacheDiscovery::new(config_large, networks).is_err());
	}

	#[test]
	fn test_create_discovery_with_default_polling_interval() {
		let config = toml::Value::try_from(HashMap::from([(
			"chain_name",
			toml::Value::String("pecorino".to_string()),
		)]))
		.unwrap();

		let networks = create_test_networks();
		let result = create_discovery(&config, &networks);
		assert!(result.is_ok());
	}
}
