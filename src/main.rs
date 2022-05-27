pub(crate) mod bitcoind_client;
mod cli;
mod convert;
mod disk;
mod hex_utils;
mod probe;

use crate::bitcoind_client::BitcoindClient;
use crate::cli::Attempt;
use crate::disk::FilesystemLogger;
use crate::disk::YourPersister;
use bitcoin::blockdata::constants::genesis_block;
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::consensus::encode;
use bitcoin::hash_types::BlockHash;
use bitcoin::network::constants::Network;
use bitcoin::secp256k1::Secp256k1;
use bitcoin_bech32::WitnessProgram;
use lightning::chain;
use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::chain::chainmonitor;
use lightning::chain::keysinterface::{InMemorySigner, KeysInterface, KeysManager, Recipient};
use lightning::chain::{BestBlock, Filter, Watch};
use lightning::ln::channelmanager::{self, PaymentId};
use lightning::ln::channelmanager::{
	ChainParameters, ChannelManagerReadArgs, SimpleArcChannelManager,
};
use lightning::ln::peer_handler::{IgnoringMessageHandler, MessageHandler, SimpleArcPeerManager};
use lightning::ln::{PaymentHash, PaymentPreimage, PaymentSecret};
use lightning::routing::network_graph::{NetGraphMsgHandler, NetworkGraph};
use lightning::routing::router::{Route, RouteHop};
use lightning::routing::scoring::ProbabilisticScorer;
use lightning::util::config::UserConfig;
use lightning::util::events::{Event, PaymentPurpose};
use lightning::util::logger::Logger;
use lightning::util::ser::ReadableArgs;
use lightning::{log_debug, log_given_level, log_info, log_internal, log_trace};
use lightning_background_processor::BackgroundProcessor;
use lightning_block_sync::init;
use lightning_block_sync::poll;
use lightning_block_sync::SpvClient;
use lightning_block_sync::UnboundedCache;
use lightning_invoice::payment;
use lightning_invoice::utils::DefaultRouter;
use lightning_net_tokio::SocketDescriptor;
use lightning_persister::FilesystemPersister;
use rand::{thread_rng, Rng};
use rusqlite::Connection;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Write;
use std::ops::Deref;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

pub(crate) enum HTLCStatus {
	Pending,
	Succeeded,
	Failed,
}

pub(crate) struct MillisatAmount(Option<u64>);

impl fmt::Display for MillisatAmount {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self.0 {
			Some(amt) => write!(f, "{}", amt),
			None => write!(f, "unknown"),
		}
	}
}

pub fn print_hops(route: &Vec<RouteHop>) {
	for (i, hop) in route.iter().enumerate() {
		println!("==== Hop {} ====", i);
		println!("PublicKey({})", hop.pubkey);
		println!("channel_id: {}", hop.short_channel_id);
	}
}

pub(crate) struct PaymentInfo {
	preimage: Option<PaymentPreimage>,
	secret: Option<PaymentSecret>,
	status: HTLCStatus,
	amt_msat: MillisatAmount,
}

pub(crate) type PaymentInfoStorage = Arc<Mutex<HashMap<PaymentHash, PaymentInfo>>>;

type ChainMonitor = chainmonitor::ChainMonitor<
	InMemorySigner,
	Arc<dyn Filter + Send + Sync>,
	Arc<BitcoindClient>,
	Arc<BitcoindClient>,
	Arc<FilesystemLogger>,
	Arc<YourPersister>,
	//Arc<FilesystemPersister>,
>;

pub(crate) type PeerManager = SimpleArcPeerManager<
	SocketDescriptor,
	ChainMonitor,
	BitcoindClient,
	BitcoindClient,
	dyn chain::Access + Send + Sync,
	FilesystemLogger,
>;

pub(crate) type ChannelManager =
	SimpleArcChannelManager<ChainMonitor, BitcoindClient, BitcoindClient, FilesystemLogger>;

