//! Module for structures that combine the features of [`sparse_chain`] and [`tx_graph`].
use crate::{
    collections::HashSet,
    sparse_chain::{self, ChainPosition, SparseChain},
    tx_graph::{self, TxGraph},
    BlockId, ForEachTxOut, FullTxOut, TxHeight,
};
use alloc::{string::ToString, vec::Vec};
use bitcoin::{OutPoint, Transaction, TxOut, Txid};
use core::fmt::Debug;

/// A consistent combination of a [`SparseChain<P>`] and a [`TxGraph<T>`].
///
/// `SparseChain` only keeps track of transaction ids and their position in the chain, but you often
/// want to store the full transactions as well. Additionally, you want to make sure that everything
/// in the chain is consistent with the full transaction data. `ChainGraph` enforces these two
/// invariants:
///
/// 1. Every transaction that is in the chain is also in the graph (you always have the full
/// transaction).
/// 2. No transactions in the chain conflict with each other, i.e., they don't double spend each
/// other or have ancestors that double spend each other.
///
/// Note that the `ChainGraph` guarantees a 1:1 mapping between transactions in the `chain` and
/// `graph` but not the other way around. Transactions may fall out of the *chain* (via re-org or
/// mempool eviction) but will remain in the *graph*.
#[derive(Clone, Debug, PartialEq)]
pub struct ChainGraph<P = TxHeight> {
    chain: SparseChain<P>,
    graph: TxGraph,
}

impl<P> Default for ChainGraph<P> {
    fn default() -> Self {
        Self {
            chain: Default::default(),
            graph: Default::default(),
        }
    }
}

impl<P> AsRef<SparseChain<P>> for ChainGraph<P> {
    fn as_ref(&self) -> &SparseChain<P> {
        &self.chain
    }
}

impl<P> AsRef<TxGraph> for ChainGraph<P> {
    fn as_ref(&self) -> &TxGraph {
        &self.graph
    }
}

impl<P> AsRef<ChainGraph<P>> for ChainGraph<P> {
    fn as_ref(&self) -> &ChainGraph<P> {
        self
    }
}

impl<P> ChainGraph<P> {
    /// Returns a reference to the internal [`SparseChain`].
    pub fn chain(&self) -> &SparseChain<P> {
        &self.chain
    }

    /// Returns a reference to the internal [`TxGraph`].
    pub fn graph(&self) -> &TxGraph {
        &self.graph
    }
}

