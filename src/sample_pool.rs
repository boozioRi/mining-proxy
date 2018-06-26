// Simple sample pool server that implements most of what you need, note that it does NOT currently
// check for duplicate shares...

extern crate base64;
extern crate bitcoin;
extern crate bytes;
extern crate crypto;
extern crate futures;
extern crate hyper;
extern crate tokio;
extern crate tokio_io;
extern crate tokio_codec;
extern crate secp256k1;
extern crate serde_json;

mod msg_framing;
use msg_framing::*;

mod utils;

mod rpc_client;
use rpc_client::*;

use bitcoin::blockdata::transaction::Transaction;
use bitcoin::blockdata::block::BlockHeader;
use bitcoin::network::serialize::BitcoinHash;
use bitcoin::network;
use bitcoin::util::address::Address;
use bitcoin::util::privkey;
use bitcoin::util::hash::Sha256dHash;

use bytes::BufMut;

use crypto::digest::Digest;
use crypto::sha2::Sha256;

use futures::{future,Stream,Sink,Future};
use futures::sync::mpsc;

use tokio::{net, timer};

use secp256k1::key::PublicKey;
use secp256k1::Secp256k1;

use std::{cmp, env, io, mem};
use std::str::FromStr;
use std::sync::{Arc, Weak, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH, Duration, Instant};
use std::collections::{hash_map, HashMap};

// These are useful to plug in business logic into:
fn check_user_auth(user_id: &Vec<u8>, user_auth: &Vec<u8>) -> bool {
	println!("User {} authed with pass {}", String::from_utf8_lossy(user_id), String::from_utf8_lossy(user_auth));
	true
}

fn share_submitted(user_id: &Vec<u8>, user_tag_1: &Vec<u8>, value: u64) {
	println!("Got valid share with value {} from \"{}\" from machine identified as \"{}\"", value, String::from_utf8_lossy(user_id), String::from_utf8_lossy(user_tag_1));
}

fn weak_block_submitted(user_id: &Vec<u8>, user_tag_1: &Vec<u8>, value: u64, _header: &BlockHeader, txn: &Vec<Vec<u8>>, _extra_block_data: &Vec<u8>) {
	println!("Got valid weak block with value {} from \"{}\" with {} txn from machine identified as \"{}\"", value, String::from_utf8_lossy(user_id), txn.len(), String::from_utf8_lossy(user_tag_1));
}

// Note that because leading_0s_to_target gets the *largest* number with the given number of
// leading 0s, we offset by 1 higher than we really want (this limits stratum false-positives
// in the naive difficulty converter).

const MIN_TARGET_LEADING_0S: u8 = 47; // Diff ~16384
const WEAK_BLOCK_RATIO_0S: u8 = 8; // 2**8x harder to mine weak blocks
const MAX_USER_SHARES_PER_30_SEC: usize = 30;
const MIN_USER_SHARES_PER_30_SEC: usize = 1;

// Dont change anything below...
const MAX_TARGET_LEADING_0S: u8 = 71 - WEAK_BLOCK_RATIO_0S; // Roughly network diff/16 at the time of writing, should be more than sufficiently high for any use-case

struct PerUserClientRef {
	send_stream: mpsc::Sender<PoolMessage>,
	client_id: u64,
	user_id: Vec<u8>,
	min_target: u8,
	cur_target: AtomicUsize,
	accepted_shares: AtomicUsize,
}

