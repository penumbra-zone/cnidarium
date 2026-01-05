//! JMT pruning support for substores.
//!
//! This module provides functionality to prune historical JMT nodes from a substore,
//! collapsing all state to a single version. This is a non-consensus-breaking operation
//! that reclaims disk space without affecting the current state.
//!
//! **Context**
//! The Jellyfish Merkle Tree (JMT) is a **versioned** sparse merkle tree data structure.
//! Every update to the tree bumps a monotonic version counter. The branching factor for
//! the JMT is 16. Each key insert is hashed into a 256 bits path key.
//! k := H(key) e.g, 0x0123456789abcdef, where each nibble is a conceptual level in the tree.
//! It is conceptual because the tree is compact so depth can be lower. Nodes are addressed by
//! their version and nibble path. The former allows storage and abstraction layers to improve
//! locality of caches and SSD layers.
//!
//! By default, historical state is preserved which allows full nodes to answer historical
//! queries, and generate proofs at any version. In practice, there is no code using this
//! conceptual feature. It just takes up storage.
//!
//! **Amplification**
//! Without pruning, stale nodes are preserved across writes. This allows a tree
//! to answer historical queries about arbitrary versions, at the cost of storage
//! and slower queries on cold rocksdb seeks.
//!
//! The amplification from those extranodes is superlinear in the number of inserts.
//! The quick intuition is that each key update results in a new leaf and a stack of
//! ancestors. Without pruning, those ancestors nodes are never reclaimed. Therefore,
//! each update results in one stack of node per update.
//!
//! How many nodes are in that stack? It varies, but it will be about the height of
//! the compact tree which scales with the logarithm of the # of keys in the tree.
//! that gives an amplification of N * log M, where N is the number of updates, and
//! M the number of keys. We don't account for overlapping ancestor nodes since it
//! asymptotically resolves to a logarithm factor anyway.
use std::sync::Arc;

use anyhow::{ensure, Result};
use jmt::{
    restore::{JellyfishMerkleRestore, StateSnapshotReceiver},
    storage::{Node, NodeBatch, NodeKey, TreeReader, TreeWriter},
    JellyfishMerkleIterator, JellyfishMerkleTree, KeyHash, OwnedValue, RootHash,
};
use rocksdb::DB;

use crate::{
    store::substore::{DbNodeKey, SubstoreConfig, SubstoreSnapshot, VersionedKeyHash},
    Snapshot, Storage,
};

/// Mode for pruning operation.
#[derive(Debug, Clone, Copy, Default)]
pub enum PruneMode {
    /// Use range proofs for each chunk to verify correctness.
    /// Provides verification that the pruned tree matches the original.
    #[default]
    Verified,
}

/// Configuration for the pruning operation.
#[derive(Debug, Clone)]
pub struct PruneConfig {
    /// Number of key-value pairs to process per chunk.
    /// Larger values use more memory but fewer proof operations.
    pub chunk_size: usize,
    /// Whether to verify chunks with range proofs.
    pub mode: PruneMode,
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self {
            chunk_size: 100_000,
            mode: PruneMode::Verified,
        }
    }
}

impl PruneConfig {
    /// Validates the configuration, returning an error if invalid.
    pub fn validate(&self) -> Result<()> {
        ensure!(self.chunk_size >= 1, "chunk_size must be at least 1");
        ensure!(
            self.chunk_size <= 10_000_000,
            "chunk_size exceeds maximum of 10 million"
        );
        Ok(())
    }
}

/// Report returned after pruning a substore.
#[derive(Debug, Clone)]
pub struct PruneReport {
    /// The root hash after pruning (should match original).
    pub root: RootHash,
    /// The version that was pruned to.
    pub version: jmt::Version,
    /// Total number of keys processed.
    pub keys_processed: u64,
    /// Number of nodes before pruning.
    pub nodes_before: u64,
    /// Number of nodes after pruning.
    pub nodes_after: u64,
}

/// A TreeWriter/TreeReader implementation for pruning operations.
/// Writes directly to RocksDB column families.
struct PruningTreeStore {
    db: Arc<DB>,
    config: Arc<SubstoreConfig>,
}

impl PruningTreeStore {
    fn new(db: Arc<DB>, config: Arc<SubstoreConfig>) -> Self {
        Self { db, config }
    }
}

impl TreeWriter for PruningTreeStore {
    fn write_node_batch(&self, node_batch: &NodeBatch) -> Result<()> {
        let cf_jmt = self.config.cf_jmt(&self.db);
        let cf_jmt_values = self.config.cf_jmt_values(&self.db);

        let mut batch = rocksdb::WriteBatch::default();

        // Write nodes
        for (node_key, node) in node_batch.nodes() {
            let key_bytes = DbNodeKey::encode_from_node_key(node_key)?;
            let value_bytes = borsh::to_vec(node)?;
            batch.put_cf(cf_jmt, key_bytes, value_bytes);
        }

        // Write values
        for ((version, key_hash), some_value) in node_batch.values() {
            let key_bytes = VersionedKeyHash::encode_from_keyhash(key_hash, version);
            let value_bytes = borsh::to_vec(some_value)?;
            batch.put_cf(cf_jmt_values, key_bytes, value_bytes);
        }

        self.db.write(batch)?;
        Ok(())
    }
}