impl<P> ChainGraph<P>
where
    P: ChainPosition,
{
    /// Create a new chain graph from a `chain` and a `graph`.
    ///
    /// There are two reasons this can return an `Err`:
    ///
    /// 1. There is a transaction in the `chain` that does not have its corresponding full
    /// transaction in `graph`.
    /// 2. The `chain` has two transactions that are allegedly in it, but they conflict in the `graph`
    /// (so could not possibly be in the same chain).
    pub fn new(chain: SparseChain<P>, graph: TxGraph) -> Result<Self, NewError<P>> {
        let mut missing = HashSet::default();
        for (pos, txid) in chain.txids() {
            if let Some(tx) = graph.get_tx(*txid) {
                let conflict = graph
                    .walk_conflicts(tx, |_, txid| Some((chain.tx_position(txid)?.clone(), txid)))
                    .next();
                if let Some((conflict_pos, conflict)) = conflict {
                    return Err(NewError::Conflict {
                        a: (pos.clone(), *txid),
                        b: (conflict_pos, conflict),
                    });
                }
            } else {
                missing.insert(*txid);
            }
        }

        if !missing.is_empty() {
            return Err(NewError::Missing(missing));
        }

        Ok(Self { chain, graph })
    }

    /// Take an update in the form of a [`SparseChain<P>`][`SparseChain`] and attempt to turn it
    /// into a chain graph by filling in full transactions from `self` and from `new_txs`. This
    /// returns a `ChainGraph<P, Cow<T>>` where the [`Cow<'a, T>`] will borrow the transaction if it
    /// got it from `self`.
    ///
    /// This is useful when interacting with services like an electrum server which returns a list
    /// of txids and heights when calling [`script_get_history`], which can easily be inserted into a
    /// [`SparseChain<TxHeight>`][`SparseChain`]. From there, you need to figure out which full
    /// transactions you are missing in your chain graph and form `new_txs`. You then use
    /// `inflate_update` to turn this into an update `ChainGraph<P, Cow<Transaction>>` and finally
    /// use [`determine_changeset`] to generate the changeset from it.
    ///
    /// [`SparseChain`]: crate::sparse_chain::SparseChain
    /// [`Cow<'a, T>`]: std::borrow::Cow
    /// [`script_get_history`]: https://docs.rs/electrum-client/latest/electrum_client/trait.ElectrumApi.html#tymethod.script_get_history
    /// [`determine_changeset`]: Self::determine_changeset
    pub fn inflate_update(
        &self,
        update: SparseChain<P>,
        new_txs: impl IntoIterator<Item = Transaction>,
    ) -> Result<ChainGraph<P>, NewError<P>> {
        let mut inflated_chain = SparseChain::default();
        let mut inflated_graph = TxGraph::default();

        for (height, hash) in update.checkpoints().clone().into_iter() {
            let _ = inflated_chain
                .insert_checkpoint(BlockId { height, hash })
                .expect("must insert");
        }

        // [TODO] @evanlinjin: These need better comments
        // - copy transactions that have changed positions into the graph
        // - add new transactions to an inflated chain
        for (pos, txid) in update.txids() {
            match self.chain.tx_position(*txid) {
                Some(original_pos) => {
                    if original_pos != pos {
                        let tx = self
                            .graph
                            .get_tx(*txid)
                            .expect("tx must exist as it is referenced in sparsechain")
                            .clone();
                        let _ = inflated_chain
                            .insert_tx(*txid, pos.clone())
                            .expect("must insert since this was already in update");
                        let _ = inflated_graph.insert_tx(tx);
                    }
                }
                None => {
                    let _ = inflated_chain
                        .insert_tx(*txid, pos.clone())
                        .expect("must insert since this was already in update");
                }
            }
        }

        for tx in new_txs {
            let _ = inflated_graph.insert_tx(tx);
        }

        ChainGraph::new(inflated_chain, inflated_graph)
    }

    /// Gets the checkpoint limit.
    ///
    /// Refer to [`SparseChain::checkpoint_limit`] for more.
    pub fn checkpoint_limit(&self) -> Option<usize> {
        self.chain.checkpoint_limit()
    }

    /// Sets the checkpoint limit.
    ///
    /// Refer to [`SparseChain::set_checkpoint_limit`] for more.
    pub fn set_checkpoint_limit(&mut self, limit: Option<usize>) {
        self.chain.set_checkpoint_limit(limit)
    }

    /// Determines the changes required to invalidate checkpoints `from_height` (inclusive) and
    /// above. Displaced transactions will have their positions moved to [`TxHeight::Unconfirmed`].
    pub fn invalidate_checkpoints_preview(&self, from_height: u32) -> ChangeSet<P> {
        ChangeSet {
            chain: self.chain.invalidate_checkpoints_preview(from_height),
            ..Default::default()
        }
    }

    /// Invalidate checkpoints `from_height` (inclusive) and above. Displaced transactions will be
    /// re-positioned to [`TxHeight::Unconfirmed`].
    ///
    /// This is equivalent to calling [`Self::invalidate_checkpoints_preview`] and
    /// [`Self::apply_changeset`] in sequence.
    pub fn invalidate_checkpoints(&mut self, from_height: u32) -> ChangeSet<P>
    where
        ChangeSet<P>: Clone,
    {
        let changeset = self.invalidate_checkpoints_preview(from_height);
        self.apply_changeset(changeset.clone());
        changeset
    }

    /// Get a transaction currently in the underlying [`SparseChain`].
    ///
    /// This does not necessarily mean that it is *confirmed* in the blockchain; it might just be in
    /// the unconfirmed transaction list within the [`SparseChain`].
    pub fn get_tx_in_chain(&self, txid: Txid) -> Option<(&P, &Transaction)> {
        let position = self.chain.tx_position(txid)?;
        let full_tx = self.graph.get_tx(txid).expect("must exist");
        Some((position, full_tx))
    }

    /// Determines the changes required to insert a transaction into the inner [`ChainGraph`] and
    /// [`SparseChain`] at the given `position`.
    ///
    /// If inserting it into the chain `position` will result in conflicts, the returned
    /// [`ChangeSet`] should evict conflicting transactions.
    pub fn insert_tx_preview(
        &self,
        tx: Transaction,
        pos: P,
    ) -> Result<ChangeSet<P>, InsertTxError<P>> {
        let mut changeset = ChangeSet {
            chain: self.chain.insert_tx_preview(tx.txid(), pos)?,
            graph: self.graph.insert_tx_preview(tx),
        };
        self.fix_conflicts(&mut changeset)?;
        Ok(changeset)
    }

    /// Inserts [`Transaction`] at the given chain position.
    ///
    /// This is equivalent to calling [`Self::insert_tx_preview`] and [`Self::apply_changeset`] in
    /// sequence.
    pub fn insert_tx(&mut self, tx: Transaction, pos: P) -> Result<ChangeSet<P>, InsertTxError<P>> {
        let changeset = self.insert_tx_preview(tx, pos)?;
        self.apply_changeset(changeset.clone());
        Ok(changeset)
    }

    /// Determines the changes required to insert a [`TxOut`] into the internal [`TxGraph`].
    pub fn insert_txout_preview(&self, outpoint: OutPoint, txout: TxOut) -> ChangeSet<P> {
        ChangeSet {
            chain: Default::default(),
            graph: self.graph.insert_txout_preview(outpoint, txout),
        }
    }

    /// Inserts a [`TxOut`] into the internal [`TxGraph`].
    ///
    /// This is equivalent to calling [`Self::insert_txout_preview`] and [`Self::apply_changeset`]
    /// in sequence.
    pub fn insert_txout(&mut self, outpoint: OutPoint, txout: TxOut) -> ChangeSet<P> {
        let changeset = self.insert_txout_preview(outpoint, txout);
        self.apply_changeset(changeset.clone());
        changeset
    }

    /// Determines the changes required to insert a `block_id` (a height and block hash) into the
    /// chain.
    ///
    /// If a checkpoint with a different hash already exists at that height, this will return an error.
    pub fn insert_checkpoint_preview(
        &self,
        block_id: BlockId,
    ) -> Result<ChangeSet<P>, InsertCheckpointError> {
        self.chain
            .insert_checkpoint_preview(block_id)
            .map(|chain_changeset| ChangeSet {
                chain: chain_changeset,
                ..Default::default()
            })
    }

    /// Inserts checkpoint into [`Self`].
    ///
    /// This is equivalent to calling [`Self::insert_checkpoint_preview`] and
    /// [`Self::apply_changeset`] in sequence.
    pub fn insert_checkpoint(
        &mut self,
        block_id: BlockId,
    ) -> Result<ChangeSet<P>, InsertCheckpointError> {
        let changeset = self.insert_checkpoint_preview(block_id)?;
        self.apply_changeset(changeset.clone());
        Ok(changeset)
    }

    /// Calculates the difference between self and `update` in the form of a [`ChangeSet`].
    pub fn determine_changeset(
        &self,
        update: &ChainGraph<P>,
    ) -> Result<ChangeSet<P>, UpdateError<P>> {
        let chain_changeset = self
            .chain
            .determine_changeset(&update.chain)
            .map_err(UpdateError::Chain)?;

        let mut changeset = ChangeSet {
            chain: chain_changeset,
            graph: self.graph.determine_additions(&update.graph),
        };

        self.fix_conflicts(&mut changeset)?;
        Ok(changeset)
    }

    /// Given a transaction, return an iterator of `txid`s that conflict with it (spends at least
    /// one of the same inputs). This iterator includes all descendants of conflicting transactions.
    ///
    /// This method only returns conflicts that exist in the [`SparseChain`] as transactions that
    /// are not included in [`SparseChain`] are already considered as evicted.
    pub fn tx_conflicts_in_chain<'a>(
        &'a self,
        tx: &'a Transaction,
    ) -> impl Iterator<Item = (&'a P, Txid)> + 'a {
        self.graph.walk_conflicts(tx, move |_, conflict_txid| {
            self.chain
                .tx_position(conflict_txid)
                .map(|conflict_pos| (conflict_pos, conflict_txid))
        })
    }

    /// Fix changeset conflicts.
    ///
    /// **WARNING:** If there are any missing full txs, conflict resolution will not be complete. In
    /// debug mode, this will result in panic.
    fn fix_conflicts(&self, changeset: &mut ChangeSet<P>) -> Result<(), UnresolvableConflict<P>> {
        let mut chain_conflicts = vec![];

        for (&txid, pos_change) in &changeset.chain.txids {
            let pos = match pos_change {
                Some(pos) => {
                    // Ignore txs that are still in the chain -- we only care about new ones
                    if self.chain.tx_position(txid).is_some() {
                        continue;
                    }
                    pos
                }
                // Ignore txids that are being deleted by the change (they can't conflict)
                None => continue,
            };

            let mut full_tx = self.graph.get_tx(txid);

            if full_tx.is_none() {
                full_tx = changeset.graph.tx.iter().find(|tx| tx.txid() == txid)
            }

            debug_assert!(full_tx.is_some(), "should have full tx at this point");

            let full_tx = match full_tx {
                Some(full_tx) => full_tx,
                None => continue,
            };

            for (conflict_pos, conflict_txid) in self.tx_conflicts_in_chain(full_tx) {
                chain_conflicts.push((pos.clone(), txid, conflict_pos, conflict_txid))
            }
        }

        for (update_pos, update_txid, conflicting_pos, conflicting_txid) in chain_conflicts {
            // We have found a tx that conflicts with our update txid. Only allow this when the
            // conflicting tx will be positioned as "unconfirmed" after the update is applied.
            // If so, we will modify the changeset to evict the conflicting txid.

            // determine the position of the conflicting txid after the current changeset is applied
            let conflicting_new_pos = changeset
                .chain
                .txids
                .get(&conflicting_txid)
                .map(Option::as_ref)
                .unwrap_or(Some(conflicting_pos));

            match conflicting_new_pos {
                None => {
                    // conflicting txid will be deleted, can ignore
                }
                Some(existing_new_pos) => match existing_new_pos.height() {
                    TxHeight::Confirmed(_) => {
                        // the new position of the conflicting tx is "confirmed", therefore cannot be
                        // evicted, return error
                        return Err(UnresolvableConflict {
                            already_confirmed_tx: (conflicting_pos.clone(), conflicting_txid),
                            update_tx: (update_pos, update_txid),
                        });
                    }
                    TxHeight::Unconfirmed => {
                        // the new position of the conflicting tx is "unconfirmed", therefore it can
                        // be evicted
                        changeset.chain.txids.insert(conflicting_txid, None);
                    }
                },
            };
        }

        Ok(())
    }

    /// Applies `changeset` to `self`.
    ///
    /// **Warning** this method assumes that the changeset is correctly formed. If it is not, the
    /// chain graph may behave incorrectly in the future and panic unexpectedly.
    pub fn apply_changeset(&mut self, changeset: ChangeSet<P>) {
        self.chain.apply_changeset(changeset.chain);
        self.graph.apply_additions(changeset.graph);
    }

    /// Applies the `update` chain graph. Note this is shorthand for calling
    /// [`Self::determine_changeset()`] and [`Self::apply_changeset()`] in sequence.
    pub fn apply_update(&mut self, update: ChainGraph<P>) -> Result<ChangeSet<P>, UpdateError<P>> {
        let changeset = self.determine_changeset(&update)?;
        self.apply_changeset(changeset.clone());
        Ok(changeset)
    }

    /// Get the full transaction output at an outpoint if it exists in the chain and the graph.
    pub fn full_txout(&self, outpoint: OutPoint) -> Option<FullTxOut<P>> {
        self.chain.full_txout(&self.graph, outpoint)
    }

    /// Iterate over the full transactions and their position in the chain ordered by their position
    /// in ascending order.
    pub fn transactions_in_chain(&self) -> impl DoubleEndedIterator<Item = (&P, &Transaction)> {
        self.chain
            .txids()
            .map(move |(pos, txid)| (pos, self.graph.get_tx(*txid).expect("must exist")))
    }

    /// Find the transaction in the chain that spends `outpoint`.
    ///
    /// This uses the input/output relationships in the internal `graph`. Note that the transaction
    /// which includes `outpoint` does not need to be in the `graph` or the `chain` for this to
    /// return `Some(_)`.
    pub fn spent_by(&self, outpoint: OutPoint) -> Option<(&P, Txid)> {
        self.chain.spent_by(&self.graph, outpoint)
    }

    /// Whether the chain graph contains any data whatsoever.
    pub fn is_empty(&self) -> bool {
        self.chain.is_empty() && self.graph.is_empty()
    }
}

