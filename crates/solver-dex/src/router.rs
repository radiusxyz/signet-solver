//! DEX routing logic for finding optimal swap paths.
//!
//! This module provides:
//! - Multi-hop routing algorithm
//! - Route optimization based on liquidity and fees
//! - Swap quote aggregation

use crate::uniswap_v4::UniswapV4Router;
use alloy_primitives::{hex, U256};
use solver_types::Address;
use std::collections::VecDeque;
use thiserror::Error;

/// Represents errors that can happen during routing.
#[derive(Debug, Error)]
pub enum DexError {
	#[error("No route found from {0} to {1}")]
	NoRouteFound(String, String),

	#[error("Insufficient liquidity for swap")]
	InsufficientLiquidity,

	#[error("Slippage tolerance exceeded: expected {expected}, got {actual}")]
	SlippageExceeded { expected: String, actual: String },

	#[error("Pool discovery failed: {0}")]
	PoolDiscoveryFailed(String),

	#[error("Swap simulation failed: {0}")]
	SimulationFailed(String),

	#[error("DEX contract error: {0}")]
	ContractError(String),
}

/// Represents a token pair for swapping
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TokenPair {
	pub token_in: Address,
	pub token_out: Address,
	pub chain_id: u64,
}

/// Represents a liquidity pool on a DEX
#[derive(Debug, Clone)]
pub struct PoolInfo {
	pub pool_address: Address,
	pub token_a: Address,
	pub token_b: Address,
	pub reserve_a: U256,
	pub reserve_b: U256,
	pub fee_bps: u16, // Fee in basis points
}

/// Represents a single swap route from input token to output token
#[derive(Debug, Clone)]
pub struct SwapRoute {
	/// Sequence of tokens in the route (includes input and output)
	pub path: Vec<Address>,
	/// Pools used for each hop
	pub pools: Vec<PoolInfo>,
	/// Estimated output amount
	pub amount_out: U256,
	/// Total fees in basis points
	pub total_fee_bps: u16,
}

/// Quote for a swap operation
#[derive(Debug, Clone)]
pub struct SwapQuote {
	/// The swap route
	pub route: SwapRoute,
	/// Input token amount
	pub amount_in: U256,
	/// Expected output amount (before slippage)
	pub amount_out: U256,
	/// Minimum output amount (after slippage tolerance)
	pub amount_out_min: U256,
	/// Slippage tolerance in basis points
	pub slippage_bps: u16,
	/// Estimated gas cost for the swap
	pub estimated_gas: u64,
}

/// DEX router for finding and executing optimal swap routes
pub struct DexRouter {
	/// Uniswap V4 router instance
	uniswap_router: UniswapV4Router,
	/// Maximum number of hops allowed in a route
	max_hops: usize,
	/// Slippage tolerance in basis points
	slippage_tolerance_bps: u16,
	/// Pool price information: (token0, token1, price_ratio)
	pool_prices: Vec<(Address, Address, f64)>,
}

impl DexRouter {
	/// Creates a new DexRouter instance with pool price information
	pub fn new(
		uniswap_router: UniswapV4Router,
		max_hops: usize,
		slippage_tolerance_bps: u16,
		pool_prices: Vec<(Address, Address, f64)>,
	) -> Self {
		Self {
			uniswap_router,
			max_hops,
			slippage_tolerance_bps,
			pool_prices,
		}
	}

	/// Updates pool prices (for dynamic price updates)
	pub fn update_pool_prices(&mut self, pool_prices: Vec<(Address, Address, f64)>) {
		self.pool_prices = pool_prices;
	}

