// Copyright 2017 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Statement routing and validation statement table router implementation.
//!
//! During the attestation process, validators exchange statements on validity and availability
//! of parachain candidates.
//!
//! The `Router` in this file hooks into the underlying network to fulfill
//! the `TableRouter` trait from `polkadot-validation`, which is expected to call into a shared statement table
//! and dispatch evaluation work as necessary when new statements come in.

use sr_primitives::traits::{ProvideRuntimeApi, BlakeTwo256, Hash as HashT};
use polkadot_validation::{
	SharedTable, TableRouter, SignedStatement, GenericStatement, ParachainWork, Incoming,
	Validated, Outgoing,
};
use polkadot_primitives::{Block, Hash, SessionKey};
use polkadot_primitives::parachain::{
	BlockData, Extrinsic, CandidateReceipt, ParachainHost, Id as ParaId, Message
};

use codec::{Encode, Decode};
use futures::prelude::*;
use parking_lot::Mutex;

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;

use validation::{self, SessionDataFetcher, NetworkService, Executor};

type IngressPairRef<'a> = (ParaId, &'a [Message]);

fn attestation_topic(parent_hash: Hash) -> Hash {
	let mut v = parent_hash.as_ref().to_vec();
	v.extend(b"attestations");

	BlakeTwo256::hash(&v[..])
}

/// Table routing implementation.
pub struct Router<P, E, N: NetworkService, T> {
	table: Arc<SharedTable>,
	attestation_topic: Hash,
	fetcher: SessionDataFetcher<P, E, N, T>,
	deferred_statements: Arc<Mutex<DeferredStatements>>,
}

impl<P, E, N: NetworkService, T> Router<P, E, N, T> {
	pub(crate) fn new(
		table: Arc<SharedTable>,
		fetcher: SessionDataFetcher<P, E, N, T>,
	) -> Self {
		let parent_hash = fetcher.parent_hash();
		Router {
			table,
			attestation_topic: attestation_topic(parent_hash),
			deferred_statements: Arc::new(Mutex::new(DeferredStatements::new())),
			fetcher,
		}
	}

	/// Return a future of checked messages. These should be imported into the router
	/// with `import_statement`.
	pub(crate) fn checked_statements(&self) -> impl Stream<Item=SignedStatement,Error=()> {
		// spin up a task in the background that processes all incoming statements
		// TODO: propagate statements more intelligently.
		// https://github.com/paritytech/polkadot/issues/158
		let parent_hash = self.parent_hash();
		self.network().gossip_messages_for(self.attestation_topic)
			.filter_map(|msg| {
				debug!(target: "validation", "Processing statement for live validation session");
				SignedStatement::decode(&mut &msg[..])
			})
			.filter(move |statement| ::polkadot_validation::check_statement(
				&statement.statement,
				&statement.signature,
				statement.sender,
				&parent_hash,
			))
	}

	fn parent_hash(&self) -> Hash {
		self.fetcher.parent_hash()
	}

	fn network(&self) -> &Arc<N> {
		self.fetcher.network()
	}
}

impl<P, E: Clone, N: NetworkService, T: Clone> Clone for Router<P, E, N, T> {
	fn clone(&self) -> Self {
		Router {
			table: self.table.clone(),
			fetcher: self.fetcher.clone(),
			attestation_topic: self.attestation_topic.clone(),
			deferred_statements: self.deferred_statements.clone(),
		}
	}
}