/// Represents changes to [`ChainGraph`].
///
/// This is essentially a combination of [`sparse_chain::ChangeSet`] and [`tx_graph::Additions`].
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Deserialize, serde::Serialize),
    serde(
        crate = "serde_crate",
        bound(
            deserialize = "P: serde::Deserialize<'de>",
            serialize = "P: serde::Serialize"
        )
    )
)]
#[must_use]
pub struct ChangeSet<P> {
    pub chain: sparse_chain::ChangeSet<P>,
    pub graph: tx_graph::Additions,
}

impl<P> ChangeSet<P> {
    /// Returns `true` if this [`ChangeSet`] records no changes.
    pub fn is_empty(&self) -> bool {
        self.chain.is_empty() && self.graph.is_empty()
    }

    /// Returns `true` if this [`ChangeSet`] contains transaction evictions.
    pub fn contains_eviction(&self) -> bool {
        self.chain
            .txids
            .iter()
            .any(|(_, new_pos)| new_pos.is_none())
    }

    /// Appends the changes in `other` into self such that applying `self` afterward has the same
    /// effect as sequentially applying the original `self` and `other`.
    pub fn append(&mut self, other: ChangeSet<P>)
    where
        P: ChainPosition,
    {
        self.chain.append(other.chain);
        self.graph.append(other.graph);
    }
}

