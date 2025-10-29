//! Uniswap V4 integration module.
//!
//! This module provides Uniswap V4-specific functionality for:
//! - Pool discovery and querying
//! - Swap quote calculation
//! - Transaction encoding for swaps

use crate::router::{DexError, PoolInfo};
use alloy_primitives::{hex, Address as AlloyAddress, Bytes, I256, U160, U256};
use alloy_sol_types::{sol, SolCall};
use solver_types::Address;

// Uniswap V4 PoolSwapTest ABI definitions
sol! {
	/// Pool key identifying a unique pool
	#[derive(Debug)]
	struct PoolKey {
		address currency0;
		address currency1;
		uint24 fee;
		int24 tickSpacing;
		address hooks;
	}

	/// Swap parameters
	#[derive(Debug)]
	struct SwapParams {
		bool zeroForOne;
		int256 amountSpecified;
		uint160 sqrtPriceLimitX96;
	}

	/// Test settings for swap execution
	#[derive(Debug)]
	struct TestSettings {
		bool takeClaims;
		bool settleUsingBurn;
	}

	/// PoolSwapTest interface
	interface IPoolSwapTest {
		function swap(
			PoolKey calldata key,
			SwapParams calldata params,
			TestSettings calldata testSettings,
			bytes calldata hookData
		) external returns (int256);
	}

	/// ERC20 interface for token approvals
	interface IERC20 {
		function approve(address spender, uint256 amount) external returns (bool);
	}
}

/// Uniswap V4 Router contract interface
pub struct UniswapV4Router {
	router_address: AlloyAddress,
	chain_id: u64,
	default_fee_tier: u32, // in hundredths of a bip (3000 = 0.3%)
}

impl UniswapV4Router {
	/// Creates a new Uniswap V4 Router instance
	pub fn new(router_address: AlloyAddress, chain_id: u64, default_fee_tier: u32) -> Self {
		Self {
			router_address,
			chain_id,
			default_fee_tier,
		}
	}

	/// Discovers pools for a token pair.
	///
	/// For demo purposes, this returns hardcoded pool information.
	/// In production, this should query the PoolManager contract.
	pub async fn discover_pools(
		&self,
		_token_in: &Address,
		_token_out: &Address,
	) -> Result<Vec<PoolInfo>, DexError> {
		// TODO: Implement on-chain pool discovery
		// For now, pool information is managed in config

		tracing::debug!("Pool discovery deferred to routing logic with config-based pools");

		Ok(Vec::new())
	}

	/// Calculates the output amount for a given input amount and route.
	///
	/// Uses hardcoded pool price ratios from config.
	pub fn calculate_quote(
		&self,
		route: &[Address],
		amount_in: U256,
		pool_prices: &[(Address, Address, f64)], // (token0, token1, price_ratio)
	) -> Result<U256, DexError> {
		let mut current_amount = amount_in;

		for i in 0..route.len() - 1 {
			let token_in = &route[i];
			let token_out = &route[i + 1];

			// Find the pool price
			let price_ratio = pool_prices
				.iter()
				.find(|(t0, t1, _)| {
					(t0 == token_in && t1 == token_out) || (t0 == token_out && t1 == token_in)
				})
				.map(|(t0, t1, ratio)| {
					if t0 == token_in && t1 == token_out {
						*ratio
					} else {
						1.0 / ratio // Inverse ratio
					}
				})
				.ok_or_else(|| {
					DexError::NoRouteFound(hex::encode(&token_in.0), hex::encode(&token_out.0))
				})?;

			// Apply price ratio
			// amount_out = amount_in * price_ratio
			let ratio_scaled = (price_ratio * 1e18) as u128;
			current_amount = current_amount
				.saturating_mul(U256::from(ratio_scaled))
				.checked_div(U256::from(1e18 as u128))
				.unwrap_or(U256::ZERO);
		}

		tracing::debug!(
			amount_in = %amount_in,
			amount_out = %current_amount,
			"Calculated swap quote"
		);

		Ok(current_amount)
	}