impl<P: ProvideRuntimeApi + Send + Sync + 'static, E, N, T> Router<P, E, N, T> where
	P::Api: ParachainHost<Block>,
	N: NetworkService,
	T: Clone + Executor + Send + 'static,
	E: Future<Item=(),Error=()> + Clone + Send + 'static,
{
	/// Import a statement whose signature has been checked already.
	pub(crate) fn import_statement(&self, statement: SignedStatement) {
		trace!(target: "p_net", "importing validation statement {:?}", statement.statement);

		// defer any statements for which we haven't imported the candidate yet
		let c_hash = {
			let candidate_data = match statement.statement {
				GenericStatement::Candidate(ref c) => Some(c.hash()),
				GenericStatement::Valid(ref hash)
					| GenericStatement::Invalid(ref hash)
					=> self.table.with_candidate(hash, |c| c.map(|_| *hash)),
			};
			match candidate_data {
				Some(x) => x,
				None => {
					self.deferred_statements.lock().push(statement);
					return;
				}
			}
		};

		// import all statements pending on this candidate
		let (mut statements, _traces) = if let GenericStatement::Candidate(_) = statement.statement {
			self.deferred_statements.lock().get_deferred(&c_hash)
		} else {
			(Vec::new(), Vec::new())
		};

		// prepend the candidate statement.
		debug!(target: "validation", "Importing statements about candidate {:?}", c_hash);
		statements.insert(0, statement);
		let producers: Vec<_> = self.table.import_remote_statements(
			self,
			statements.iter().cloned(),
		);
		// dispatch future work as necessary.
		for (producer, statement) in producers.into_iter().zip(statements) {
			self.fetcher.knowledge().lock().note_statement(statement.sender, &statement.statement);

			if let Some(work) = producer.map(|p| self.create_work(c_hash, p)) {
				trace!(target: "validation", "driving statement work to completion");
				let work = work.select2(self.fetcher.exit().clone()).then(|_| Ok(()));
				self.fetcher.executor().spawn(work);
			}
		}
	}

	/// Broadcast outgoing messages to peers.
	pub(crate) fn broadcast_egress(&self, outgoing: Outgoing) {
		use slice_group_by::LinearGroupBy;

		let mut group_messages = Vec::new();
		for egress in outgoing {
			let source = egress.from;
			let messages = egress.messages.outgoing_messages;

			let groups = LinearGroupBy::new(&messages, |a, b| a.target == b.target);
			for group in groups {
				let target = match group.get(0) {
					Some(msg) => msg.target,
					None => continue, // skip empty.
				};

				group_messages.clear(); // reuse allocation from previous iterations.
				group_messages.extend(group.iter().map(|msg| msg.data.clone()).map(Message));

				debug!(target: "valdidation", "Circulating messages from {:?} to {:?} at {}",
					source, target, self.parent_hash());

				// this is the ingress from source to target, with given messages.
				let target_incoming =
					validation::incoming_message_topic(self.parent_hash(), target);
				let ingress_for: IngressPairRef = (source, &group_messages[..]);

				self.network().gossip_message(target_incoming, ingress_for.encode());
			}
		}
	}

	fn create_work<D>(&self, candidate_hash: Hash, producer: ParachainWork<D>)
		-> impl Future<Item=(),Error=()> + Send + 'static
		where
		D: Future<Item=(BlockData, Incoming),Error=io::Error> + Send + 'static,
	{
		let table = self.table.clone();
		let network = self.network().clone();
		let knowledge = self.fetcher.knowledge().clone();
		let attestation_topic = self.attestation_topic.clone();

		producer.prime(self.fetcher.api().clone())
			.map(move |validated| {
				// store the data before broadcasting statements, so other peers can fetch.
				knowledge.lock().note_candidate(
					candidate_hash,
					Some(validated.block_data().clone()),
					validated.extrinsic().cloned(),
				);

				// propagate the statement.
				// consider something more targeted than gossip in the future.
				let signed = table.import_validated(validated);
				network.gossip_message(attestation_topic, signed.encode());
			})
			.map_err(|e| debug!(target: "p_net", "Failed to produce statements: {:?}", e))
	}
}