impl<P> Default for ChangeSet<P> {
    fn default() -> Self {
        Self {
            chain: Default::default(),
            graph: Default::default(),
        }
    }
}

impl<P> ForEachTxOut for ChainGraph<P> {
    fn for_each_txout(&self, f: impl FnMut((OutPoint, &TxOut))) {
        self.graph.for_each_txout(f)
    }
}

impl<P> ForEachTxOut for ChangeSet<P> {
    fn for_each_txout(&self, f: impl FnMut((OutPoint, &TxOut))) {
        self.graph.for_each_txout(f)
    }
}

/// Error that may occur when calling [`ChainGraph::new`].
#[derive(Clone, Debug, PartialEq)]
pub enum NewError<P> {
    /// Two transactions within the sparse chain conflicted with each other
    Conflict { a: (P, Txid), b: (P, Txid) },
    /// One or more transactions in the chain were not in the graph
    Missing(HashSet<Txid>),
}

impl<P: core::fmt::Debug> core::fmt::Display for NewError<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NewError::Conflict { a, b } => write!(
                f,
                "Unable to inflate sparse chain to chain graph since transactions {:?} and {:?}",
                a, b
            ),
            NewError::Missing(missing) => write!(
                f,
                "missing full transactions for {}",
                missing
                    .iter()
                    .map(|txid| txid.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

#[cfg(feature = "std")]
impl<P: core::fmt::Debug> std::error::Error for NewError<P> {}

/// Error that may occur when inserting a transaction.
///
/// Refer to [`ChainGraph::insert_tx_preview`] and [`ChainGraph::insert_tx`].
#[derive(Clone, Debug, PartialEq)]
pub enum InsertTxError<P> {
    Chain(sparse_chain::InsertTxError<P>),
    UnresolvableConflict(UnresolvableConflict<P>),
}

impl<P: core::fmt::Debug> core::fmt::Display for InsertTxError<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            InsertTxError::Chain(inner) => core::fmt::Display::fmt(inner, f),
            InsertTxError::UnresolvableConflict(inner) => core::fmt::Display::fmt(inner, f),
        }
    }
}