pub(crate) type InvoicePayer<E> = payment::InvoicePayer<
	Arc<ChannelManager>,
	Router,
	Arc<Mutex<ProbabilisticScorer<Arc<NetworkGraph>>>>,
	Arc<FilesystemLogger>,
	E,
>;

type Router = DefaultRouter<Arc<NetworkGraph>, Arc<FilesystemLogger>>;

pub type PaymentState = Arc<Mutex<HashMap<PaymentId, Route>>>;
// pub(crate) type PaymentInfoStorage = Arc<Mutex<HashMap<PaymentHash, PaymentInfo>>>;

async fn handle_ldk_events(
	_pending_payment_state: PaymentState, channel_manager: Arc<ChannelManager>,
	bitcoind_client: Arc<BitcoindClient>, keys_manager: Arc<KeysManager>,
	inbound_payments: PaymentInfoStorage, outbound_payments: PaymentInfoStorage,
	pending_payments: PaymentInfoStorage, network: Network, event: &Event,
	db: Arc<Mutex<rusqlite::Connection>>, logger: Arc<FilesystemLogger>,
) {
	match event {
		Event::FundingGenerationReady {
			temporary_channel_id,
			channel_value_satoshis,
			output_script,
			..
		} => {
			// Construct the raw transaction with one output, that is paid the amount of the
			// channel.
			let addr = WitnessProgram::from_scriptpubkey(
				&output_script[..],
				match network {
					Network::Bitcoin => bitcoin_bech32::constants::Network::Bitcoin,
					Network::Testnet => bitcoin_bech32::constants::Network::Testnet,
					Network::Regtest => bitcoin_bech32::constants::Network::Regtest,
					Network::Signet => bitcoin_bech32::constants::Network::Signet,
				},
			)
			.expect("Lightning funding tx should always be to a SegWit output")
			.to_address();
			let mut outputs = vec![HashMap::with_capacity(1)];
			outputs[0].insert(addr, *channel_value_satoshis as f64 / 100_000_000.0);
			let raw_tx = bitcoind_client.create_raw_transaction(outputs).await;

			// Have your wallet put the inputs into the transaction such that the output is
			// satisfied.
			let funded_tx = bitcoind_client.fund_raw_transaction(raw_tx).await;

			// Sign the final funding transaction and broadcast it.
			let signed_tx = bitcoind_client.sign_raw_transaction_with_wallet(funded_tx.hex).await;
			assert_eq!(signed_tx.complete, true);
			let final_tx: Transaction =
				encode::deserialize(&hex_utils::to_vec(&signed_tx.hex).unwrap()).unwrap();
			// Give the funding transaction back to LDK for opening the channel.
			if channel_manager
				.funding_transaction_generated(&temporary_channel_id, final_tx)
				.is_err()
			{
				println!(
					"\nERROR: Channel went away before we could fund it. The peer disconnected or refused the channel.");
				print!("> ");
				io::stdout().flush().unwrap();
			}
		}
		Event::PaymentReceived { payment_hash, purpose, amt, .. } => {
			let mut payments = inbound_payments.lock().unwrap();
			let (payment_preimage, payment_secret) = match purpose {
				PaymentPurpose::InvoicePayment { payment_preimage, payment_secret, .. } => {
					(*payment_preimage, Some(*payment_secret))
				}
				PaymentPurpose::SpontaneousPayment(preimage) => (Some(*preimage), None),
			};
			let status = match channel_manager.claim_funds(payment_preimage.unwrap()) {
				true => {
					println!(
						"\nEVENT: received payment from payment hash {} of {} millisatoshis",
						hex_utils::hex_str(&payment_hash.0),
						amt
					);
					print!("> ");
					io::stdout().flush().unwrap();
					HTLCStatus::Succeeded
				}
				_ => HTLCStatus::Failed,
			};
			match payments.entry(*payment_hash) {
				Entry::Occupied(mut e) => {
					let payment = e.get_mut();
					payment.status = status;
					payment.preimage = payment_preimage;
					payment.secret = payment_secret;
				}
				Entry::Vacant(e) => {
					e.insert(PaymentInfo {
						preimage: payment_preimage,
						secret: payment_secret,
						status,
						amt_msat: MillisatAmount(Some(*amt)),
					});
				}
			}
		}
		Event::PaymentSent { payment_preimage, payment_hash, fee_paid_msat, .. } => {
			let mut payments = outbound_payments.lock().unwrap();
			for (hash, payment) in payments.iter_mut() {
				if *hash == *payment_hash {
					payment.preimage = Some(*payment_preimage);
					payment.status = HTLCStatus::Succeeded;
					println!(
						"\nEVENT: successfully sent payment of {} millisatoshis{} from \
								 payment hash {:?} with preimage {:?}",
						payment.amt_msat,
						if let Some(fee) = fee_paid_msat {
							format!(" (fee {} msat)", fee)
						} else {
							"".to_string()
						},
						hex_utils::hex_str(&payment_hash.0),
						hex_utils::hex_str(&payment_preimage.0)
					);
					print!("> ");
					io::stdout().flush().unwrap();
				}
			}
		}
		Event::OpenChannelRequest { .. } => {
			// Unreachable, we don't set manually_accept_inbound_channels
		}
		Event::PaymentPathSuccessful { .. } => {
			println!("PaymentPathSuccessful");
			print!("> ")
		}
		Event::PaymentPathFailed { path, error_code, .. } => {
			// get last hop for channel/node details
			let (last_hop, path) = path.split_last().unwrap();
			let chan_id = last_hop.short_channel_id;
			let guessed_node_pubkey = last_hop.pubkey;
			if path.len() < 1 {
				return;
			}
			let node_pubkey = path.last().unwrap().pubkey;

			if let Some(error_code) = error_code {
				let result = match error_code {
					0x400f => "incorrect_or_unknown_payment_details",
					0x100c => "fee_insufficient",
					0xc005 => "invalid_onion_hmac",
					0x400a => "unknown_next_peer",
					_ => "unknown",
				};

				log_info!(logger, "Result: {}", result);

				let attempt = Attempt {
					target_pubkey: node_pubkey.to_string(),
					guess_pubkey: guessed_node_pubkey.to_string(),
					channel_id: chan_id.to_string(),
					result: result.to_string(),
				};

				db.clone()
					.lock()
					.unwrap()
					.execute(
						"INSERT INTO attempt (
                                target_pubkey, guess_pubkey, channel_id, result)
                            VALUES (?1, ?2, ?3, ?4)",
						[
							&attempt.target_pubkey,
							&attempt.guess_pubkey,
							&attempt.channel_id,
							&attempt.result,
						],
					)
					.unwrap();
			}
		}
		Event::PaymentFailed { payment_hash, .. } => {
			log_debug!(logger,
				"\nEVENT: Failed to send payment to payment hash {:?}: exhausted payment retry attempts",
				hex_utils::hex_str(&payment_hash.0)
			);
			// print!("> ");
			// io::stdout().flush().unwrap();

			/*
			let mut payments = outbound_payments.lock().unwrap();
			if payments.contains_key(&payment_hash) {
				let payment = payments.get_mut(&payment_hash).unwrap();
				payment.status = HTLCStatus::Failed;
			}
						*/
			let mut pending_payments_lock = pending_payments.lock().unwrap();
			if pending_payments_lock.contains_key(&payment_hash) {
				let _payment = pending_payments_lock.remove(&payment_hash).unwrap();
				log_trace!(
					logger,
					"Removed payment hash {:?} from pending state",
					hex_utils::hex_str(&payment_hash.0)
				);
			}
		}
		Event::PaymentForwarded { fee_earned_msat, claim_from_onchain_tx } => {
			let from_onchain_str = if *claim_from_onchain_tx {
				"from onchain downstream claim"
			} else {
				"from HTLC fulfill message"
			};
			if let Some(fee_earned) = fee_earned_msat {
				println!(
					"\nEVENT: Forwarded payment, earning {} msat {}",
					fee_earned, from_onchain_str
				);
			} else {
				println!("\nEVENT: Forwarded payment, claiming onchain {}", from_onchain_str);
			}
			print!("> ");
			io::stdout().flush().unwrap();
		}
		Event::PendingHTLCsForwardable { time_forwardable } => {
			let forwarding_channel_manager = channel_manager.clone();
			let min = time_forwardable.as_millis() as u64;
			tokio::spawn(async move {
				let millis_to_sleep = thread_rng().gen_range(min, min * 5) as u64;
				tokio::time::sleep(Duration::from_millis(millis_to_sleep)).await;
				forwarding_channel_manager.process_pending_htlc_forwards();
			});
		}
		Event::SpendableOutputs { outputs } => {
			let destination_address = bitcoind_client.get_new_address().await;
			let output_descriptors = &outputs.iter().map(|a| a).collect::<Vec<_>>();
			let tx_feerate =
				bitcoind_client.get_est_sat_per_1000_weight(ConfirmationTarget::Normal);
			let spending_tx = keys_manager
				.spend_spendable_outputs(
					output_descriptors,
					Vec::new(),
					destination_address.script_pubkey(),
					tx_feerate,
					&Secp256k1::new(),
				)
				.unwrap();
			bitcoind_client.broadcast_transaction(&spending_tx);
		}
		Event::ChannelClosed { channel_id, reason, user_channel_id: _ } => {
			println!(
				"\nEVENT: Channel {} closed due to: {:?}",
				hex_utils::hex_str(channel_id),
				reason
			);
			print!("> ");
			io::stdout().flush().unwrap();
		}
		Event::DiscardFunding { .. } => {
			// A "real" node should probably "lock" the UTXOs spent in funding transactions until
			// the funding transaction either confirms, or this event is generated.
		}
	}
}

