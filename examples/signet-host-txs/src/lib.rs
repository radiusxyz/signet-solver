use std::{
	collections::{HashMap, HashSet},
	path::{Path, PathBuf},
	time::Duration,
};

use anyhow::{Context, Result};
use clap::Parser;
use alloy_primitives::Bytes;
use reqwest::Client as HttpClient;
use serde_json::json;
use signet_tx_cache::client::TxCache;
use solver_config::Config;
use solver_delivery::implementations::signet::bundle::SignetBundleDelivery;
use solver_types::SecretString;
use tokio::time::sleep;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use alloy_network::AnyNetwork;
use alloy_provider::{Provider, ProviderBuilder};

#[derive(Parser, Debug)]
#[command(
	about = "Preview and submit host-chain swap transactions for Signet intents",
	version
)]
pub struct Options {
	/// Path to the solver configuration file (TOML)
	#[arg(long, default_value = "config/demo.toml")]
	pub config: PathBuf,

	/// Poll interval for Signet cache in milliseconds
	#[arg(long, default_value_t = 500)]
	pub poll_ms: u64,

	/// Continue running after processing the first intent
	#[arg(long)]
	pub keep_running: bool,

	/// Build host transactions but do not submit them to the host chain
	#[arg(long)]
	pub dry_run: bool,

	/// Builder RPC (eth_sendBundle) endpoint for host-chain bundles
	#[arg(long, default_value = "https://host-builder-rpc.pecorino.signet.sh")]
	pub builder_rpc: String,
}

fn resolve_config_path(path: &Path) -> Result<PathBuf> {
	if path.is_absolute() {
		Ok(path.to_path_buf())
	} else {
		let cwd = std::env::current_dir()?;
		Ok(cwd.join(path))
	}
}

fn extract_secret(table: &toml::value::Table, key: &str) -> Result<SecretString> {
	let value = table
		.get(key)
		.and_then(|v| v.as_str())
		.context(format!("Missing '{}' in account configuration", key))?;
	Ok(SecretString::from(value))
}

fn extract_network_keys(table: &toml::value::Table) -> HashMap<u64, SecretString> {
	let mut result = HashMap::new();
	if let Some(network_table) = table.get("network_private_keys").and_then(|v| v.as_table()) {
		for (chain_id_str, value) in network_table {
			if let Some(key) = value.as_str() {
				match chain_id_str.parse::<u64>() {
					Ok(chain_id) => {
						result.insert(chain_id, SecretString::from(key));
					},
					Err(err) => {
						warn!(%chain_id_str, %err, "Skipping invalid chain id in network_private_keys");
					},
				}
			}
		}
	}
	result
}

fn tx_cache_from_url(url: &str) -> Result<TxCache> {
	TxCache::new_from_string(url)
		.map_err(|e| anyhow::anyhow!("Failed to create Signet cache client for url={}: {}", url, e))
}

async fn submit_host_bundle(
	builder_rpc: &str,
	host_txs: &[Bytes],
	target_block: u64,
) -> Result<serde_json::Value> {
	let tx_hex: Vec<String> = host_txs
		.iter()
		.map(|tx| format!("0x{}", hex::encode(tx.as_ref())))
		.collect();

	let payload = json!({
		"jsonrpc": "2.0",
		"id": 1,
		"method": "eth_sendBundle",
		"params": [{
			"txs": tx_hex,
			"blockNumber": format!("0x{:x}", target_block),
		}]
	});

	let client = HttpClient::new();
	let response = client.post(builder_rpc).json(&payload).send().await?;
	let status = response.status();
	let body = response
		.json::<serde_json::Value>()
		.await
		.unwrap_or_else(|_| json!({"error": "invalid json response"}));

	if !status.is_success() {
		return Err(anyhow::anyhow!(
			"Builder RPC {} responded with {} body={}",
			builder_rpc,
			status,
			body
		));
	}

	Ok(body)
}