fn main() {
	println!("USAGE: sample-pool --listen_bind=IP:port --auth_key=base58privkey --payout_address=addr [--server_id=up_to_36_byte_string_for_coinbase] --bitcoind_rpc_path=user:pass@host:port");
	println!("--listen_bind - the address to bind to");
	println!("--auth_key - the auth key to use to authenticate to clients");
	println!("--payout_address - the Bitcoin address on which to receive payment");
	println!("--bitcoind_rpc_path - the bitcoind RPC server for checking weak block validity");
	println!("                      and header submission");

	let mut listen_bind = None;
	let mut auth_key = None;
	let mut payout_addr = None;
	let mut server_id = None;
	let mut rpc_path = None;

	for arg in env::args().skip(1) {
		if arg.starts_with("--listen_bind") {
			if listen_bind.is_some() {
				println!("Cannot specify multiple listen binds");
				return;
			}
			listen_bind = Some(match arg.split_at(14).1.parse() {
				Ok(sockaddr) => sockaddr,
				Err(_) =>{
					println!("Failed to parse listen_bind into a socket address");
					return;
				}
			});
		} else if arg.starts_with("--auth_key") {
			if auth_key.is_some() {
				println!("Cannot specify multiple auth keys");
				return;
			}
			auth_key = Some(match privkey::Privkey::from_str(arg.split_at(11).1) {
				Ok(privkey) => {
					if !privkey.compressed {
						println!("Private key must represent a compressed key!");
						return;
					}
					privkey.key
				},
				Err(_) =>{
					println!("Failed to parse auth_key into a private key");
					return;
				}
			});
		} else if arg.starts_with("--payout_address") {
			if payout_addr.is_some() {
				println!("Cannot specify multiple payout addresses");
				return;
			}
			//TODO: check network magic byte? We're allowed to mine on any net, though...
			payout_addr = Some(match Address::from_str(arg.split_at(17).1) {
				Ok(addr) => addr.script_pubkey(),
				Err(_) => {
					println!("Failed to parse payout_address into a Bitcoin address");
					return;
				}
			});
		} else if arg.starts_with("--server_id") {
			if server_id.is_some() {
				println!("Cannot specify multiple server IDs");
				return;
			}
			server_id = Some(arg.split_at(12).1.to_string());
			if server_id.as_ref().unwrap().len() > 36 {
				println!("server_id cannot be longer than 36 bytes");
				return;
			}
		} else if arg.starts_with("--bitcoind_rpc_path") {
			if rpc_path.is_some() {
				println!("Cannot specify multiple bitcoinds");
				return;
			}
			rpc_path = Some(arg.split_at(20).1.to_string());
		} else {
			println!("Unkown arg: {}", arg);
			return;
		}
	}

	if listen_bind.is_none() || auth_key.is_none() || payout_addr.is_none() || rpc_path.is_none() {
		println!("Need to specify all but server_id parameters");
		return;
	}

	let rpc_client = {
		let path = rpc_path.unwrap();
		let path_parts: Vec<&str> = path.split('@').collect();
		if path_parts.len() != 2 {
			println!("Bad RPC URL provided");
			return;
		}
		RPCClient::new(path_parts[0], path_parts[1])
	};

	{
		println!("Checking validity of RPC URL");
		let mut thread_rt = tokio::runtime::current_thread::Runtime::new().unwrap();
		match thread_rt.block_on(rpc_client.make_rpc_call("getnetworkinfo")) {
			Ok(v) => v,
			Err(_) => { panic!("Bad RPC URL"); },
		};
		println!("Success! Starting up...");
	}

	let mut rt = tokio::runtime::Builder::new().build().unwrap();
	rt.spawn(futures::lazy(move || -> Result<(), ()> {
		match net::TcpListener::bind(&listen_bind.unwrap()) {
			Ok(listener) => {
				let mut max_client_id = 0;
				let mut users: Arc<Mutex<Vec<Weak<PerUserClientRef>>>> = Arc::new(Mutex::new(Vec::new()));

				let users_timer_ref = users.clone();
				tokio::spawn(timer::Interval::new(Instant::now() + Duration::from_secs(10), Duration::from_secs(30)).for_each(move |_| {
					let mut users_lock = users_timer_ref.lock().unwrap();
					let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
					let timestamp = time.as_secs() * 1000 + time.subsec_nanos() as u64 / 1_000_000;

					users_lock.retain(|weak_user| {
						match weak_user.upgrade() {
							Some(user) => {
								let shares = user.accepted_shares.swap(0, Ordering::AcqRel);
								let cur_target = user.cur_target.load(Ordering::Acquire) as u8;
								println!("In last 30 seconds, user with id {} submitted {} shares with {} leading zeros", user.client_id, shares, cur_target);

								let new_target = if shares > MAX_USER_SHARES_PER_30_SEC && cur_target < MAX_TARGET_LEADING_0S {
									cur_target + 1
								} else if shares < MIN_USER_SHARES_PER_30_SEC && cur_target > MIN_TARGET_LEADING_0S && cur_target > user.min_target {
									cur_target - 1
								} else {
									cur_target
								};
								if new_target != cur_target {
									let _ = user.send_stream.clone().start_send(PoolMessage::ShareDifficulty {
										difficulty: PoolDifficulty {
											user_id: user.user_id.clone(),
											timestamp,
											share_target: utils::leading_0s_to_target(new_target as u8),
											weak_block_target: utils::leading_0s_to_target(new_target + WEAK_BLOCK_RATIO_0S),
										},
									});
									user.cur_target.store(new_target as usize, Ordering::Release);
								}

								true
							},
							None => { false }
						}
					});

					future::result(Ok(()))
				}).then(|_| {
					future::result(Ok(()))
				}));

				tokio::spawn(listener.incoming().for_each(move |sock| {
					sock.set_nodelay(true).unwrap();

					let (tx, rx) = tokio_codec::Framed::new(sock, PoolMsgFramer::new()).split();
					let (mut send_sink, send_stream) = mpsc::channel(5);
					tokio::spawn(tx.send_all(send_stream.map_err(|_| -> io::Error {
						panic!("mpsc streams cant generate errors!");
					})).then(|_| {
						future::result(Ok(()))
					}));

					let secp_ctx = Secp256k1::new();
					macro_rules! sign_message {
						($msg: expr, $msg_type: expr) => {
							{
								let mut msg_signed = bytes::BytesMut::with_capacity(1000);
								msg_signed.put_u8($msg_type);
								$msg.encode_unsigned(&mut msg_signed);
								let hash = {
									let mut sha = Sha256::new();
									sha.input(&msg_signed[..]);
									let mut h = [0; 32];
									sha.result(&mut h);
									secp256k1::Message::from_slice(&h).unwrap()
								};

								secp_ctx.sign(&hash, &auth_key.unwrap()).unwrap()
							}
						}
					}

					let users_ref = users.clone();
					let server_id_vec = match server_id {
						Some(ref id) => id.as_bytes().to_vec(),
						None => vec![],
					};
					let payout_addr_clone = payout_addr.as_ref().unwrap().clone();

					let mut connection_clients = HashMap::new();
					let mut client_ids = HashMap::new();

					let mut client_version = None;
					let mut last_weak_block = None;

					tokio::spawn(rx.for_each(move |msg| {
						macro_rules! send_response {
							($msg: expr) => {
								match send_sink.start_send($msg) {
									Ok(_) => {},
									Err(_) => return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)))
								}
							}
						}

						macro_rules! reject_share {
							($share_msg: expr, $reason: expr) => {
								{
									send_response!(PoolMessage::ShareRejected {
										user_tag_1: $share_msg.user_tag_1.clone(),
										user_tag_2: $share_msg.user_tag_2.clone(),
										reason: $reason,
									});
								}
							}
						}

						macro_rules! check_coinbase_tx {
							($coinbase_tx: expr, $share_msg: expr, $extra_fail_cmd: expr) => {
								{
									if $coinbase_tx.input.len() != 1 || $coinbase_tx.output.len() < 1 {
										reject_share!($share_msg, ShareRejectedReason::BadPayoutInfo);
										$extra_fail_cmd;
										return future::result(Ok(()));
									}

									let mut our_payout = 0;
									for (idx, out) in $coinbase_tx.output.iter().enumerate() {
										if idx == 0 {
											our_payout = out.value;
											if out.script_pubkey != payout_addr_clone {
												reject_share!($share_msg, ShareRejectedReason::BadPayoutInfo);
												$extra_fail_cmd;
												return future::result(Ok(()));
											}
										} else if out.value != 0 {
											reject_share!($share_msg, ShareRejectedReason::BadPayoutInfo);
											$extra_fail_cmd;
											return future::result(Ok(()));
										}
									}

									let coinbase = &$coinbase_tx.input[0].script_sig[..];
									if coinbase.len() < 8 {
										reject_share!($share_msg, ShareRejectedReason::BadPayoutInfo);
										$extra_fail_cmd;
										return future::result(Ok(()));
									}

									let client_id = if let Some(client_id) = client_ids.get(&utils::slice_to_le64(&coinbase[coinbase.len() - 8..])) {
										client_id
									} else {
										reject_share!($share_msg, ShareRejectedReason::BadPayoutInfo);
										$extra_fail_cmd;
										return future::result(Ok(()));
									};

									(our_payout, client_id)
								}
							}
						}

						macro_rules! share_received {
							($user: expr, $cur_target: expr, $share_msg: expr) => {
								{
									send_response!(PoolMessage::ShareAccepted {
										user_tag_1: $share_msg.user_tag_1.clone(),
										user_tag_2: $share_msg.user_tag_2.clone(),
									});
									let accepted_shares = $user.accepted_shares.fetch_add(1, Ordering::AcqRel);
									if accepted_shares + 1 > MAX_USER_SHARES_PER_30_SEC && $cur_target < MAX_TARGET_LEADING_0S {
										let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
										let timestamp = time.as_secs() * 1000 + time.subsec_nanos() as u64 / 1_000_000;

										send_response!(PoolMessage::ShareDifficulty {
											difficulty: PoolDifficulty {
												user_id: $user.user_id.clone(),
												timestamp,
												share_target: utils::leading_0s_to_target($cur_target + 1),
												weak_block_target: utils::leading_0s_to_target($cur_target + 1 + WEAK_BLOCK_RATIO_0S),
											},
										});
										$user.cur_target.store(($cur_target + 1) as usize, Ordering::Release);
										$user.accepted_shares.store((accepted_shares + 1) / 2, Ordering::Release);
									}
								}
							}
						}

						match msg {
							PoolMessage::ProtocolSupport { max_version, min_version, flags } => {
								if client_version.is_some() {
									println!("Client sent duplicative ProtocolSupport");
									return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
								}
								if min_version > 1 || max_version < 1 {
									return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
								}
								if flags != 0 {
									println!("Client requested unknown flags {}", flags);
								}
								client_version = Some(1);
								send_response!(PoolMessage::ProtocolVersion {
									selected_version: 1,
									flags: 0,
									auth_key: PublicKey::from_secret_key(&secp_ctx, &auth_key.unwrap()).unwrap(),
								});

								let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
								let timestamp = time.as_secs() * 1000 + time.subsec_nanos() as u64 / 1_000_000;
								let payout_info = PoolPayoutInfo {
									timestamp,
									remaining_payout: payout_addr_clone.clone(),
									appended_outputs: vec![],
								};
								send_response!(PoolMessage::PayoutInfo {
									signature: sign_message!(payout_info, 13),
									payout_info,
								});
							},
							PoolMessage::ProtocolVersion { .. } => {
								println!("Got ProtocolVersion?");
								return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
							},
							PoolMessage::UserAuth { info } => {
								let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
								let timestamp = time.as_secs() * 1000 + time.subsec_nanos() as u64 / 1_000_000;

								if client_version.is_none() {
									println!("Client sent UserAuth before ProtocolSupport");
									return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
								}
								if {
									let connection_entry = connection_clients.entry(info.user_id.clone());
									if let hash_map::Entry::Occupied(_) = connection_entry {
										println!("Got a UserAuth for an already-registered client, disconencting proxy!");
										return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
									}
									if check_user_auth(&info.user_id, &info.user_auth) {
										let client_id = max_client_id;
										max_client_id += 1;

										println!("Got new user with id {} for client id {}", utils::bytes_to_hex(&info.user_id), client_id);

										let mut client_coinbase_postfix = server_id_vec.clone();
										client_coinbase_postfix.extend_from_slice(&utils::le64_to_array(client_id));

										let initial_target = cmp::min(MAX_TARGET_LEADING_0S, cmp::max(MIN_TARGET_LEADING_0S, cmp::max(utils::count_leading_zeros(&info.suggested_target) + 1, utils::count_leading_zeros(&info.minimum_target) + 1)));
										let user = Arc::new(PerUserClientRef {
											send_stream: send_sink.clone(),
											client_id,
											user_id: info.user_id.clone(),
											min_target: utils::count_leading_zeros(&info.minimum_target) + 1,
											cur_target: AtomicUsize::new(initial_target as usize),
											accepted_shares: AtomicUsize::new(0),
										});
										client_ids.insert(client_id, info.user_id.clone());
										connection_entry.or_insert(user.clone());
										users_ref.lock().unwrap().push(Arc::downgrade(&user));

										let user_payout_info = PoolUserPayoutInfo {
											user_id: info.user_id.clone(),
											timestamp,
											coinbase_postfix: client_coinbase_postfix.clone(),
										};
										send_response!(PoolMessage::AcceptUserAuth {
											signature: sign_message!(user_payout_info, 15),
											info: user_payout_info,
										});

										send_response!(PoolMessage::ShareDifficulty {
											difficulty: PoolDifficulty {
												user_id: info.user_id.clone(),
												timestamp,
												share_target: utils::leading_0s_to_target(initial_target),
												weak_block_target: utils::leading_0s_to_target(initial_target + WEAK_BLOCK_RATIO_0S),
											},
										});
										false
									} else { true }
								} {
									if connection_clients.is_empty() {
										return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
									} else {
										send_response!(PoolMessage::RejectUserAuth { user_id: info.user_id });
										return future::result(Ok(()));
									}
								}
							},
							PoolMessage::PayoutInfo { .. } => {
								println!("Got PayoutInfo?");
								return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
							},
							PoolMessage::AcceptUserAuth { .. } => {
								println!("Got AcceptUserAuth?");
								return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
							},
							PoolMessage::RejectUserAuth { .. } => {
								println!("Got RejectUserAuth?");
								return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
							},
							PoolMessage::DropUser { user_id } => {
								if let Some(client_ref) = connection_clients.remove(&user_id) {
									client_ids.remove(&client_ref.client_id);
								} else {
									println!("Got DropUser for an un-authed user");
									return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
								}
							},
							PoolMessage::ShareDifficulty { .. } => {
								println!("Got ShareDifficulty?");
								return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
							},
							PoolMessage::Share { ref share } => {
								if client_version.is_none() || connection_clients.is_empty() {
									println!("Client sent Share before version/id handshake");
									return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
								}

								let (our_payout, client_id) = check_coinbase_tx!(share.coinbase_tx, share, {});

								let mut merkle_lhs = [0; 32];
								merkle_lhs.copy_from_slice(&share.coinbase_tx.txid()[..]);
								let mut sha = Sha256::new();
								for rhs in share.merkle_rhss.iter() {
									sha.reset();
									sha.input(&merkle_lhs);
									sha.input(&rhs[..]);
									sha.result(&mut merkle_lhs);
									sha.reset();
									sha.input(&merkle_lhs);
									sha.result(&mut merkle_lhs);
								}

								let block_hash = BlockHeader {
									version: share.header_version,
									prev_blockhash: Sha256dHash::from(&share.header_prevblock[..]),
									merkle_root: Sha256dHash::from(&merkle_lhs[..]),
									time: share.header_time,
									bits: share.header_nbits,
									nonce: share.header_nonce,
								}.bitcoin_hash();
								let leading_zeros = utils::count_leading_zeros(&block_hash[..]);

								let client = connection_clients.get(client_id).unwrap();
								let client_target = client.cur_target.load(Ordering::Acquire) as u8;

								if leading_zeros >= client_target + WEAK_BLOCK_RATIO_0S {
									println!("Got share that met weak block target, ignored as we'll check the weak block");
								} else if leading_zeros >= client_target {
									share_submitted(client_id, &share.user_tag_1, our_payout);
									share_received!(client, client_target, share);
								} else {
									reject_share!(share, ShareRejectedReason::BadHash);
								}
							},
							PoolMessage::WeakBlock { mut sketch } => {
								if client_version.is_none() || connection_clients.is_empty() {
									println!("Client sent Share before version/id handshake");
									return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
								}
								if sketch.txn.len() < 1 {
									println!("Client sent WeakBlock with no transactions");
									return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
								}

								let (coinbase_txid, (our_payout, client_id)) = match &sketch.txn[0] {
									&WeakBlockAction::TakeTx { .. } => {
										reject_share!(sketch, ShareRejectedReason::BadWork);
										send_response!(PoolMessage::WeakBlockStateReset {});
										return future::result(Ok(()));
									},
									&WeakBlockAction::NewTx { ref tx } => {
										let tx_deser_attempt: Result<Transaction, _> = network::serialize::deserialize(tx);
										match tx_deser_attempt {
											Ok(tx_deser) => {
												(tx_deser.txid(), check_coinbase_tx!(tx_deser, sketch, send_response!(PoolMessage::WeakBlockStateReset {})))
											},
											Err(_) => {
												reject_share!(sketch, ShareRejectedReason::BadPayoutInfo);
												send_response!(PoolMessage::WeakBlockStateReset {});
												return future::result(Ok(()));
											}
										}
									},
								};

								let mut merkle_lhs = [0; 32];
								merkle_lhs.copy_from_slice(&coinbase_txid[..]);
								let mut sha = Sha256::new();
								for rhs in sketch.merkle_rhss.iter() {
									sha.reset();
									sha.input(&merkle_lhs);
									sha.input(&rhs[..]);
									sha.result(&mut merkle_lhs);
									sha.reset();
									sha.input(&merkle_lhs);
									sha.result(&mut merkle_lhs);
								}

								let header = BlockHeader {
									version: sketch.header_version,
									prev_blockhash: Sha256dHash::from(&sketch.header_prevblock[..]),
									merkle_root: Sha256dHash::from(&merkle_lhs[..]),
									time: sketch.header_time,
									bits: sketch.header_nbits,
									nonce: sketch.header_nonce,
								};

								let mut new_txn = Vec::with_capacity(sketch.txn.len());
								{
									let mut dummy_last_weak_block: Vec<Vec<u8>> = Vec::new();
									let last_weak_ref = if last_weak_block.is_some() {
										last_weak_block.as_mut().unwrap()
									} else { &mut dummy_last_weak_block };

									for action in sketch.txn.drain(..) {
										match action {
											WeakBlockAction::TakeTx { n } => {
												if n as usize >= last_weak_ref.len() {
													reject_share!(sketch, ShareRejectedReason::BadWork);
													send_response!(PoolMessage::WeakBlockStateReset {});
													return future::result(Ok(()));
												}
												new_txn.push(Vec::new());
												mem::swap(&mut last_weak_ref[n as usize], &mut new_txn.last_mut().unwrap());
											},
											WeakBlockAction::NewTx { tx } => {
												new_txn.push(tx);
											}
										}
									}
								}

								let block_hash = header.bitcoin_hash();
								let leading_zeros = utils::count_leading_zeros(&block_hash[..]);

								let client = connection_clients.get(client_id).unwrap();
								let client_target = client.cur_target.load(Ordering::Acquire) as u8;

								if leading_zeros >= client_target + WEAK_BLOCK_RATIO_0S {
									weak_block_submitted(client_id, &sketch.user_tag_1, our_payout, &header, &new_txn, &sketch.extra_block_data);
									share_received!(client, client_target, sketch);
								} else {
									reject_share!(sketch, ShareRejectedReason::BadHash);
								}

								last_weak_block = Some(new_txn);
							},
							PoolMessage::WeakBlockStateReset { } => {
								println!("Got WeakBlockStateReset?");
								return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
							},
							PoolMessage::ShareAccepted { .. } => {
								println!("Got ShareAccepted?");
								return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
							},
							PoolMessage::ShareRejected { .. } => {
								println!("Got ShareRejected?");
								return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
							},
							PoolMessage::NewPoolServer { .. } => {
								println!("Got NewPoolServer?");
								return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
							},
							PoolMessage::VendorMessage { .. } => {
								println!("Got vendor message");
								return future::result(Ok(()));
							},
						}
						future::result(Ok(()))
					}).then(|_| {
						future::result(Ok(()))
					}));

					future::result(Ok(()))
				}).then(|_| {
					future::result(Ok(()))
				}));
			},
			Err(_) => {
				println!("Failed to bind to listen bind addr");
				return Ok(())
			}
		};

		Ok(())
	}));
	rt.shutdown_on_idle().wait().unwrap();
}
