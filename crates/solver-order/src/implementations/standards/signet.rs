//! Signet order implementation for bundle-based settlement.
//!
//! This module provides a minimal order interface for Signet orders, which use
//! Permit2-based SignedOrders instead of EIP-7683 StandardOrders. The actual order
//! validation and execution is handled by the Signet bundle delivery mechanism.

use crate::{OrderError, OrderInterface};
use alloy_primitives::Bytes;
use async_trait::async_trait;
use solver_types::{
	current_timestamp, order::ChainSettlerInfo, oracle::OracleRoutes,
	standards::eip7683::interfaces::StandardOrder, Address, ConfigSchema, ExecutionParams,
	FillProof, NetworksConfig, Order, OrderIdCallback, OrderStatus, Transaction,
};

/// Signet order implementation.
///
/// This implementation provides a pass-through for Signet orders, which are handled
/// entirely by the Signet bundle delivery mechanism. It does not perform StandardOrder
/// validation since Signet uses a different order format (SignedOrder with Permit2Batch).
#[derive(Debug)]
pub struct SignetOrderImpl {
	/// Networks configuration
	networks: NetworksConfig,
	/// Oracle routes (unused for Signet but required by interface)
	_oracle_routes: OracleRoutes,
}

impl SignetOrderImpl {
	/// Creates a new Signet order implementation.
	pub fn new(
		networks: NetworksConfig,
		oracle_routes: OracleRoutes,
	) -> Result<Self, OrderError> {
		Ok(Self {
			networks,
			_oracle_routes: oracle_routes,
		})
	}
}

#[async_trait]
impl OrderInterface for SignetOrderImpl {
	fn config_schema(&self) -> Box<dyn ConfigSchema> {
		Box::new(SignetOrderSchema)
	}

	async fn generate_prepare_transaction(
		&self,
		_source: &str,
		_order: &Order,
		_params: &ExecutionParams,
	) -> Result<Option<Transaction>, OrderError> {
		// Signet orders don't need preparation - they're submitted directly to the bundle
		Ok(None)
	}

	async fn generate_fill_transaction(
		&self,
		_order: &Order,
		_params: &ExecutionParams,
	) -> Result<Transaction, OrderError> {
		// Fill transactions are handled by Signet bundle delivery, not through this interface
		Err(OrderError::InvalidOrder(
			"Signet orders are filled via bundle delivery".to_string(),
		))
	}

	async fn generate_claim_transaction(
		&self,
		_order: &Order,
		_fill_proof: &FillProof,
	) -> Result<Transaction, OrderError> {
		// Claim transactions are handled by Signet bundle delivery, not through this interface
		Err(OrderError::InvalidOrder(
			"Signet orders are claimed via bundle delivery".to_string(),
		))
	}

	async fn validate_order(&self, _order_bytes: &Bytes) -> Result<StandardOrder, OrderError> {
		// Signet doesn't use StandardOrder, so this is not supported
		Err(OrderError::ValidationFailed(
			"Signet orders don't use StandardOrder format".to_string(),
		))
	}

	async fn validate_and_create_order(
		&self,
		_order_bytes: &Bytes,
		intent_data: &Option<serde_json::Value>,
		_lock_type: &str,
		_order_id_callback: OrderIdCallback,
		solver_address: &Address,
	) -> Result<Order, OrderError> {
		// Extract SignedOrder from intent_data
		let signed_order: signet_types::SignedOrder = intent_data
			.as_ref()
			.ok_or_else(|| {
				OrderError::ValidationFailed("Missing SignedOrder in intent data".to_string())
			})
			.and_then(|data| {
				serde_json::from_value(data.clone()).map_err(|e| {
					OrderError::ValidationFailed(format!("Failed to deserialize SignedOrder: {}", e))
				})
			})?;

		// Compute order ID using the order hash
		let order_hash = signed_order.order_hash();
		let order_id = hex::encode(order_hash.as_slice());

		// For Signet orders:
		// - Inputs are ALWAYS on the rollup chain (where permit2 locks the tokens)
		// - Outputs can be on rollup OR host chain (cross-chain transfers)

		// Rollup chain ID is fixed (Pecorino = 14174)
		// TODO: Make this configurable if supporting multiple Signet chains
		let rollup_chain_id = 14174u64;

		// Get rollup network config for inputs
		let rollup_network = self
			.networks
			.get(&rollup_chain_id)
			.ok_or_else(|| {
				OrderError::InvalidOrder(format!("No network config for rollup chain {}", rollup_chain_id))
			})?;

		// Get destination chain from first output
		let dest_chain_id = signed_order
			.outputs()
			.first()
			.map(|o| o.chainId as u64)
			.unwrap_or(rollup_chain_id);

		let dest_network = self.networks.get(&dest_chain_id).ok_or_else(|| {
			OrderError::InvalidOrder(format!("No network config for destination chain {}", dest_chain_id))
		})?;

		// Inputs are always on the rollup chain (permit2)
		let input_chains = vec![ChainSettlerInfo {
			chain_id: rollup_chain_id,
			settler_address: rollup_network.input_settler_address.clone(),
		}];

		// Outputs are on the destination chain (from SignedOrder)
		let output_chains = vec![ChainSettlerInfo {
			chain_id: dest_chain_id,
			settler_address: dest_network.output_settler_address.clone(),
		}];

		// Create order with all SignedOrder data stored in the data field
		Ok(Order {
			id: order_id,
			standard: "signet".to_string(),
			created_at: current_timestamp(),
			updated_at: current_timestamp(),
			status: OrderStatus::Created,
			data: intent_data.clone().unwrap_or_default(),
			solver_address: solver_address.clone(),
			quote_id: None,
			input_chains,
			output_chains,
			execution_params: None,
			prepare_tx_hash: None,
			fill_tx_hash: None,
			post_fill_tx_hash: None,
			pre_claim_tx_hash: None,
			claim_tx_hash: None,
			fill_proof: None,
		})
	}
}

/// Configuration schema for Signet order implementation.
pub struct SignetOrderSchema;

impl ConfigSchema for SignetOrderSchema {
	fn validate(&self, _config: &toml::Value) -> Result<(), solver_types::ValidationError> {
		// Signet orders have no specific configuration requirements
		Ok(())
	}
}

/// Factory function to create a Signet order implementation.
pub fn create_order(
	_config: &toml::Value,
	networks: &NetworksConfig,
	oracle_routes: &OracleRoutes,
) -> Result<Box<dyn OrderInterface>, OrderError> {
	let order_impl = SignetOrderImpl::new(networks.clone(), oracle_routes.clone())?;
	Ok(Box::new(order_impl))
}

/// Registry for the Signet order implementation.
pub struct Registry;

impl solver_types::ImplementationRegistry for Registry {
	const NAME: &'static str = "signet";
	type Factory = crate::OrderFactory;

	fn factory() -> Self::Factory {
		create_order
	}
}

impl crate::OrderRegistry for Registry {}