impl<P> From<sparse_chain::InsertTxError<P>> for InsertTxError<P> {
    fn from(inner: sparse_chain::InsertTxError<P>) -> Self {
        Self::Chain(inner)
    }
}

#[cfg(feature = "std")]
impl<P: core::fmt::Debug> std::error::Error for InsertTxError<P> {}

/// A nice alias of [`sparse_chain::InsertCheckpointError`].
pub type InsertCheckpointError = sparse_chain::InsertCheckpointError;

/// Represents an update failure.
#[derive(Clone, Debug, PartialEq)]
pub enum UpdateError<P> {
    /// The update chain was inconsistent with the existing chain
    Chain(sparse_chain::UpdateError<P>),
    /// A transaction in the update spent the same input as an already confirmed transaction
    UnresolvableConflict(UnresolvableConflict<P>),
}

impl<P: core::fmt::Debug> core::fmt::Display for UpdateError<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            UpdateError::Chain(inner) => core::fmt::Display::fmt(inner, f),
            UpdateError::UnresolvableConflict(inner) => core::fmt::Display::fmt(inner, f),
        }
    }
}

impl<P> From<sparse_chain::UpdateError<P>> for UpdateError<P> {
    fn from(inner: sparse_chain::UpdateError<P>) -> Self {
        Self::Chain(inner)
    }
}