impl<P: ProvideRuntimeApi + Send, E, N, T> TableRouter for Router<P, E, N, T> where
	P::Api: ParachainHost<Block>,
	N: NetworkService,
	T: Clone + Executor + Send + 'static,
	E: Future<Item=(),Error=()> + Clone + Send + 'static,
{
	type Error = io::Error;
	type FetchCandidate = validation::BlockDataReceiver;
	type FetchIncoming = validation::IncomingReceiver;

	fn local_candidate(&self, receipt: CandidateReceipt, block_data: BlockData, extrinsic: Extrinsic) {
		// produce a signed statement
		let hash = receipt.hash();
		let validated = Validated::collated_local(receipt, block_data.clone(), extrinsic.clone());
		let statement = self.table.import_validated(validated);

		// give to network to make available.
		self.fetcher.knowledge().lock().note_candidate(hash, Some(block_data), Some(extrinsic));
		self.network().gossip_message(self.attestation_topic, statement.encode());
	}

	fn fetch_block_data(&self, candidate: &CandidateReceipt) -> Self::FetchCandidate {
		self.fetcher.fetch_block_data(candidate)
	}

	fn fetch_incoming(&self, parachain: ParaId) -> Self::FetchIncoming {
		self.fetcher.fetch_incoming(parachain)
	}
}

impl<P, E, N: NetworkService, T> Drop for Router<P, E, N, T> {
	fn drop(&mut self) {
		self.fetcher.network().drop_gossip(self.attestation_topic);
	}
}

// A unique trace for valid statements issued by a validator.
#[derive(Hash, PartialEq, Eq, Clone, Debug)]
enum StatementTrace {
	Valid(SessionKey, Hash),
	Invalid(SessionKey, Hash),
}

// helper for deferring statements whose associated candidate is unknown.
struct DeferredStatements {
	deferred: HashMap<Hash, Vec<SignedStatement>>,
	known_traces: HashSet<StatementTrace>,
}

impl DeferredStatements {
	fn new() -> Self {
		DeferredStatements {
			deferred: HashMap::new(),
			known_traces: HashSet::new(),
		}
	}

	fn push(&mut self, statement: SignedStatement) {
		let (hash, trace) = match statement.statement {
			GenericStatement::Candidate(_) => return,
			GenericStatement::Valid(hash) => (hash, StatementTrace::Valid(statement.sender, hash)),
			GenericStatement::Invalid(hash) => (hash, StatementTrace::Invalid(statement.sender, hash)),
		};

		if self.known_traces.insert(trace) {
			self.deferred.entry(hash).or_insert_with(Vec::new).push(statement);
		}
	}

	fn get_deferred(&mut self, hash: &Hash) -> (Vec<SignedStatement>, Vec<StatementTrace>) {
		match self.deferred.remove(hash) {
			None => (Vec::new(), Vec::new()),
			Some(deferred) => {
				let mut traces = Vec::new();
				for statement in deferred.iter() {
					let trace = match statement.statement {
						GenericStatement::Candidate(_) => continue,
						GenericStatement::Valid(hash) => StatementTrace::Valid(statement.sender, hash),
						GenericStatement::Invalid(hash) => StatementTrace::Invalid(statement.sender, hash),
					};

					self.known_traces.remove(&trace);
					traces.push(trace);
				}

				(deferred, traces)
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use substrate_primitives::H512;

	#[test]
	fn deferred_statements_works() {
		let mut deferred = DeferredStatements::new();
		let hash = [1; 32].into();
		let sig = H512::from([2; 64]).into();
		let sender = [255; 32].into();

		let statement = SignedStatement {
			statement: GenericStatement::Valid(hash),
			sender,
			signature: sig,
		};

		// pre-push.
		{
			let (signed, traces) = deferred.get_deferred(&hash);
			assert!(signed.is_empty());
			assert!(traces.is_empty());
		}

		deferred.push(statement.clone());
		deferred.push(statement.clone());

		// draining: second push should have been ignored.
		{
			let (signed, traces) = deferred.get_deferred(&hash);
			assert_eq!(signed.len(), 1);

			assert_eq!(traces.len(), 1);
			assert_eq!(signed[0].clone(), statement);
			assert_eq!(traces[0].clone(), StatementTrace::Valid(sender, hash));
		}

		// after draining
		{
			let (signed, traces) = deferred.get_deferred(&hash);
			assert!(signed.is_empty());
			assert!(traces.is_empty());
		}
	}
}