impl TreeReader for PruningTreeStore {
    fn get_node_option(&self, node_key: &NodeKey) -> Result<Option<Node>> {
        let cf_jmt = self.config.cf_jmt(&self.db);
        let key_bytes = DbNodeKey::encode_from_node_key(node_key)?;

        match self.db.get_pinned_cf(cf_jmt, &key_bytes)? {
            Some(bytes) => {
                let node: Node = borsh::from_slice(&bytes)?;
                Ok(Some(node))
            }
            None => Ok(None),
        }
    }

    fn get_value_option(
        &self,
        max_version: jmt::Version,
        key_hash: KeyHash,
    ) -> Result<Option<OwnedValue>> {
        let cf_jmt_values = self.config.cf_jmt_values(&self.db);

        // Look for the value at exactly max_version
        let key_bytes = VersionedKeyHash::encode_from_keyhash(&key_hash, &max_version);
        match self.db.get_pinned_cf(cf_jmt_values, &key_bytes)? {
            Some(bytes) => {
                let value: Option<Vec<u8>> = borsh::from_slice(&bytes)?;
                Ok(value)
            }
            None => Ok(None),
        }
    }

    fn get_rightmost_leaf(&self) -> Result<Option<(NodeKey, jmt::storage::LeafNode)>> {
        // Not needed for restore operations
        Ok(None)
    }
}

/// Counts the number of JMT nodes in a substore.
fn count_nodes(db: &Arc<DB>, config: &SubstoreConfig) -> Result<u64> {
    let cf_jmt = config.cf_jmt(db);
    let mut count = 0u64;
    let mut iter = db.raw_iterator_cf(cf_jmt);
    iter.seek_to_first();
    while iter.valid() {
        count += 1;
        iter.next();
    }
    Ok(count)
}

/// Prunes a substore's JMT to a single version.
///
/// This function reads all key-value pairs from the old database at the specified version,
/// and writes them to a fresh JMT in the new database. After pruning, all nodes will be
/// at the same version.
///
/// # Returns
/// A `PruneReport` with statistics about the pruning operation.
#[tracing::instrument(
    skip(old_storage, old_snapshot, new_storage, config, prune_config),
    fields(
        prefix = %hex::encode(&config.prefix),
        version = version,
        chunk_size = prune_config.chunk_size,
        mode = ?prune_config.mode,
    )
)]
pub fn prune_substore(
    old_storage: &Storage,
    old_snapshot: Snapshot,
    new_storage: &Storage,
    config: &SubstoreConfig,
    version: jmt::Version,
    prune_config: &PruneConfig,
) -> Result<PruneReport> {
    prune_config.validate()?;

    let null_root = KeyHash([0u8; 32]);

    let old_snapshot = old_snapshot.0.snapshot.clone();
    let old_db = &old_storage.db();
    let new_db = &new_storage.db();

    let config = Arc::new(SubstoreConfig::new(&config.prefix));

    let nodes_before = count_nodes(old_db, &config)?;

    let substore_snapshot = SubstoreSnapshot {
        config: Arc::clone(&config),
        rocksdb_snapshot: old_snapshot,
        version,
        db: old_db.clone(),
    };
    let substore_snapshot_arc = Arc::new(substore_snapshot);

    let original_root_hash = substore_snapshot_arc.root_hash()?;

    let tree_store = Arc::new(PruningTreeStore::new(new_db.clone(), Arc::clone(&config)));
    let iter = JellyfishMerkleIterator::new(substore_snapshot_arc.clone(), version, null_root)?;

    let chunk_size = prune_config.chunk_size;
    let mut chunk: Vec<(KeyHash, OwnedValue)> = Vec::with_capacity(chunk_size);
    let mut keys_processed = 0u64;

    // Creates a tree for range proof generation
    let old_tree = JellyfishMerkleTree::<_, sha2::Sha256>::new(substore_snapshot_arc.as_ref());

    // A restore instance with verification
    let mut restore = JellyfishMerkleRestore::<sha2::Sha256>::new(
        tree_store.clone(),
        version,
        original_root_hash,
    )?;

    for result in iter {
        let (key_hash, value) = result?;
        chunk.push((key_hash, value));
        keys_processed += 1;

        if chunk.len() == chunk_size {
            let rightmost_key = chunk.last().expect("chunk is not empty").0;
            let proof = old_tree.get_range_proof(rightmost_key, version)?;
            let chunk_data: Vec<_> = chunk.drain(..).collect();
            restore.add_chunk(chunk_data, proof)?;
            tracing::info!(keys_processed, "processed chunk of keys during pruning");
        }
    }

    // Process remaining entries
    if !chunk.is_empty() {
        let rightmost_key = chunk.last().expect("chunk is not empty").0;
        let proof = old_tree.get_range_proof(rightmost_key, version)?;
        restore.add_chunk(chunk, proof)?;
        tracing::info!(
            keys_processed,
            "processed final chunk of keys during pruning"
        );
    }
    restore.finish()?;

    // Silence unused variable warning for mode field
    let _ = prune_config.mode;
    let nodes_after = count_nodes(new_db, &config)?;

    Ok(PruneReport {
        root: original_root_hash,
        version,
        keys_processed,
        nodes_before,
        nodes_after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prune_config_validation() {
        // Valid config
        let config = PruneConfig::default();
        assert!(config.validate().is_ok());

        // Invalid: zero chunk size
        let config = PruneConfig {
            chunk_size: 0,
            mode: PruneMode::Verified,
        };
        assert!(config.validate().is_err());

        // Invalid: chunk size too large
        let config = PruneConfig {
            chunk_size: 100_000_000,
            mode: PruneMode::Verified,
        };
        assert!(config.validate().is_err());
    }
}
