// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use frame_support::storage;
use log::info;
use polkadot_primitives::BlakeTwo256;
use rand::prelude::*;
use sc_cli::{Error, Result};
use sc_client_api::{Backend as ClientBackend, StorageProvider, UsageProvider};
use sp_api::CallApiAt;
use sp_runtime::traits::{Block as BlockT, HashingFor, Header as HeaderT};
use sp_state_machine::{backend::AsTrieBackend, Backend};
use std::{fmt::Debug, sync::Arc, time::Instant};

use super::cmd::StorageCmd;
use crate::shared::{new_rng, BenchRecord};

impl StorageCmd {
	/// Benchmarks the time it takes to read a single Storage item.
	/// Uses the latest state that is available for the given client.
	pub(crate) fn bench_read<B, BA, C>(
		&self,
		client: Arc<C>,
		_shared_trie_cache: Option<sp_trie::cache::SharedTrieCache<HashingFor<B>>>,
	) -> Result<BenchRecord>
	where
		C: UsageProvider<B> + StorageProvider<B, BA> + CallApiAt<B>,
		B: BlockT + Debug,
		BA: ClientBackend<B>,
		<<B as BlockT>::Header as HeaderT>::Number: From<u32>,
	{
		let mut record = BenchRecord::default();
		let best_hash = client.usage_info().chain.best_hash;

		info!("Preparing keys from block {}", best_hash);
		// Load all keys and randomly shuffle them.
		let mut keys: Vec<_> = client.storage_keys(best_hash, None, None)?.collect();
		let (mut rng, _) = new_rng(None);
		keys.shuffle(&mut rng);

		let mut child_nodes = Vec::new();
		// Interesting part here:
		// Read all the keys in the database and measure the time it takes to access each.
		info!("Reading {} keys", keys.len());

		// Read using the same TrieBackend and recorder for up to `batch_size` keys.
		// This would allow us to measure the amortized cost of reading a key.
		let recorder = (!self.params.disable_pov_recorder).then(|| Default::default());
		let mut recorder_clone = recorder.clone();
		let mut state = client
			.state_at(best_hash)
			.map_err(|_err| Error::Input("State not found".into()))?;
		let mut as_trie_backend = state.as_trie_backend();
		let mut backend = sp_state_machine::TrieBackendBuilder::wrap(&as_trie_backend)
			.with_optional_recorder(recorder)
			.build();
		let mut read_in_batch = 0;
		let mut read_keys = Vec::new();
		for key in keys.as_slice() {
			match (self.params.include_child_trees, self.is_child_key(key.clone().0)) {
				(true, Some(info)) => {
					// child tree key
					for ck in client.child_storage_keys(best_hash, info.clone(), None, None)? {
						child_nodes.push((ck.clone(), info.clone()));
					}
				},
				_ => {
					// regular key
					let start = Instant::now();
					read_keys.push(key.clone());
					let v = backend
						.storage(key.0.as_ref())
						.expect("Checked above to exist")
						.ok_or("Value unexpectedly empty")?;
					// record.append(v.len(), start.elapsed())?;
				},
			}
			read_in_batch += 1;
			if read_in_batch >= self.params.batch_size {
				// Arguments to be passed to  validate_block
				let root = backend.root();
				let pov = recorder_clone.map(|r| r.drain_storage_proof());
				info!(
					"POV: len {:?} {:?}",
					pov.as_ref().map(|p| p.len()),
					pov.clone().map(|p| p.encoded_compact_size::<HashingFor<B>>(*root))
				);

				// Stuff to execute in validate block.
				if let Some(storage_proof) = pov {
					let compact = storage_proof.into_compact_proof::<HashingFor<B>>(*root).unwrap();
					let db = match compact.to_memory_db::<HashingFor<B>>(Some(root)) {
						Ok((db, _)) => db,
						Err(_) => panic!("Compact proof decoding failure."),
					};

					let mut recorder =
					cumulus_pallet_parachain_system::validate_block::trie_recorder::SizeOnlyRecorderProvider::<HashingFor<B>>::new();
					let cache_provider =
						cumulus_pallet_parachain_system::validate_block::trie_cache::CacheProvider::<
							HashingFor<B>,
						>::new();
					// We use the storage root of the `parent_head` to ensure that it is the correct
					// root. This is already being done above while creating the in-memory db,
					// but let's be paranoid!!
					let in_memory_backend = sp_state_machine::TrieBackendBuilder::new_with_cache(
						db,
						*root,
						cache_provider,
					)
					.with_recorder(recorder.clone())
					.build();

					for test_key in &read_keys {
						let start = Instant::now();

						let v = in_memory_backend
							.storage(test_key.0.as_ref())
							.expect("Checked above to exist")
							.ok_or("Value unexpectedly empty")?;
						record.append(v.len(), start.elapsed())?;
					}

					read_keys.clear();
				}

				// Using a new recorder for every read vs using the same for the entire batch
				// produces significant different results. Since in the real use case we use a
				// single recorder per block, simulate the same behavior by creating a new
				// recorder every batch size, so that the amortized cost of reading a key is
				// measured in conditions closer to the real world.
				let recorder = (!self.params.disable_pov_recorder).then(|| Default::default());
				recorder_clone = recorder.clone();
				info!("recorder {:} clone {:}", recorder.is_some(), recorder_clone.is_some());
				state = client
					.state_at(best_hash)
					.map_err(|_err| Error::Input("State not found".to_string()))?;
				as_trie_backend = state.as_trie_backend();
				backend = sp_state_machine::TrieBackendBuilder::wrap(&as_trie_backend)
					.with_optional_recorder(recorder)
					.build();

				read_in_batch = 0;
			}
		}

		if self.params.include_child_trees {
			child_nodes.shuffle(&mut rng);

			info!("Reading {} child keys", child_nodes.len());
			for (key, info) in child_nodes.as_slice() {
				let start = Instant::now();
				let v = backend
					.child_storage(info, key.0.as_ref())
					.expect("Checked above to exist")
					.ok_or("Value unexpectedly empty")?;
				// record.append(v.len(), start.elapsed())?;

				read_in_batch += 1;
				if read_in_batch >= self.params.batch_size {
					// Using a new recorder for every read vs using the same for the entire batch
					// produces significant different results. Since in the real use case we use a
					// single recorder per block, simulate the same behavior by creating a new
					// recorder every batch size, so that the amortized cost of reading a key is
					// measured in conditions closer to the real world.
					let recorder = (!self.params.disable_pov_recorder).then(|| Default::default());
					state = client
						.state_at(best_hash)
						.map_err(|_err| Error::Input("State not found".to_string()))?;
					as_trie_backend = state.as_trie_backend();
					backend = sp_state_machine::TrieBackendBuilder::wrap(&as_trie_backend)
						.with_optional_recorder(recorder)
						.build();
					read_in_batch = 0;
				}
			}
		}
		Ok(record)
	}
}