	/// Encodes a swap transaction for a single hop.
	///
	/// Creates the calldata for PoolSwapTest.swap() with the given parameters.
	/// For multi-hop swaps, this should be called multiple times for each hop.
	pub fn encode_swap(
		&self,
		token_in: &Address,
		token_out: &Address,
		amount_in: U256,
		_amount_out_min: U256, // Not used in PoolSwapTest, but kept for compatibility
	) -> Result<Bytes, DexError> {
		// Determine token order for pool key
		// In Uniswap V4, tokens are ordered: currency0 < currency1
		let (currency0, currency1, zero_for_one) = if token_in.0 < token_out.0 {
			(
				AlloyAddress::from_slice(&token_in.0),
				AlloyAddress::from_slice(&token_out.0),
				true, // Swapping currency0 -> currency1
			)
		} else {
			(
				AlloyAddress::from_slice(&token_out.0),
				AlloyAddress::from_slice(&token_in.0),
				false, // Swapping currency1 -> currency0
			)
		};

		// Create pool key
		let pool_key = PoolKey {
			currency0,
			currency1,
			fee: alloy_primitives::Uint::from(self.default_fee_tier),
			tickSpacing: alloy_primitives::Signed::try_from(60i64).unwrap_or_default(), // Fixed tick spacing from config
			hooks: AlloyAddress::ZERO,
		};

		// Create swap params
		// Negative amountSpecified means exact input swap
		let amount_specified = if let Ok(amount_i256) = I256::try_from(amount_in) {
			-amount_i256 // Negative for exact input
		} else {
			return Err(DexError::ContractError(
				"Amount too large to convert to I256".to_string(),
			));
		};

		// sqrtPriceLimitX96: Use min/max values based on direction
		// For zeroForOne (buying token1): use MIN_SQRT_PRICE + 1
		// For oneForZero (buying token0): use MAX_SQRT_PRICE - 1
		let sqrt_price_limit = if zero_for_one {
			U160::from(4295128740_u64) // MIN_SQRT_PRICE + 1
		} else {
			U160::from_str_radix("1461446703485210103287273052203988822378723970341", 10)
				.unwrap_or(U160::MAX)
		};

		let swap_params = SwapParams {
			zeroForOne: zero_for_one,
			amountSpecified: amount_specified,
			sqrtPriceLimitX96: sqrt_price_limit,
		};

		// Test settings
		let test_settings = TestSettings {
			takeClaims: false,
			settleUsingBurn: false,
		};

		// Empty hook data
		let hook_data = Bytes::new();

		// Encode the function call
		let call = IPoolSwapTest::swapCall {
			key: pool_key,
			params: swap_params,
			testSettings: test_settings,
			hookData: hook_data.clone(),
		};
		let calldata = call.abi_encode();

		tracing::info!(
			token_in = hex::encode(&token_in.0),
			token_out = hex::encode(&token_out.0),
			amount_in = %amount_in,
			zero_for_one = zero_for_one,
			calldata_len = calldata.len(),
			"Encoded swap transaction"
		);

		Ok(Bytes::from(calldata))
	}

	/// Returns the router contract address
	pub fn router_address(&self) -> AlloyAddress {
		self.router_address
	}

	/// Returns the chain ID
	pub fn chain_id(&self) -> u64 {
		self.chain_id
	}

	/// Encodes an ERC20 approve transaction
	///
	/// Creates the calldata for ERC20.approve() to approve the router to spend tokens.
	pub fn encode_approve(token: &Address, amount: U256) -> Result<Bytes, DexError> {
		let spender = AlloyAddress::ZERO; // Will be set by caller
		let call = IERC20::approveCall { spender, amount };
		let calldata = call.abi_encode();

		tracing::debug!(
			token = hex::encode(&token.0),
			amount = %amount,
			calldata_len = calldata.len(),
			"Encoded approve transaction"
		);

		Ok(Bytes::from(calldata))
	}

	/// Generates approve calldata for a specific spender
	pub fn encode_approve_for_spender(
		token: &Address,
		spender: AlloyAddress,
		amount: U256,
	) -> Result<Bytes, DexError> {
		let call = IERC20::approveCall { spender, amount };
		let calldata = call.abi_encode();

		tracing::debug!(
			token = hex::encode(&token.0),
			spender = %spender,
			amount = %amount,
			"Encoded approve for spender"
		);

		Ok(Bytes::from(calldata))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_router_creation() {
		let router_address = "0xF46dE10373357f90193D11388cb979C948112041"
			.parse::<AlloyAddress>()
			.unwrap();
		let router = UniswapV4Router::new(router_address, 1, 0);
		assert_eq!(router.chain_id(), 1);
		assert_eq!(router.router_address(), router_address);
	}

	#[test]
	fn test_swap_encoding() {
		let router_address = "0xF46dE10373357f90193D11388cb979C948112041"
			.parse::<AlloyAddress>()
			.unwrap();
		let router = UniswapV4Router::new(router_address, 1, 0);

		// Token addresses (ensure proper ordering)
		let token_a = Address(vec![0x11; 20]); // Lower address
		let token_b = Address(vec![0x22; 20]); // Higher address

		let amount_in = U256::from(100_000_000u64); // 100 USDC (6 decimals)
		let amount_out_min = U256::from(10_000_000_000_000_000u64); // 0.01 WETH (18 decimals)

		let calldata = router
			.encode_swap(&token_a, &token_b, amount_in, amount_out_min)
			.unwrap();

		// Verify calldata is not empty and has reasonable length
		assert!(calldata.len() > 0);
		assert!(calldata.len() < 1000); // Reasonable upper bound

		tracing::debug!("Swap calldata length: {} bytes", calldata.len());
	}

	#[test]
	fn test_zero_for_one_direction() {
		let router_address = "0xF46dE10373357f90193D11388cb979C948112041"
			.parse::<AlloyAddress>()
			.unwrap();
		let router = UniswapV4Router::new(router_address, 1, 0);

		// Test both directions
		let token_low = Address(vec![0x11; 20]);
		let token_high = Address(vec![0x22; 20]);

		let amount = U256::from(100u64);

		// Swap low -> high should be zeroForOne = true
		let calldata1 = router
			.encode_swap(&token_low, &token_high, amount, U256::ZERO)
			.unwrap();
		assert!(calldata1.len() > 0);

		// Swap high -> low should be zeroForOne = false
		let calldata2 = router
			.encode_swap(&token_high, &token_low, amount, U256::ZERO)
			.unwrap();
		assert!(calldata2.len() > 0);

		// Calldatas should be different due to different swap direction
		assert_ne!(calldata1, calldata2);
	}
}
