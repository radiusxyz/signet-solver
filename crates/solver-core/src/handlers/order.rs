//! Order handler for processing order preparation and execution.
//!
//! Manages the generation and submission of prepare transactions (for off-chain orders)
//! and fill transactions, updating order state and publishing appropriate events.

use crate::engine::event_bus::EventBus;
use crate::state::OrderStateMachine;
use alloy_primitives::hex;
use solver_delivery::DeliveryService;
use serde_json::json;
use solver_order::OrderService;
use solver_storage::StorageService;
use solver_types::{
	truncate_id, DeliveryEvent, ExecutionParams, Order, OrderEvent, OrderStatus, SolverEvent,
	StorageKey, TransactionType,
};
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, instrument};

/// Errors that can occur during order processing.
///
/// These errors represent failures in service operations,
/// storage operations, or state transitions during order handling.
#[derive(Debug, Error)]
pub enum OrderError {
	#[error("Service error: {0}")]
	Service(String),
	#[error("Storage error: {0}")]
	Storage(String),
	#[error("State error: {0}")]
	State(String),
}

/// Handler for processing order preparation and execution.
///
/// The OrderHandler manages the generation and submission of prepare
/// transactions for off-chain orders and fill transactions for all orders,
/// while updating order state and publishing relevant events.
pub struct OrderHandler {
	order_service: Arc<OrderService>,
	delivery: Arc<DeliveryService>,
	storage: Arc<StorageService>,
	state_machine: Arc<OrderStateMachine>,
	event_bus: EventBus,
}

impl OrderHandler {
	pub fn new(
		order_service: Arc<OrderService>,
		delivery: Arc<DeliveryService>,
		storage: Arc<StorageService>,
		state_machine: Arc<OrderStateMachine>,
		event_bus: EventBus,
	) -> Self {
		Self {
			order_service,
			delivery,
			storage,
			state_machine,
			event_bus,
		}
	}

	/// Handles order preparation for off-chain orders.
	#[instrument(skip_all, fields(order_id = %truncate_id(&order.id)))]
	pub async fn handle_preparation(
		&self,
		source: String,
		order: Order,
		params: ExecutionParams,
	) -> Result<(), OrderError> {
		// Generate prepare transaction
		if let Some(prepare_tx) = self
			.order_service
			.generate_prepare_transaction(&source, &order, &params)
			.await
			.map_err(|e| OrderError::Service(e.to_string()))?
		{
			// Submit prepare transaction
			let prepare_tx_hash = self
				.delivery
				.deliver(prepare_tx.clone())
				.await
				.map_err(|e| OrderError::Service(e.to_string()))?;

			self.event_bus
				.publish(SolverEvent::Delivery(DeliveryEvent::TransactionPending {
					order_id: order.id.clone(),
					tx_hash: prepare_tx_hash.clone(),
					tx_type: TransactionType::Prepare,
					tx_chain_id: prepare_tx.chain_id,
				}))
				.ok();

			// Store tx_hash -> order_id mapping
			self.storage
				.store(
					StorageKey::OrderByTxHash.as_str(),
					&hex::encode(&prepare_tx_hash.0),
					&order.id,
					None,
				)
				.await
				.map_err(|e| OrderError::Storage(e.to_string()))?;

			// Update order with execution params and prepare tx hash
			self.state_machine
				.update_order_with(&order.id, |o| {
					o.execution_params = Some(params.clone());
					o.status = OrderStatus::Pending;
					o.prepare_tx_hash = Some(prepare_tx_hash);
				})
				.await
				.map_err(|e| OrderError::State(e.to_string()))?;
		} else {
			// No preparation needed (on-chain intent), go directly to Executing
			self.state_machine
				.update_order_with(&order.id, |o| {
					o.execution_params = Some(params.clone());
					o.status = OrderStatus::Executing;
				})
				.await
				.map_err(|e| OrderError::State(e.to_string()))?;

			self.event_bus
				.publish(SolverEvent::Order(OrderEvent::Executing {
					order: order.clone(),
					params,
				}))
				.ok();
		}

		Ok(())
	}