#[cfg(feature = "std")]
impl<P: core::fmt::Debug> std::error::Error for UpdateError<P> {}

/// Represents an unresolvable conflict between an update's transaction and an
/// already-confirmed transaction.
#[derive(Clone, Debug, PartialEq)]
pub struct UnresolvableConflict<P> {
    pub already_confirmed_tx: (P, Txid),
    pub update_tx: (P, Txid),
}

impl<P: core::fmt::Debug> core::fmt::Display for UnresolvableConflict<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Self {
            already_confirmed_tx,
            update_tx,
        } = self;
        write!(f, "update transaction {} at height {:?} conflicts with an already confirmed transaction {} at height {:?}",
            update_tx.1, update_tx.0, already_confirmed_tx.1, already_confirmed_tx.0)
    }
}

impl<P> From<UnresolvableConflict<P>> for UpdateError<P> {
    fn from(inner: UnresolvableConflict<P>) -> Self {
        Self::UnresolvableConflict(inner)
    }
}

impl<P> From<UnresolvableConflict<P>> for InsertTxError<P> {
    fn from(inner: UnresolvableConflict<P>) -> Self {
        Self::UnresolvableConflict(inner)
    }
}

#[cfg(feature = "std")]
impl<P: core::fmt::Debug> std::error::Error for UnresolvableConflict<P> {}