async fn start_ldk() {
	let args = match cli::parse_startup_args() {
		Ok(user_args) => user_args,
		Err(()) => return,
	};

	// Initialize the LDK data directory if necessary.
	let ldk_data_dir = format!("{}/.ldk", args.ldk_storage_dir_path);
	fs::create_dir_all(ldk_data_dir.clone()).unwrap();

	// Initialize our bitcoind client.
	let bitcoind_client = match BitcoindClient::new(
		args.bitcoind_rpc_host.clone(),
		args.bitcoind_rpc_port,
		args.bitcoind_rpc_username.clone(),
		args.bitcoind_rpc_password.clone(),
		tokio::runtime::Handle::current(),
	)
	.await
	{
		Ok(client) => Arc::new(client),
		Err(e) => {
			println!("Failed to connect to bitcoind client: {}", e);
			return;
		}
	};

	// Check that the bitcoind we've connected to is running the network we expect
	let bitcoind_chain = bitcoind_client.get_blockchain_info().await.chain;
	if bitcoind_chain
		!= match args.network {
			bitcoin::Network::Bitcoin => "main",
			bitcoin::Network::Testnet => "test",
			bitcoin::Network::Regtest => "regtest",
			bitcoin::Network::Signet => "signet",
		} {
		println!(
			"Chain argument ({}) didn't match bitcoind chain ({})",
			args.network, bitcoind_chain
		);
		return;
	}

	// ## Setup
	// Step 1: Initialize the FeeEstimator

	// BitcoindClient implements the FeeEstimator trait, so it'll act as our fee estimator.
	let fee_estimator = bitcoind_client.clone();

	// Step 2: Initialize the Logger
	let logger = Arc::new(FilesystemLogger::new(ldk_data_dir.clone()));

	// Step 3: Initialize the BroadcasterInterface

	// BitcoindClient implements the BroadcasterInterface trait, so it'll act as our transaction
	// broadcaster.
	let broadcaster = bitcoind_client.clone();

	// Step 4: Initialize Persist
	let real_persister = Arc::new(FilesystemPersister::new(ldk_data_dir.clone()));

	let persister = Arc::new(YourPersister::new(ldk_data_dir.clone()));

	// Step 5: Initialize the ChainMonitor
	let chain_monitor: Arc<ChainMonitor> = Arc::new(chainmonitor::ChainMonitor::new(
		None,
		broadcaster.clone(),
		logger.clone(),
		fee_estimator.clone(),
		persister.clone(),
	));

	// Step 6: Initialize the KeysManager

	// The key seed that we use to derive the node privkey (that corresponds to the node pubkey) and
	// other secret key material.
	let keys_seed_path = format!("{}/keys_seed", ldk_data_dir.clone());
	let keys_seed = if let Ok(seed) = fs::read(keys_seed_path.clone()) {
		assert_eq!(seed.len(), 32);
		let mut key = [0; 32];
		key.copy_from_slice(&seed);
		key
	} else {
		let mut key = [0; 32];
		thread_rng().fill_bytes(&mut key);
		match File::create(keys_seed_path.clone()) {
			Ok(mut f) => {
				f.write_all(&key).expect("Failed to write node keys seed to disk");
				f.sync_all().expect("Failed to sync node keys seed to disk");
			}
			Err(e) => {
				println!("ERROR: Unable to create keys seed file {}: {}", keys_seed_path, e);
				return;
			}
		}
		key
	};
	let cur = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();
	let keys_manager = Arc::new(KeysManager::new(&keys_seed, cur.as_secs(), cur.subsec_nanos()));

	// Step 7: Read ChannelMonitor state from disk
	let mut channelmonitors = real_persister.read_channelmonitors(keys_manager.clone()).unwrap();

	// Load channel monitor updates from disk as well
	let channelmonitorupdates = persister.read_channelmonitor_updates().unwrap();
	for (_, channel_monitor) in channelmonitors.iter_mut() {
		// which utxo is this channel monitoring for?
		let (channel_output, _) = channel_monitor.get_funding_txo();
		let channel_updates_res = channelmonitorupdates.get(&channel_output.txid);
		match channel_updates_res {
			Some(channel_updates) => {
				// if we found the channel monitor for this channel update,
				// apply in order
				let mut sorted_channel_updates = channel_updates.clone();
				sorted_channel_updates.sort_by(|a, b| a.update_id.cmp(&b.update_id));
				for channel_monitor_update in sorted_channel_updates.iter_mut() {
					println!(
						"applying update {} for {}",
						channel_monitor_update.update_id, channel_output.txid
					);

					match channel_monitor.update_monitor(
						channel_monitor_update,
						&broadcaster,
						&fee_estimator,
						&logger,
					) {
						Ok(_) => continue,
						Err(e) => {
							log_info!(logger, "{:?}", e);
							return;
						}
					}
				}
			}
			None => continue,
		}
	}

	// Step 8: Initialize the ChannelManager
	let mut user_config = UserConfig::default();
	user_config.peer_channel_config_limits.force_announced_channel_preference = false;
	let mut restarting_node = true;
	let (channel_manager_blockhash, channel_manager) = {
		if let Ok(mut f) = fs::File::open(format!("{}/manager", ldk_data_dir.clone())) {
			let mut channel_monitor_mut_references = Vec::new();
			for (_, channel_monitor) in channelmonitors.iter_mut() {
				channel_monitor_mut_references.push(channel_monitor);
			}
			let read_args = ChannelManagerReadArgs::new(
				keys_manager.clone(),
				fee_estimator.clone(),
				chain_monitor.clone(),
				broadcaster.clone(),
				logger.clone(),
				user_config,
				channel_monitor_mut_references,
			);
			<(BlockHash, ChannelManager)>::read(&mut f, read_args).unwrap()
		} else {
			// We're starting a fresh node.
			restarting_node = false;
			let getinfo_resp = bitcoind_client.get_blockchain_info().await;

			let chain_params = ChainParameters {
				network: args.network,
				best_block: BestBlock::new(
					getinfo_resp.latest_blockhash,
					getinfo_resp.latest_height as u32,
				),
			};
			let fresh_channel_manager = channelmanager::ChannelManager::new(
				fee_estimator.clone(),
				chain_monitor.clone(),
				broadcaster.clone(),
				logger.clone(),
				keys_manager.clone(),
				user_config,
				chain_params,
			);
			(getinfo_resp.latest_blockhash, fresh_channel_manager)
		}
	};

	// Step 9: Sync ChannelMonitors and ChannelManager to chain tip
	let mut chain_listener_channel_monitors = Vec::new();
	let mut cache = UnboundedCache::new();
	let mut chain_tip: Option<poll::ValidatedBlockHeader> = None;
	if restarting_node {
		let mut chain_listeners =
			vec![(channel_manager_blockhash, &channel_manager as &dyn chain::Listen)];

		for (blockhash, channel_monitor) in channelmonitors.drain(..) {
			let outpoint = channel_monitor.get_funding_txo().0;
			chain_listener_channel_monitors.push((
				blockhash,
				(channel_monitor, broadcaster.clone(), fee_estimator.clone(), logger.clone()),
				outpoint,
			));
		}

		for monitor_listener_info in chain_listener_channel_monitors.iter_mut() {
			chain_listeners
				.push((monitor_listener_info.0, &monitor_listener_info.1 as &dyn chain::Listen));
		}
		chain_tip = Some(
			init::synchronize_listeners(
				&mut bitcoind_client.deref(),
				args.network,
				&mut cache,
				chain_listeners,
			)
			.await
			.unwrap(),
		);
	}

	// Step 10: Give ChannelMonitors to ChainMonitor
	for item in chain_listener_channel_monitors.drain(..) {
		let channel_monitor = item.1 .0;
		let funding_outpoint = item.2;
		chain_monitor.watch_channel(funding_outpoint, channel_monitor).unwrap();
	}

	// Step 11: Optional: Initialize the NetGraphMsgHandler
	let genesis = genesis_block(args.network).header.block_hash();
	let network_graph_path = format!("{}/network_graph", ldk_data_dir.clone());
	let network_graph = Arc::new(disk::read_network(Path::new(&network_graph_path), genesis));
	let network_gossip = Arc::new(NetGraphMsgHandler::new(
		Arc::clone(&network_graph),
		None::<Arc<dyn chain::Access + Send + Sync>>,
		logger.clone(),
	));
	let network_graph_persist = Arc::clone(&network_graph);
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(Duration::from_secs(600));
		loop {
			interval.tick().await;
			if disk::persist_network(Path::new(&network_graph_path), &network_graph_persist)
				.is_err()
			{
				// Persistence errors here are non-fatal as we can just fetch the routing graph
				// again later, but they may indicate a disk error which could be fatal elsewhere.
				eprintln!(
					"Warning: Failed to persist network graph, check your disk and permissions"
				);
			}
		}
	});

	// Step 12: Initialize the PeerManager
	let channel_manager: Arc<ChannelManager> = Arc::new(channel_manager);
	let mut ephemeral_bytes = [0; 32];
	rand::thread_rng().fill_bytes(&mut ephemeral_bytes);
	let lightning_msg_handler = MessageHandler {
		chan_handler: channel_manager.clone(),
		route_handler: network_gossip.clone(),
	};
	let peer_manager: Arc<PeerManager> = Arc::new(PeerManager::new(
		lightning_msg_handler,
		keys_manager.get_node_secret(Recipient::Node).unwrap(),
		&ephemeral_bytes,
		logger.clone(),
		Arc::new(IgnoringMessageHandler {}),
	));

	// ## Running LDK
	// Step 13: Initialize networking

	let peer_manager_connection_handler = peer_manager.clone();
	let listening_port = args.ldk_peer_listening_port;
	tokio::spawn(async move {
		let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", listening_port))
			.await
			.expect("Failed to bind to listen port - is something else already listening on it?");
		loop {
			let peer_mgr = peer_manager_connection_handler.clone();
			let tcp_stream = listener.accept().await.unwrap().0;
			tokio::spawn(async move {
				lightning_net_tokio::setup_inbound(
					peer_mgr.clone(),
					tcp_stream.into_std().unwrap(),
				)
				.await;
			});
		}
	});

	// Step 14: Connect and Disconnect Blocks
	if chain_tip.is_none() {
		chain_tip =
			Some(init::validate_best_block_header(&mut bitcoind_client.deref()).await.unwrap());
	}
	let channel_manager_listener = channel_manager.clone();
	let chain_monitor_listener = chain_monitor.clone();
	let bitcoind_block_source = bitcoind_client.clone();
	let network = args.network;
	tokio::spawn(async move {
		let mut derefed = bitcoind_block_source.deref();
		let chain_poller = poll::ChainPoller::new(&mut derefed, network);
		let chain_listener = (chain_monitor_listener, channel_manager_listener);
		let mut spv_client =
			SpvClient::new(chain_tip.unwrap(), chain_poller, &mut cache, &chain_listener);
		loop {
			spv_client.poll_best_tip().await.unwrap();
			tokio::time::sleep(Duration::from_secs(1)).await;
		}
	});

	// Create DB
	let path = "./my_db.db3";
	let db_arc: Arc<Mutex<rusqlite::Connection>> =
		Arc::new(Mutex::new(Connection::open(&path).unwrap()));
	// let db = Connection::open(&path).unwrap();
	// println!("{}", db.is_autocommit());

	db_arc
		.clone()
		.lock()
		.unwrap()
		.execute(
			"CREATE TABLE if not exists attempt ( 
            target_pubkey TEXT,
            guess_pubkey TEXT,
            channel_id TEXT,
            result TEXT
            )",
			[], // empty list of parameters.
		)
		.unwrap();

	// TODO: read attempts
	// select target_pubkey, channel_id from attempt
	// should end up with a vec of attempts

	// Step 15: Handle LDK Events
	let channel_manager_event_listener = channel_manager.clone();
	let keys_manager_listener = keys_manager.clone();
	// TODO: persist payment info to disk
	let inbound_payments: PaymentInfoStorage = Arc::new(Mutex::new(HashMap::new()));
	let outbound_payments: PaymentInfoStorage = Arc::new(Mutex::new(HashMap::new()));
	let pending_payments: PaymentInfoStorage = Arc::new(Mutex::new(HashMap::new()));
	let inbound_pmts_for_events = inbound_payments.clone();
	let outbound_pmts_for_events = outbound_payments.clone();
	let pending_pmts_for_events = pending_payments.clone();
	let network = args.network;
	let bitcoind_rpc = bitcoind_client.clone();
	let handle = tokio::runtime::Handle::current();
	let payment_state: PaymentState = Arc::new(Mutex::new(HashMap::new()));
	let payment_state_for_events = payment_state.clone();
	let event_logger = logger.clone();
	let db_arc_copy = db_arc.clone();
	let event_handler = move |event: &Event| {
		handle.block_on(handle_ldk_events(
			payment_state_for_events.clone(),
			channel_manager_event_listener.clone(),
			bitcoind_rpc.clone(),
			keys_manager_listener.clone(),
			inbound_pmts_for_events.clone(),
			outbound_pmts_for_events.clone(),
			pending_pmts_for_events.clone(),
			network,
			event,
			db_arc.clone(),
			event_logger.clone(),
		));
	};

	// Step 16: Initialize routing ProbabilisticScorer
	let scorer_path = format!("{}/prob_scorer", ldk_data_dir.clone());
	let scorer = Arc::new(Mutex::new(disk::read_scorer(
		Path::new(&scorer_path),
		Arc::clone(&network_graph),
	)));
	let scorer_persist = Arc::clone(&scorer);
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(Duration::from_secs(600));
		loop {
			interval.tick().await;
			if disk::persist_scorer(Path::new(&scorer_path), &scorer_persist.lock().unwrap())
				.is_err()
			{
				// Persistence errors here are non-fatal as channels will be re-scored as payments
				// fail, but they may indicate a disk error which could be fatal elsewhere.
				eprintln!("Warning: Failed to persist scorer, check your disk and permissions");
			}
		}
	});

	// Step 17: Create InvoicePayer
	let router = DefaultRouter::new(network_graph.clone(), logger.clone());
	let invoice_payer = Arc::new(InvoicePayer::new(
		channel_manager.clone(),
		router,
		scorer.clone(),
		logger.clone(),
		event_handler,
		payment::RetryAttempts(0),
	));

	// Step 18: Persist ChannelManager
	let data_dir = ldk_data_dir.clone();
	let persist_channel_manager_callback =
		move |node: &ChannelManager| FilesystemPersister::persist_manager(data_dir.clone(), &*node);

	// Step 19: Background Processing
	let background_processor = BackgroundProcessor::start(
		persist_channel_manager_callback,
		invoice_payer.clone(),
		chain_monitor.clone(),
		channel_manager.clone(),
		Some(network_gossip.clone()),
		peer_manager.clone(),
		logger.clone(),
	);

	// Regularly reconnect to channel peers.
	let connect_cm = Arc::clone(&channel_manager);
	let connect_pm = Arc::clone(&peer_manager);
	let peer_data_path = format!("{}/channel_peer_data", ldk_data_dir.clone());
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(Duration::from_secs(1));
		loop {
			interval.tick().await;
			match disk::read_channel_peer_data(Path::new(&peer_data_path)) {
				Ok(info) => {
					let peers = connect_pm.get_peer_node_ids();
					for node_id in connect_cm
						.list_channels()
						.iter()
						.map(|chan| chan.counterparty.node_id)
						.filter(|id| !peers.contains(id))
					{
						for (pubkey, peer_addr) in info.iter() {
							if *pubkey == node_id {
								let _ = cli::do_connect_peer(
									*pubkey,
									peer_addr.clone(),
									Arc::clone(&connect_pm),
								)
								.await;
							}
						}
					}
				}
				Err(e) => println!("ERROR: errored reading channel peer info from disk: {:?}", e),
			}
		}
	});

	// Regularly broadcast our node_announcement. This is only required (or possible) if we have
	// some public channels, and is only useful if we have public listen address(es) to announce.
	// In a production environment, this should occur only after the announcement of new channels
	// to avoid churn in the global network graph.
	let chan_manager = Arc::clone(&channel_manager);
	let network = args.network;
	if !args.ldk_announced_listen_addr.is_empty() {
		tokio::spawn(async move {
			let mut interval = tokio::time::interval(Duration::from_secs(60));
			loop {
				interval.tick().await;
				chan_manager.broadcast_node_announcement(
					[0; 3],
					args.ldk_announced_node_name,
					args.ldk_announced_listen_addr.clone(),
				);
			}
		});
	}

	// Start the CLI.
	cli::poll_for_user_input(
		payment_state.clone(),
		invoice_payer.clone(),
		peer_manager.clone(),
		channel_manager.clone(),
		keys_manager.clone(),
		inbound_payments,
		outbound_payments,
		pending_payments,
		ldk_data_dir.clone(),
		network,
		network_graph.clone(),
		logger.clone(),
		scorer.clone(),
		db_arc_copy.clone(),
	)
	.await;

	// Stop the background processor.
	background_processor.stop().unwrap();
}

#[tokio::main]
pub async fn main() {
	start_ldk().await;
}