	/// Finds the best swap route from input token to output token.
	///
	/// Uses BFS to explore all possible routes up to max_hops.
	/// Given the limited number of pools, this is efficient enough.
	///
	/// # Arguments
	///
	/// * `token_in` - Input token address
	/// * `token_out` - Output token address
	/// * `amount_out_needed` - Amount of output token needed
	///
	/// # Returns
	///
	/// Returns the optimal SwapQuote with slippage buffer included.
	pub async fn find_best_route(
		&self,
		token_in: &Address,
		token_out: &Address,
		amount_out_needed: U256,
	) -> Result<SwapQuote, DexError> {
		tracing::info!(
			token_in = %hex::encode(&token_in.0),
			token_out = %hex::encode(&token_out.0),
			amount_out_needed = %amount_out_needed,
			max_hops = self.max_hops,
			pools_available = self.pool_prices.len(),
			"Finding best swap route"
		);

		// Build graph of available routes
		let routes = self.find_all_routes(token_in, token_out)?;

		if routes.is_empty() {
			tracing::warn!(
				token_in = %hex::encode(&token_in.0),
				token_out = %hex::encode(&token_out.0),
				"No route found between tokens"
			);
			return Err(DexError::NoRouteFound(
				hex::encode(&token_in.0),
				hex::encode(&token_out.0),
			));
		}

		let routes_for_debug: Vec<Vec<String>> = routes
			.iter()
			.map(|route| route.iter().map(|t| hex::encode(&t.0)).collect())
			.collect();
		tracing::debug!(
			routes = ?routes_for_debug,
			max_hops = self.max_hops,
			"Enumerated swap routes for candidate search"
		);

		// Find best route by calculating quotes for each
		let mut best_route: Option<(Vec<Address>, U256, U256)> = None; // (path, amount_in, amount_out)

		for route in routes {
			// Calculate how much input we need to get the desired output
			// We work backwards from output to input
			let amount_in_needed = self.calculate_input_needed(&route, amount_out_needed)?;

			// Add slippage buffer (swap more than needed)
			let amount_in_with_buffer = self.add_slippage_buffer(amount_in_needed);

			// Calculate actual output with buffered input
			let amount_out = self.uniswap_router.calculate_quote(
				&route,
				amount_in_with_buffer,
				&self.pool_prices,
			)?;

			tracing::debug!(
				route = ?route.iter().map(|t| hex::encode(&t.0)).collect::<Vec<_>>(),
				amount_in_needed = %amount_in_needed,
				amount_in_with_buffer = %amount_in_with_buffer,
				amount_out = %amount_out,
				"Calculated route quote"
			);

			// Check if we get enough output
			if amount_out < amount_out_needed {
				tracing::debug!("Route does not provide enough output, skipping");
				continue;
			}

			// Select route with minimum input requirement
			if best_route.is_none()
				|| amount_in_with_buffer < best_route.as_ref().unwrap().1
			{
				best_route = Some((route, amount_in_with_buffer, amount_out));
			}
		}

		let (route, amount_in, amount_out) = best_route.ok_or_else(|| {
			DexError::InsufficientLiquidity
		})?;

		// Calculate minimum output amount with slippage protection
		let amount_out_min = self.calculate_min_amount_out(amount_out);

		// Estimate gas based on route length
		let estimated_gas = self.estimate_swap_gas(route.len() - 1);

		let hop_count = route.len().saturating_sub(1);
		tracing::info!(
			route_tokens = route.len(),
			route_hops = hop_count,
			amount_in = %amount_in,
			amount_out = %amount_out,
			amount_out_min = %amount_out_min,
			estimated_gas = estimated_gas,
			"Found best route"
		);

		let swap_route = SwapRoute {
			path: route,
			pools: Vec::new(), // Pool info not needed for execution
			amount_out,
			total_fee_bps: 0, // 0% fee for testing
		};

		Ok(SwapQuote {
			route: swap_route,
			amount_in,
			amount_out,
			amount_out_min,
			slippage_bps: self.slippage_tolerance_bps,
			estimated_gas,
		})
	}

