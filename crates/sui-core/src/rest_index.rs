// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::authority::authority_store_tables::LiveObject;
use crate::authority::AuthorityStore;
use crate::checkpoints::CheckpointStore;
use crate::state_accumulator::AccumulatorStore;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;
use sui_rest_api::CheckpointData;
use sui_types::base_types::MoveObjectType;
use sui_types::base_types::ObjectID;
use sui_types::base_types::SequenceNumber;
use sui_types::base_types::SuiAddress;
use sui_types::digests::TransactionDigest;
use sui_types::messages_checkpoint::CheckpointContents;
use sui_types::object::Owner;
use sui_types::storage::error::Error as StorageError;
use tracing::{debug, info};
use typed_store::rocks::{DBMap, MetricConf};
use typed_store::traits::Map;
use typed_store::traits::{TableSummary, TypedStoreDebug};
use typed_store::TypedStoreError;
use typed_store_derive::DBMapUtils;

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct OwnerIndexKey {
    owner: SuiAddress,
    object_id: ObjectID,
}

impl OwnerIndexKey {
    fn new(owner: SuiAddress, object_id: ObjectID) -> Self {
        Self { owner, object_id }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OwnerIndexInfo {
    // object_id of the object is a part of the Key
    pub version: SequenceNumber,
    pub type_: MoveObjectType,
}

impl OwnerIndexInfo {
    pub fn new(object: &Object) -> Self {
        Self {
            version: object.version(),
            type_: object.type_().expect("packages cannot be owned").to_owned(),
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize, Eq, PartialEq, Debug)]
pub struct TransactionInfo {
    checkpoint: u64,
}

/// RocksDB tables for the RestIndexStore
///
/// NOTE: Authors and Reviewers before adding any new tables ensure that they are either:
/// - bounded in size by the live object set
/// - are prune-able and have corresponding logic in the `prune` function
#[derive(DBMapUtils)]
struct IndexStoreTables {
    /// An index of extra metadata for Transactions.
    ///
    /// Only contains entries for transactions which have yet to be pruned from the main database.
    transactions: DBMap<TransactionDigest, TransactionInfo>,

    /// An index of object ownership.
    ///
    /// Allows an efficient iterator to list all objects currently owned by a specific user
    /// account.
    owner: DBMap<OwnerIndexKey, OwnerIndexInfo>,
    // NOTE: Authors and Reviewers before adding any new tables ensure that they are either:
    // - bounded in size by the live object set
    // - are prune-able and have corresponding logic in the `prune` function
}

impl IndexStoreTables {
    fn is_empty(&self) -> bool {
        self.transactions.is_empty()
    }

    fn init(
        &mut self,
        authority_store: &AuthorityStore,
        checkpoint_store: &CheckpointStore,
    ) -> Result<(), StorageError> {
        info!("Initializing REST indexes");

        // Iterate through available, executed checkpoints that have yet to be pruned
        // to initialize checkpoint and transaction based indexes.
        if let Some(highest_executed_checkpint) =
            checkpoint_store.get_highest_executed_checkpoint_seq_number()?
        {
            let lowest_available_checkpoint =
                checkpoint_store.get_highest_pruned_checkpoint_seq_number()?;

            let mut batch = self.transactions.batch();

            for seq in lowest_available_checkpoint..=highest_executed_checkpint {
                let checkpoint = checkpoint_store
                    .get_checkpoint_by_sequence_number(seq)?
                    .ok_or_else(|| StorageError::missing(format!("missing checkpoint {seq}")))?;
                let contents = checkpoint_store
                    .get_checkpoint_contents(&checkpoint.content_digest)?
                    .ok_or_else(|| StorageError::missing(format!("missing checkpoint {seq}")))?;

                let info = TransactionInfo {
                    checkpoint: checkpoint.sequence_number,
                };

                batch.insert_batch(
                    &self.transactions,
                    contents.iter().map(|digests| (digests.transaction, info)),
                )?;
            }

            batch.write()?;
        }

        // Iterate through live object set to initialize object-based indexes
        for object in authority_store
            .iter_live_object_set(false)
            .filter_map(LiveObject::to_normal)
        {
            let Owner::AddressOwner(owner) = object.owner else {
                continue;
            };

            let mut batch = self.owner.batch();

            // Owner Index
            let owner_key = OwnerIndexKey::new(owner, object.id());
            let owner_info = OwnerIndexInfo::new(&object);

            batch.insert_batch(&self.owner, [(owner_key, owner_info)])?;

            batch.write()?;
        }

        info!("Finished initializing REST indexes");

        Ok(())
    }

    /// Prune data from this Index
    fn prune(
        &self,
        checkpoint_contents_to_prune: &[CheckpointContents],
    ) -> Result<(), TypedStoreError> {
        let mut batch = self.transactions.batch();

        let transactions_to_prune = checkpoint_contents_to_prune
            .iter()
            .flat_map(|contents| contents.iter().map(|digests| digests.transaction));

        batch.delete_batch(&self.transactions, transactions_to_prune)?;

        batch.write()
    }

    /// Index a Checkpoint
    fn index_checkpoint(&self, checkpoint: &CheckpointData) -> Result<(), TypedStoreError> {
        debug!(
            checkpoint = checkpoint.checkpoint_summary.sequence_number,
            "indexing checkpoint"
        );

        let mut batch = self.transactions.batch();

        // transactions index
        {
            let info = TransactionInfo {
                checkpoint: checkpoint.checkpoint_summary.sequence_number,
            };

            batch.insert_batch(
                &self.transactions,
                checkpoint
                    .checkpoint_contents
                    .iter()
                    .map(|digests| (digests.transaction, info)),
            )?;
        }

        // owner index
        {
            for tx in &checkpoint.transactions {
                // determine changes from removed objects
                for removed_object in tx.removed_objects() {
                    match removed_object.owner() {
                        Owner::AddressOwner(address) => {
                            let owner_key = OwnerIndexKey::new(*address, removed_object.id());
                            batch.delete_batch(&self.owner, [owner_key])?;
                        }
                        Owner::ObjectOwner(_) | Owner::Shared { .. } | Owner::Immutable => {}
                    }
                }

                // determine changes from changed objects
                for (object, old_object) in tx.changed_objects() {
                    if let Some(old_object) = old_object {
                        if old_object.owner() != object.owner() {
                            match old_object.owner() {
                                Owner::AddressOwner(address) => {
                                    let owner_key = OwnerIndexKey::new(*address, old_object.id());
                                    batch.delete_batch(&self.owner, [owner_key])?;
                                }

                                Owner::ObjectOwner(_) | Owner::Shared { .. } | Owner::Immutable => {
                                }
                            }
                        }
                    }

                    match object.owner() {
                        Owner::AddressOwner(owner) => {
                            let owner_key = OwnerIndexKey::new(*owner, object.id());
                            let owner_info = OwnerIndexInfo::new(object);
                            batch.insert_batch(&self.owner, [(owner_key, owner_info)])?;
                        }
                        Owner::ObjectOwner(_) | Owner::Shared { .. } | Owner::Immutable => {}
                    }
                }
            }
        }

        batch.write()?;

        debug!(
            checkpoint = checkpoint.checkpoint_summary.sequence_number,
            "finished indexing checkpoint"
        );
        Ok(())
    }
}

pub struct RestIndexStore {
    tables: IndexStoreTables,
}

impl RestIndexStore {
    pub fn new(
        path: PathBuf,
        authority_store: &AuthorityStore,
        checkpoint_store: &CheckpointStore,
    ) -> Self {
        let mut tables = IndexStoreTables::open_tables_read_write(
            path,
            MetricConf::new("rest-index"),
            None,
            None,
        );

        // If the index tables are empty then we need to populate them
        if tables.is_empty() {
            tables.init(authority_store, checkpoint_store).unwrap();
        }

        Self { tables }
    }

    pub fn new_without_init(path: PathBuf) -> Self {
        let tables = IndexStoreTables::open_tables_read_write(
            path,
            MetricConf::new("rest-index"),
            None,
            None,
        );

        Self { tables }
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    pub fn prune(
        &self,
        checkpoint_contents_to_prune: &[CheckpointContents],
    ) -> Result<(), TypedStoreError> {
        self.tables.prune(checkpoint_contents_to_prune)
    }

    pub fn index_checkpoint(&self, checkpoint: &CheckpointData) -> Result<(), TypedStoreError> {
        self.tables.index_checkpoint(checkpoint)
    }
}