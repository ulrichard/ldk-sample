use crate::convert::{BlockchainInfo, FeeResponse, FundedTx, NewAddress, RawTx, SignedTx};
use base64;
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::consensus::encode;
use bitcoin::hash_types::{BlockHash, Txid};
use bitcoin::util::address::Address;
use bitcoin::Network;
use electrum_client::{Client, ElectrumApi};
use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning_block_sync::http::HttpEndpoint;
use lightning_block_sync::rpc::RpcClient;
use lightning_block_sync::{AsyncBlockSourceResult, BlockData, BlockHeaderData, BlockSource};
use serde_json;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub struct ElectrumClient {
	electrum_client: Arc<Client>,
	url: String,
	fees: Arc<HashMap<Target, AtomicU32>>,
	handle: tokio::runtime::Handle,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub enum Target {
	Background,
	Normal,
	HighPriority,
}

/// The minimum feerate we are allowed to send, as specify by LDK.
const MIN_FEERATE: u32 = 253;

impl ElectrumClient {
	pub async fn new(network: Network, handle: tokio::runtime::Handle) -> std::io::Result<Self> {
		let url = match network {
			Network::Testnet => "ssl://electrum.blockstream.info:60002",
			Network::Bitcoin => "ssl://electrum.blockstream.info:50002",
			_ => {
				return Err(std::io::Error::new(
					std::io::ErrorKind::InvalidInput,
					"Network not supported",
				))
			}
		}
		.to_string();
		let client = Client::new(&url).map_err(|e| {
			std::io::Error::new(
				std::io::ErrorKind::InvalidInput,
				format!("Failed to create electrum client: {}", e),
			)
		})?;
		let mut fees: HashMap<Target, AtomicU32> = HashMap::new();
		fees.insert(Target::Background, AtomicU32::new(MIN_FEERATE));
		fees.insert(Target::Normal, AtomicU32::new(2_000));
		fees.insert(Target::HighPriority, AtomicU32::new(5_000));
		let client = Self {
			electrum_client: Arc::new(client),
			url,
			fees: Arc::new(fees),
			handle: handle.clone(),
		};
		ElectrumClient::poll_for_fee_estimates(
			client.fees.clone(),
			client.electrum_client.clone(),
			handle,
		);
		Ok(client)
	}

	fn poll_for_fee_estimates(
		fees: Arc<HashMap<Target, AtomicU32>>, client: Arc<Client>, handle: tokio::runtime::Handle,
	) {
		handle.spawn(async move {
			loop {
				// LDK expects feerates in satoshis per KW but Electrum gives fees
				// denominated in satoshis per vB.

				let background_estimate = {
					let feerate = client.estimate_fee(5).map_or(MIN_FEERATE, |f| f as u32 * 250);
					if feerate > MIN_FEERATE {
						feerate
					} else {
						MIN_FEERATE
					}
				};

				let normal_estimate = {
					let feerate = client.estimate_fee(2).map_or(2_000, |f| f as u32 * 250);
					if feerate > MIN_FEERATE {
						feerate
					} else {
						MIN_FEERATE
					}
				};

				let high_prio_estimate = {
					let feerate = client.estimate_fee(1).map_or(5_000, |f| f as u32 * 250);
					if feerate > MIN_FEERATE {
						feerate
					} else {
						MIN_FEERATE
					}
				};

				fees.get(&Target::Background)
					.unwrap()
					.store(background_estimate, Ordering::Release);
				fees.get(&Target::Normal).unwrap().store(normal_estimate, Ordering::Release);
				fees.get(&Target::HighPriority)
					.unwrap()
					.store(high_prio_estimate, Ordering::Release);
				tokio::time::sleep(Duration::from_secs(60)).await;
			}
		});
	}
}

impl FeeEstimator for ElectrumClient {
	fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
		match confirmation_target {
			ConfirmationTarget::Background => {
				self.fees.get(&Target::Background).unwrap().load(Ordering::Acquire)
			}
			ConfirmationTarget::Normal => {
				self.fees.get(&Target::Normal).unwrap().load(Ordering::Acquire)
			}
			ConfirmationTarget::HighPriority => {
				self.fees.get(&Target::HighPriority).unwrap().load(Ordering::Acquire)
			}
		}
	}
}

impl BroadcasterInterface for ElectrumClient {
	fn broadcast_transaction(&self, tx: &Transaction) {
		let tx = tx.clone();
		let electrum_client = self.electrum_client.clone();

		self.handle.spawn(async move {
			// This may error due to RL calling `broadcast_transaction` with the same transaction
			// multiple times, but the error is safe to ignore.
			match electrum_client.transaction_broadcast(&tx) {
				Ok(_) => {}
				Err(e) => {
					let err_str = e.to_string();
					if !err_str.contains("Transaction already in block chain")
						&& !err_str.contains("Inputs missing or spent")
						&& !err_str.contains("bad-txns-inputs-missingorspent")
						&& !err_str.contains("txn-mempool-conflict")
						&& !err_str.contains("non-BIP68-final")
						&& !err_str.contains("insufficient fee, rejecting replacement ")
					{
						panic!("{}", e);
					}
				}
			}
		});
	}
}