	/// Finds all possible routes from token_in to token_out using BFS.
	fn find_all_routes(
		&self,
		token_in: &Address,
		token_out: &Address,
	) -> Result<Vec<Vec<Address>>, DexError> {
		let mut routes = Vec::new();
		let mut queue = VecDeque::new();

		// Start with token_in
		queue.push_back(vec![token_in.clone()]);

		while let Some(current_path) = queue.pop_front() {
			let current_token = current_path.last().unwrap();

			// Check if we reached the destination
			if current_token == token_out {
				routes.push(current_path.clone());
				continue;
			}

			// Don't exceed max hops
			if current_path.len() > self.max_hops {
				continue;
			}

			// Find all connected tokens
			for (t0, t1, _) in &self.pool_prices {
				let next_token = if t0 == current_token {
					Some(t1)
				} else if t1 == current_token {
					Some(t0)
				} else {
					None
				};

				if let Some(next) = next_token {
					// Avoid cycles
					if !current_path.contains(next) {
						let mut new_path = current_path.clone();
						new_path.push(next.clone());
						queue.push_back(new_path);
					}
				}
			}
		}

		tracing::debug!(
			routes_found = routes.len(),
			token_in = %hex::encode(&token_in.0),
			token_out = %hex::encode(&token_out.0),
			"Found routes"
		);

		Ok(routes)
	}

	/// Calculates input amount needed to get desired output (working backwards).
	fn calculate_input_needed(
		&self,
		route: &[Address],
		amount_out: U256,
	) -> Result<U256, DexError> {
		let mut current_amount = amount_out;

		// Work backwards from output to input
		for i in (0..route.len() - 1).rev() {
			let token_in = &route[i];
			let token_out = &route[i + 1];

			// Find the pool price
			let price_ratio = self.pool_prices
				.iter()
				.find(|(t0, t1, _)| {
					(t0 == token_in && t1 == token_out) || (t0 == token_out && t1 == token_in)
				})
				.map(|(t0, t1, ratio)| {
					if t0 == token_in && t1 == token_out {
						*ratio // Forward direction
					} else {
						1.0 / ratio // Reverse direction
					}
				})
				.ok_or_else(|| {
					DexError::NoRouteFound(hex::encode(&token_in.0), hex::encode(&token_out.0))
				})?;

			// Calculate input needed: amount_in = amount_out / price_ratio
			let inverse_ratio = 1.0 / price_ratio;
			let ratio_scaled = (inverse_ratio * 1e18) as u128;
			current_amount = current_amount
				.saturating_mul(U256::from(ratio_scaled))
				.checked_div(U256::from(1e18 as u128))
				.unwrap_or(U256::MAX);
		}

		Ok(current_amount)
	}

	/// Adds slippage buffer to input amount (swap more than needed).
	fn add_slippage_buffer(&self, amount: U256) -> U256 {
		// Add slippage buffer: amount * (10000 + slippage_bps) / 10000
		let multiplier = 10000 + self.slippage_tolerance_bps as u128;
		let divisor = 10000u128;

		amount
			.saturating_mul(U256::from(multiplier))
			.checked_div(U256::from(divisor))
			.unwrap_or(amount)
	}

	/// Calculates the minimum output amount considering slippage tolerance.
	fn calculate_min_amount_out(&self, amount_out: U256) -> U256 {
		let slippage_multiplier = 10000 - self.slippage_tolerance_bps as u128;
		let divisor = 10000u128;

		// amount_out_min = amount_out * (10000 - slippage_bps) / 10000
		amount_out
			.saturating_mul(U256::from(slippage_multiplier))
			.checked_div(U256::from(divisor))
			.unwrap_or(U256::ZERO)
	}

	/// Estimates gas cost for a swap with given number of hops.
	///
	/// TODO: Implement dynamic gas estimation based on route complexity.
	fn estimate_swap_gas(&self, num_hops: usize) -> u64 {
		// Rough estimates for Uniswap V4 (adjust based on actual measurements)
		const BASE_GAS: u64 = 100_000;
		const GAS_PER_HOP: u64 = 50_000;

		BASE_GAS + (num_hops as u64 * GAS_PER_HOP)
	}

	/// Returns the Uniswap router instance
	pub fn uniswap_router(&self) -> &UniswapV4Router {
		&self.uniswap_router
	}

	/// Returns the maximum hops configuration
	pub fn max_hops(&self) -> usize {
		self.max_hops
	}

