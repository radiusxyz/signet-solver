use std::{
	collections::{HashMap, HashSet},
	path::{Path, PathBuf},
	time::Duration,
};

use anyhow::{Context, Result};
use clap::Parser;
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
struct Options {
	/// Path to the solver configuration file (TOML)
	#[arg(long, default_value = "config/demo.toml")]
	config: PathBuf,

	/// Poll interval for Signet cache in milliseconds
	#[arg(long, default_value_t = 500)]
	poll_ms: u64,

	/// Continue running after processing the first intent
	#[arg(long)]
	keep_running: bool,

	/// Build host transactions but do not submit them to the host chain
	#[arg(long)]
	dry_run: bool,
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

#[tokio::main]
async fn main() -> Result<()> {
	let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
	tracing_subscriber::fmt().with_env_filter(env_filter).init();

	let options = Options::parse();

	let config_path = resolve_config_path(&options.config)?;
	let config_path_str = config_path
		.to_str()
		.ok_or_else(|| anyhow::anyhow!("Config path contains invalid UTF-8: {}", config_path.display()))?;
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
		tx_cache_pecorino()
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

		let orders = match tx_cache.get_orders().await {
			Ok(list) => list,
			Err(err) => {
				error!(?err, "Failed to fetch orders from Signet cache");
				continue;
			},
		};

		if orders.is_empty() {
			continue;
		}

		for order in orders {
			let order_id = hex::encode(order.order_hash().as_slice());
			if processed_orders.contains(&order_id) {
				continue;
			}

			orders_observed += 1;
			info!(%order_id, outputs = order.outputs.len(), "Received SignedOrder from cache");

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
                            for (idx, raw) in host_txs.iter().enumerate() {
                                let raw_hex = format!("0x{}", hex::encode(raw.as_ref()));
                                info!(%order_id, tx_index = idx, raw_tx = %raw_hex, "Submitting host transaction");

                                match provider.send_raw_transaction(raw.as_ref()).await {
                                    Ok(pending) => {
                                        let pending = pending.with_required_confirmations(1);
                                        let tx_hash_hex = format!(
                                            "0x{}",
                                            hex::encode(pending.tx_hash().as_slice())
                                        );
                                        info!(%order_id, tx_index = idx, tx_hash = %tx_hash_hex, "Host transaction broadcast");
                                        match pending.get_receipt().await {
                                            Ok(receipt) => {
                                                info!(%order_id, tx_index = idx, ?receipt, "Host transaction confirmed");
                                            },
                                            Err(err) => {
                                                error!(%order_id, tx_index = idx, ?err, "Error while awaiting host transaction confirmation");
                                            },
                                        }
                                    },
                                    Err(err) => {
                                        error!(%order_id, tx_index = idx, ?err, "Failed to broadcast host transaction");
                                    },
                                }
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

fn tx_cache_pecorino() -> TxCache {
	TxCache::pecorino()
}

fn tx_cache_from_url(url: &str) -> Result<TxCache> {
	TxCache::new_from_string(url)
		.map_err(|err| anyhow::anyhow!("Failed to create Signet cache client for {}: {}", url, err))
}