pub async fn run() -> Result<()> {
	let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
	tracing_subscriber::fmt().with_env_filter(env_filter).init();

	let options = Options::parse();

	let config_path = resolve_config_path(&options.config)?;
	let config_path_str = config_path.to_str().ok_or_else(|| {
		anyhow::anyhow!(
			"Config path contains invalid UTF-8: {}",
			config_path.display()
		)
	})?;
	let config = Config::from_file(config_path_str)
		.await
		.with_context(|| format!("Failed to load config from {}", config_path.display()))?;

	let signet_bundle_value = config
		.delivery
		.implementations
		.get("signet_bundle")
		.context("Missing 'signet_bundle' delivery configuration")?;
	let signet_bundle_table = signet_bundle_value
		.as_table()
		.context("signet_bundle configuration must be a table")?;

	let account_impl = config
		.account
		.implementations
		.get(&config.account.primary)
		.context("Missing primary account configuration")?;
	let account_table = account_impl
		.as_table()
		.context("Primary account configuration must be a table")?;

	let default_private_key = extract_secret(account_table, "private_key")?;
	let network_keys = extract_network_keys(account_table);

	let delivery = SignetBundleDelivery::from_config(
		signet_bundle_value,
		&config.networks,
		&default_private_key,
		&network_keys,
	)?;

	let chain_name = signet_bundle_table
		.get("chain_name")
		.and_then(|v| v.as_str())
		.unwrap_or("pecorino")
		.to_string();

	let tx_cache = if chain_name == "pecorino" {
		TxCache::pecorino()
	} else {
		let url = format!("https://cache.{}.signet.sh", chain_name);
		tx_cache_from_url(&url)?
	};

	let host_chain_id = signet_bundle_table
		.get("host_chain_id")
		.and_then(|v| v.as_integer())
		.context("signet_bundle.host_chain_id is required")? as u64;
	let host_network = config
		.networks
		.get(&host_chain_id)
		.context("Missing network configuration for host chain")?;
	let host_rpc = host_network
		.get_http_url()
		.context("Host network does not define an HTTP RPC URL")?;

	let host_provider = if options.dry_run {
		None
	} else {
		let url = host_rpc
			.parse::<reqwest::Url>()
			.map_err(|err| anyhow::anyhow!("Invalid host RPC URL {}: {}", host_rpc, err))?;
		Some(
			ProviderBuilder::new()
				.network::<AnyNetwork>()
				.connect_http(url),
		)
	};

	info!(%chain_name, host_chain_id, %host_rpc, "Starting Signet host transaction preview example");

	let poll_duration = Duration::from_millis(options.poll_ms);
	let mut processed_orders = HashSet::new();
	let mut orders_observed = 0usize;

	loop {
		sleep(poll_duration).await;

		info!("Polling Signet cache for orders");

		let orders_response = match tx_cache.get_orders(None).await {
			Ok(list) => list,
			Err(err) => {
				error!(?err, "Failed to fetch orders from Signet cache");
				continue;
			},
		};

		let orders = orders_response.inner().orders.clone();

		if orders.is_empty() {
			info!("No orders available yet; continuing to poll");
			continue;
		}

		for order in orders {
			let order_id = hex::encode(order.order_hash().as_slice());
			if processed_orders.contains(&order_id) {
				continue;
			}

			orders_observed += 1;
			info!(%order_id, outputs = order.outputs().len(), "Received SignedOrder from cache");

			match delivery.build_host_swap_transactions(&order).await {
				Ok(host_txs) => {
					if host_txs.is_empty() {
						info!(%order_id, "No host-chain transactions required for this order");
						processed_orders.insert(order_id);
						continue;
					}

					info!(%order_id, tx_count = host_txs.len(), "Generated host-chain swap transactions");

					if options.dry_run {
						for (idx, raw) in host_txs.iter().enumerate() {
							info!(
								%order_id,
								tx_index = idx,
								bytes = raw.len(),
								raw_tx = %format!("0x{}", hex::encode(raw.as_ref())),
								"Dry run: host transaction ready"
							);
						}
						processed_orders.insert(order_id.clone());
						if !options.keep_running {
							info!("Dry run complete");
							return Ok(());
						}
						continue;
					}

					if let Some(provider) = host_provider.as_ref() {
						let latest_block = provider
							.get_block_number()
							.await
							.context("Failed to fetch latest host block number")?;
						let target_block = latest_block + 1;
						info!(
							%order_id,
							target_block,
							host_tx_count = host_txs.len(),
							builder_rpc = %options.builder_rpc.as_str(),
							"Submitting host transactions as bundle"
						);

						match submit_host_bundle(&options.builder_rpc, &host_txs, target_block)
							.await
						{
							Ok(response) => {
								info!(
									%order_id,
									target_block,
									response = %response,
									"Host bundle submitted to builder"
								);
							},
							Err(err) => {
								error!(%order_id, target_block, ?err, "Failed to submit host bundle");
								continue;
							},
						}
					}

					processed_orders.insert(order_id.clone());
					if !options.keep_running {
						info!(%order_id, "Host swap execution complete; exiting");
						return Ok(());
					}
				},
				Err(err) => {
					error!(%order_id, ?err, "Failed to build host swap transactions");
				},
			}
		}

		if !options.keep_running && orders_observed == 0 {
			info!("No orders observed; exiting");
			return Ok(());
		}
	}
}