	/// Returns the slippage tolerance
	pub fn slippage_tolerance_bps(&self) -> u16 {
		self.slippage_tolerance_bps
	}

	/// Generates transactions for executing a swap.
	///
	/// Returns a vector of transactions that need to be executed atomically:
	/// - Approve transactions for each token being swapped
	/// - Swap transactions for each hop in the route
	///
	/// # Arguments
	///
	/// * `quote` - The swap quote containing route and amounts
	/// * `chain_id` - Chain ID for the transactions
	///
	/// # Returns
	///
	/// Vector of transactions ready to be signed and submitted
	pub fn generate_swap_transactions(
		&self,
		quote: &SwapQuote,
		chain_id: u64,
	) -> Result<Vec<solver_types::Transaction>, DexError> {
		let mut transactions = Vec::new();
		let router_address = self.uniswap_router.router_address();

		tracing::info!(
			route_length = quote.route.path.len(),
			amount_in = %quote.amount_in,
			"Generating swap transactions"
		);

		// For each hop in the route, generate approve + swap transactions
		for i in 0..quote.route.path.len() - 1 {
			let token_in = &quote.route.path[i];
			let token_out = &quote.route.path[i + 1];

			// Determine amount for this hop
			// For the first hop, use the total input amount
			// For subsequent hops, the amount comes from the previous swap output
			let amount_in = if i == 0 {
				quote.amount_in
			} else {
				// For multi-hop, we'd need to calculate intermediate amounts
				// For now, we'll use the input amount (single-hop swaps)
				quote.amount_in
			};

			// 1. Generate approve transaction for token_in
			let approve_calldata = crate::uniswap_v4::UniswapV4Router::encode_approve_for_spender(
				token_in,
				router_address,
				amount_in,
			)?;

			let approve_tx = solver_types::Transaction {
				to: Some(token_in.clone()),
				data: approve_calldata.to_vec(),
				value: U256::ZERO,
				chain_id,
				nonce: None, // Will be filled by delivery service
				gas_limit: Some(100_000), // Standard approve gas limit
				gas_price: None,
				max_fee_per_gas: None,
				max_priority_fee_per_gas: None,
				metadata: None,
			};

			transactions.push(approve_tx);

			// 2. Generate swap transaction
			let swap_calldata =
				self.uniswap_router
					.encode_swap(token_in, token_out, amount_in, quote.amount_out_min)?;

			let swap_tx = solver_types::Transaction {
				to: Some(Address(router_address.as_slice().to_vec())),
				data: swap_calldata.to_vec(),
				value: U256::ZERO,
				chain_id,
				nonce: None, // Will be filled by delivery service
				gas_limit: Some(500_000), // Higher gas limit for swaps
				gas_price: None,
				max_fee_per_gas: None,
				max_priority_fee_per_gas: None,
				metadata: None,
			};

			transactions.push(swap_tx);

			tracing::debug!(
				hop = i + 1,
				token_in = hex::encode(&token_in.0),
				token_out = hex::encode(&token_out.0),
				amount = %amount_in,
				"Generated swap transaction for hop"
			);
		}

		tracing::info!(
			total_transactions = transactions.len(),
			"Generated all swap transactions"
		);

		Ok(transactions)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use alloy_primitives::Address as AlloyAddress;

	fn create_test_pool_prices() -> Vec<(Address, Address, f64)> {
		let token_c = Address(hex::decode("1Cba8B73D2B655FE0f6af733873dA09328FF4d99").unwrap());
		let token_d = Address(hex::decode("897afeae2AB0b372E8f23f281005360792d05b93").unwrap());
		let token_b = Address(hex::decode("bDD57Be55e6aF632A078397234ACd8B16ffBfd53").unwrap());

		vec![
			(token_c.clone(), token_d.clone(), 2.0),  // 1C = 2D
			(token_d, token_b, 0.55),                  // 1D = 0.55B
		]
	}

	#[test]
	fn test_calculate_min_amount_out() {
		let router_address = "0xF46dE10373357f90193D11388cb979C948112041"
			.parse::<AlloyAddress>()
			.unwrap();
		let uniswap_router = UniswapV4Router::new(router_address, 1, 0);
		let pool_prices = create_test_pool_prices();
		let router = DexRouter::new(uniswap_router, 2, 100, pool_prices); // 1% slippage

		let amount_out = U256::from(100_000);
		let min_out = router.calculate_min_amount_out(amount_out);

		// 100,000 * (10000 - 100) / 10000 = 99,000
		assert_eq!(min_out, U256::from(99_000));
	}

	#[test]
	fn test_add_slippage_buffer() {
		let router_address = "0xF46dE10373357f90193D11388cb979C948112041"
			.parse::<AlloyAddress>()
			.unwrap();
		let uniswap_router = UniswapV4Router::new(router_address, 1, 0);
		let pool_prices = create_test_pool_prices();
		let router = DexRouter::new(uniswap_router, 2, 100, pool_prices); // 1% slippage

		let amount = U256::from(100_000);
		let buffered = router.add_slippage_buffer(amount);

		// 100,000 * (10000 + 100) / 10000 = 101,000
		assert_eq!(buffered, U256::from(101_000));
	}

	#[test]
	fn test_find_all_routes() {
		let router_address = "0xF46dE10373357f90193D11388cb979C948112041"
			.parse::<AlloyAddress>()
			.unwrap();
		let uniswap_router = UniswapV4Router::new(router_address, 1, 0);
		let pool_prices = create_test_pool_prices();
		let router = DexRouter::new(uniswap_router, 3, 100, pool_prices); // Allow 3 hops

		let token_c = Address(hex::decode("1Cba8B73D2B655FE0f6af733873dA09328FF4d99").unwrap());
		let token_b = Address(hex::decode("bDD57Be55e6aF632A078397234ACd8B16ffBfd53").unwrap());

		let routes = router.find_all_routes(&token_c, &token_b).unwrap();

		assert!(!routes.is_empty(), "Should find at least one route");
		assert_eq!(routes[0].len(), 3, "Route should be C -> D -> B (3 tokens)");
	}

	#[test]
	fn test_calculate_input_needed() {
		let router_address = "0xF46dE10373357f90193D11388cb979C948112041"
			.parse::<AlloyAddress>()
			.unwrap();
		let uniswap_router = UniswapV4Router::new(router_address, 1, 0);
		let pool_prices = create_test_pool_prices();
		let router = DexRouter::new(uniswap_router, 3, 100, pool_prices);

		let token_c = Address(hex::decode("1Cba8B73D2B655FE0f6af733873dA09328FF4d99").unwrap());
		let token_d = Address(hex::decode("897afeae2AB0b372E8f23f281005360792d05b93").unwrap());
		let token_b = Address(hex::decode("bDD57Be55e6aF632A078397234ACd8B16ffBfd53").unwrap());

		let route = vec![token_c, token_d, token_b];
		let amount_out = U256::from(110); // Want 110 B

		let amount_in = router.calculate_input_needed(&route, amount_out).unwrap();

		// Working backwards:
		// 110B -> need 110/0.55 = 200D
		// 200D -> need 200/2 = 100C
		// So we need ~100C to get 110B (allow 1-2% rounding due to float precision)
		assert!(
			amount_in >= U256::from(99) && amount_in <= U256::from(101),
			"Expected ~100, got {}",
			amount_in
		);
	}

	#[test]
	fn test_estimate_swap_gas() {
		let router_address = "0xF46dE10373357f90193D11388cb979C948112041"
			.parse::<AlloyAddress>()
			.unwrap();
		let uniswap_router = UniswapV4Router::new(router_address, 1, 0);
		let pool_prices = create_test_pool_prices();
		let router = DexRouter::new(uniswap_router, 2, 100, pool_prices);

		assert_eq!(router.estimate_swap_gas(1), 150_000);
		assert_eq!(router.estimate_swap_gas(2), 200_000);
	}
}
