// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! This module contains traits for LDK to access UTXOs to check gossip data is correct.
//!
//! When lightning nodes gossip channel information, they resist DoS attacks by checking that each
//! channel matches a UTXO on-chain, requiring at least some marginal on-chain transacting in
//! order to announce a channel. This module handles that checking.

use bitcoin::{BlockHash, TxOut};
use bitcoin::hashes::hex::ToHex;

use crate::ln::chan_utils::make_funding_redeemscript_from_slices;
use crate::ln::msgs::{self, LightningError, ErrorAction};
use crate::routing::gossip::{NetworkGraph, NodeId};
use crate::util::logger::{Level, Logger};
use crate::util::ser::Writeable;

use crate::prelude::*;

use alloc::sync::{Arc, Weak};
use crate::sync::Mutex;
use core::ops::Deref;

/// An error when accessing the chain via [`UtxoLookup`].
#[derive(Clone, Debug)]
pub enum UtxoLookupError {
	/// The requested chain is unknown.
	UnknownChain,

	/// The requested transaction doesn't exist or hasn't confirmed.
	UnknownTx,
}

/// The result of a [`UtxoLookup::get_utxo`] call. A call may resolve either synchronously,
/// returning the `Sync` variant, or asynchronously, returning an [`UtxoFuture`] in the `Async`
/// variant.
pub enum UtxoResult {
	/// A result which was resolved synchronously. It either includes a [`TxOut`] for the output
	/// requested or a [`UtxoLookupError`].
	Sync(Result<TxOut, UtxoLookupError>),
	/// A result which will be resolved asynchronously. It includes a [`UtxoFuture`], a `clone` of
	/// which you must keep locally and call [`UtxoFuture::resolve`] on once the lookup completes.
	///
	/// Note that in order to avoid runaway memory usage, the number of parallel checks is limited,
	/// but only fairly loosely. Because a pending checks block all message processing, leaving
	/// checks pending for an extended time may cause DoS of other functions. It is recommended you
	/// keep a tight timeout on lookups, on the order of a few seconds.
	Async(UtxoFuture),
}

/// The `UtxoLookup` trait defines behavior for accessing on-chain UTXOs.
pub trait UtxoLookup {
	/// Returns the transaction output of a funding transaction encoded by [`short_channel_id`].
	/// Returns an error if `genesis_hash` is for a different chain or if such a transaction output
	/// is unknown.
	///
	/// [`short_channel_id`]: https://github.com/lightning/bolts/blob/master/07-routing-gossip.md#definition-of-short_channel_id
	fn get_utxo(&self, genesis_hash: &BlockHash, short_channel_id: u64) -> UtxoResult;
}

enum ChannelAnnouncement {
	Full(msgs::ChannelAnnouncement),
	Unsigned(msgs::UnsignedChannelAnnouncement),
}

struct UtxoMessages {
	complete: Option<Result<TxOut, UtxoLookupError>>,
	channel_announce: Option<ChannelAnnouncement>,
}

/// Represents a future resolution of a [`UtxoLookup::get_utxo`] query resolving async.
///
/// See [`UtxoResult::Async`] and [`UtxoFuture::resolve`] for more info.
#[derive(Clone)]
pub struct UtxoFuture {
	state: Arc<Mutex<UtxoMessages>>,
}

/// A trivial implementation of [`UtxoLookup`] which is used to call back into the network graph
/// once we have a concrete resolution of a request.
struct UtxoResolver(Result<TxOut, UtxoLookupError>);
impl UtxoLookup for UtxoResolver {
	fn get_utxo(&self, _genesis_hash: &BlockHash, _short_channel_id: u64) -> UtxoResult {
		UtxoResult::Sync(self.0.clone())
	}
}

impl UtxoFuture {
	/// Builds a new future for later resolution.
	pub fn new() -> Self {
		Self { state: Arc::new(Mutex::new(UtxoMessages {
			complete: None,
			channel_announce: None,
		}))}
	}

	/// Resolves this future against the given `graph` and with the given `result`.
	pub fn resolve<L: Deref>(&self, graph: &NetworkGraph<L>, result: Result<TxOut, UtxoLookupError>)
	where L::Target: Logger {
		let announcement = {
			let mut pending_checks = graph.pending_checks.internal.lock().unwrap();
			let mut async_messages = self.state.lock().unwrap();

			if async_messages.channel_announce.is_none() {
				// We raced returning to `check_channel_announcement` which hasn't updated
				// `channel_announce` yet. That's okay, we can set the `complete` field which it will
				// check once it gets control again.
				async_messages.complete = Some(result);
				return;
			}
			let announcement_msg = match async_messages.channel_announce.as_ref().unwrap() {
				ChannelAnnouncement::Full(signed_msg) => &signed_msg.contents,
				ChannelAnnouncement::Unsigned(msg) => &msg,
			};

			pending_checks.lookup_completed(announcement_msg, &Arc::downgrade(&self.state));

			async_messages.channel_announce.take().unwrap()
		};

		// Now that we've updated our internal state, pass the pending messages back through the
		// network graph with a different `UtxoLookup` which will resolve immediately.
		// Note that we ignore errors as we don't disconnect peers anyway, so there's nothing to do
		// with them.
		let resolver = UtxoResolver(result);
		match announcement {
			ChannelAnnouncement::Full(signed_msg) => {
				let _ = graph.update_channel_from_announcement(&signed_msg, &Some(&resolver));
			},
			ChannelAnnouncement::Unsigned(msg) => {
				let _ = graph.update_channel_from_unsigned_announcement(&msg, &Some(&resolver));
			},
		}
	}
}

struct PendingChecksContext {
	channels: HashMap<u64, Weak<Mutex<UtxoMessages>>>,
}