	/// Handles order execution by generating and submitting a fill transaction.
	#[instrument(skip_all, fields(order_id = %truncate_id(&order.id)))]
	pub async fn handle_execution(
		&self,
		order: Order,
		params: ExecutionParams,
	) -> Result<(), OrderError> {
		// Signet orders use bundle delivery instead of traditional fill transactions
		if order.standard == "signet" {
			// For Signet, we create a bundle transaction with the SignedOrder data
			// The bundle delivery service will handle creating SignedFill and submitting to cache
			debug!(
				max_fee_per_gas = ?params.gas_price,
				max_priority_fee_per_gas = ?params.priority_fee,
				"Handling Signet order execution via bundle delivery"
			);

		let tx = solver_types::Transaction {
			chain_id: order
				.input_chains
				.first()
					.map(|c| c.chain_id)
					.unwrap_or(14174),
				to: order
					.input_chains
					.first()
					.map(|c| Some(c.settler_address.clone()))
					.unwrap_or_else(|| {
						// Fallback to zero address if no input chains
						Some(solver_types::Address(vec![0u8; 20]))
					}),
				data: vec![], // Empty data - bundle delivery constructs the actual payload
				value: alloy_primitives::U256::ZERO,
				gas_limit: None,
				gas_price: None, // Use EIP-1559 fees instead
				max_fee_per_gas: params.gas_price.to::<u128>().try_into().ok(),
				max_priority_fee_per_gas: params
					.priority_fee
					.and_then(|fee| fee.to::<u128>().try_into().ok()),
				nonce: None,
			metadata: Some(json!({
				"signed_order": order.data.clone(),
			})), // Pass SignedOrder data to bundle delivery
		};

			// Submit via bundle delivery (which will create SignedFill and submit bundle)
			let tx_hash = self
				.delivery
				.deliver(tx.clone())
				.await
				.map_err(|e| OrderError::Service(e.to_string()))?;

			self.event_bus
				.publish(SolverEvent::Delivery(DeliveryEvent::TransactionPending {
					order_id: order.id.clone(),
					tx_hash: tx_hash.clone(),
					tx_type: TransactionType::Fill,
					tx_chain_id: tx.chain_id,
				}))
				.ok();

			// Store fill transaction
			self.state_machine
				.set_transaction_hash(&order.id, tx_hash.clone(), TransactionType::Fill)
				.await
				.map_err(|e| OrderError::State(e.to_string()))?;

			// Store reverse mapping: tx_hash -> order_id
			self.storage
				.store(
					StorageKey::OrderByTxHash.as_str(),
					&hex::encode(&tx_hash.0),
					&order.id,
					None,
				)
				.await
				.map_err(|e| OrderError::Storage(e.to_string()))?;

			// For Signet orders, remove intent from storage immediately after successful bundle submission
			// Signet bundles are submitted to 10 consecutive blocks, making them highly reliable
			// This prevents duplicate processing of the same intent
			if let Err(e) = self
				.storage
				.remove(StorageKey::Intents.as_str(), &order.id)
				.await
			{
				tracing::warn!(
					order_id = %order.id,
					error = %e,
					"Failed to remove intent after Signet bundle submission (non-critical)"
				);
			} else {
				tracing::info!(
					order_id = %order.id,
					"Removed intent from storage after successful Signet bundle submission"
				);
			}

			return Ok(());
		}

		// For non-Signet orders, use traditional fill transaction generation
		// Generate fill transaction
		let mut tx = self
			.order_service
			.generate_fill_transaction(&order, &params)
			.await
			.map_err(|e| OrderError::Service(e.to_string()))?;

		// For EIP-7683 orders, attach order data as metadata
		// This allows delivery to reconstruct the order for settlement
		// The lock_type information is embedded in the order.data JSON
		if order.standard == "eip7683" {
			tx.metadata = Some(order.data.clone());
		}

		// Submit transaction
		let tx_hash = self
			.delivery
			.deliver(tx.clone())
			.await
			.map_err(|e| OrderError::Service(e.to_string()))?;

		self.event_bus
			.publish(SolverEvent::Delivery(DeliveryEvent::TransactionPending {
				order_id: order.id.clone(),
				tx_hash: tx_hash.clone(),
				tx_type: TransactionType::Fill,
				tx_chain_id: tx.chain_id,
			}))
			.ok();

		// Store fill transaction
		self.state_machine
			.set_transaction_hash(&order.id, tx_hash.clone(), TransactionType::Fill)
			.await
			.map_err(|e| OrderError::State(e.to_string()))?;

		// Store reverse mapping: tx_hash -> order_id
		self.storage
			.store(
				StorageKey::OrderByTxHash.as_str(),
				&hex::encode(&tx_hash.0),
				&order.id,
				None,
			)
			.await
			.map_err(|e| OrderError::Storage(e.to_string()))?;

		Ok(())
	}
}