impl PendingChecksContext {
	fn lookup_completed(&mut self,
		msg: &msgs::UnsignedChannelAnnouncement, completed_state: &Weak<Mutex<UtxoMessages>>
	) {
		if let hash_map::Entry::Occupied(e) = self.channels.entry(msg.short_channel_id) {
			if Weak::ptr_eq(e.get(), &completed_state) {
				e.remove();
			}
		}
	}
}

/// A set of messages which are pending UTXO lookups for processing.
pub(super) struct PendingChecks {
	internal: Mutex<PendingChecksContext>,
}

impl PendingChecks {
	pub(super) fn new() -> Self {
		PendingChecks { internal: Mutex::new(PendingChecksContext {
			channels: HashMap::new(),
		}) }
	}

	fn check_replace_previous_entry(msg: &msgs::UnsignedChannelAnnouncement,
		full_msg: Option<&msgs::ChannelAnnouncement>, replacement: Option<Weak<Mutex<UtxoMessages>>>,
		pending_channels: &mut HashMap<u64, Weak<Mutex<UtxoMessages>>>
	) -> Result<(), msgs::LightningError> {
		match pending_channels.entry(msg.short_channel_id) {
			hash_map::Entry::Occupied(mut e) => {
				// There's already a pending lookup for the given SCID. Check if the messages
				// are the same and, if so, return immediately (don't bother spawning another
				// lookup if we haven't gotten that far yet).
				match Weak::upgrade(&e.get()) {
					Some(pending_msgs) => {
						let pending_matches = match &pending_msgs.lock().unwrap().channel_announce {
							Some(ChannelAnnouncement::Full(pending_msg)) => Some(pending_msg) == full_msg,
							Some(ChannelAnnouncement::Unsigned(pending_msg)) => pending_msg == msg,
							None => {
								// This shouldn't actually be reachable. We set the
								// `channel_announce` field under the same lock as setting the
								// channel map entry. Still, we can just treat it as
								// non-matching and let the new request fly.
								debug_assert!(false);
								false
							},
						};
						if pending_matches {
							return Err(LightningError {
								err: "Channel announcement is already being checked".to_owned(),
								action: ErrorAction::IgnoreDuplicateGossip,
							});
						} else {
							// The earlier lookup is a different message. If we have another
							// request in-flight now replace the original.
							// Note that in the replace case whether to replace is somewhat
							// arbitrary - both results will be handled, we're just updating the
							// value that will be compared to future lookups with the same SCID.
							if let Some(item) = replacement {
								*e.get_mut() = item;
							}
						}
					},
					None => {
						// The earlier lookup already resolved. We can't be sure its the same
						// so just remove/replace it and move on.
						if let Some(item) = replacement {
							*e.get_mut() = item;
						} else { e.remove(); }
					},
				}
			},
			hash_map::Entry::Vacant(v) => {
				if let Some(item) = replacement { v.insert(item); }
			},
		}
		Ok(())
	}

	pub(super) fn check_channel_announcement<U: Deref>(&self,
		utxo_lookup: &Option<U>, msg: &msgs::UnsignedChannelAnnouncement,
		full_msg: Option<&msgs::ChannelAnnouncement>
	) -> Result<Option<u64>, msgs::LightningError> where U::Target: UtxoLookup {
		let handle_result = |res| {
			match res {
				Ok(TxOut { value, script_pubkey }) => {
					let expected_script =
						make_funding_redeemscript_from_slices(msg.bitcoin_key_1.as_slice(), msg.bitcoin_key_2.as_slice()).to_v0_p2wsh();
					if script_pubkey != expected_script {
						return Err(LightningError{
							err: format!("Channel announcement key ({}) didn't match on-chain script ({})",
								expected_script.to_hex(), script_pubkey.to_hex()),
							action: ErrorAction::IgnoreError
						});
					}
					Ok(Some(value))
				},
				Err(UtxoLookupError::UnknownChain) => {
					Err(LightningError {
						err: format!("Channel announced on an unknown chain ({})",
							msg.chain_hash.encode().to_hex()),
						action: ErrorAction::IgnoreError
					})
				},
				Err(UtxoLookupError::UnknownTx) => {
					Err(LightningError {
						err: "Channel announced without corresponding UTXO entry".to_owned(),
						action: ErrorAction::IgnoreError
					})
				},
			}
		};

		Self::check_replace_previous_entry(msg, full_msg, None,
			&mut self.internal.lock().unwrap().channels)?;

		match utxo_lookup {
			&None => {
				// Tentatively accept, potentially exposing us to DoS attacks
				Ok(None)
			},
			&Some(ref utxo_lookup) => {
				match utxo_lookup.get_utxo(&msg.chain_hash, msg.short_channel_id) {
					UtxoResult::Sync(res) => handle_result(res),
					UtxoResult::Async(future) => {
						let mut pending_checks = self.internal.lock().unwrap();
						let mut async_messages = future.state.lock().unwrap();
						if let Some(res) = async_messages.complete.take() {
							// In the unlikely event the future resolved before we managed to get it,
							// handle the result in-line.
							handle_result(res)
						} else {
							Self::check_replace_previous_entry(msg, full_msg,
								Some(Arc::downgrade(&future.state)), &mut pending_checks.channels)?;
							async_messages.channel_announce = Some(
								if let Some(msg) = full_msg { ChannelAnnouncement::Full(msg.clone()) }
								else { ChannelAnnouncement::Unsigned(msg.clone()) });
							Err(LightningError {
								err: "Channel being checked async".to_owned(),
								action: ErrorAction::IgnoreAndLog(Level::Gossip),
							})
						}
					},
				}
			}
		}
	}
}
