mod block_acceptor;
mod config;
mod error;
mod event;

use std::{
    collections::{btree_map::Entry, BTreeMap, HashSet},
    iter,
};

use datasize::DataSize;
use futures::FutureExt;
use itertools::Itertools;
use tracing::{debug, error, warn};

use casper_types::{EraId, TimeDiff, Timestamp};

use crate::{
    components::Component,
    effect::{
        announcements::{self, PeerBehaviorAnnouncement},
        EffectBuilder, EffectExt, Effects,
    },
    types::{
        ApprovalsHashes, Block, BlockHash, BlockSignatures, FinalitySignature, Item, NodeId,
        ValidatorMatrix,
    },
    NodeRng,
};

use crate::{
    effect::{
        announcements::BlockAccumulatorAnnouncement,
        requests::{BlockAccumulatorRequest, StorageRequest},
    },
    types::{BlockHeader, FinalitySignatureId},
};
use block_acceptor::{BlockAcceptor, ShouldStore};
pub(crate) use config::Config;
use error::Error;
pub(crate) use event::Event;

#[derive(Debug)]
pub(crate) enum SyncInstruction {
    Leap,
    CaughtUp,
    BlockExec {
        next_block_hash: Option<BlockHash>,
    },
    BlockSync {
        block_hash: BlockHash,
        should_fetch_execution_state: bool,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum StartingWith {
    ExecutableBlock(BlockHash, u64),
    BlockIdentifier(BlockHash, u64),
    SyncedBlockIdentifier(BlockHash, u64),
    Hash(BlockHash),
    // simplifies call sites; results in a Leap instruction
    Nothing,
}

impl StartingWith {
    pub(crate) fn block_hash(&self) -> BlockHash {
        match self {
            StartingWith::BlockIdentifier(hash, _) => *hash,
            StartingWith::SyncedBlockIdentifier(hash, _) => *hash,
            StartingWith::ExecutableBlock(hash, _) => *hash,
            StartingWith::Hash(hash) => *hash,
            StartingWith::Nothing => BlockHash::default(),
        }
    }

    pub(crate) fn block_height(&self) -> u64 {
        match self {
            StartingWith::BlockIdentifier(_, height) => *height,
            StartingWith::SyncedBlockIdentifier(_, height) => *height,
            StartingWith::ExecutableBlock(_, height) => *height,
            StartingWith::Hash(hash) => 0,
            StartingWith::Nothing => 0,
        }
    }

    pub(crate) fn have_block(&self) -> bool {
        match self {
            StartingWith::BlockIdentifier(..) => true,
            StartingWith::ExecutableBlock(..) => true,
            StartingWith::SyncedBlockIdentifier(..) => true,
            StartingWith::Hash(_) => false,
            StartingWith::Nothing => false,
        }
    }
}

/// A cache of pending blocks and finality signatures that are gossiped to this node.
///
/// Announces new blocks and finality signatures once they become valid.
#[derive(DataSize, Debug)]
pub(crate) struct BlockAccumulator {
    validator_matrix: ValidatorMatrix,
    attempt_execution_threshold: u64,
    dead_air_interval: TimeDiff,

    block_acceptors: BTreeMap<BlockHash, BlockAcceptor>,
    block_children: BTreeMap<BlockHash, BlockHash>,
    already_handled: HashSet<BlockHash>,

    last_progress: Timestamp,
    /// The height of the subjective local tip of the chain.
    local_tip: Option<u64>,
}

impl BlockAccumulator {
    pub(crate) fn new(
        config: Config,
        validator_matrix: ValidatorMatrix,
        local_tip: Option<u64>,
    ) -> Self {
        Self {
            validator_matrix,
            attempt_execution_threshold: config.attempt_execution_threshold(),
            dead_air_interval: config.dead_air_interval(),
            already_handled: Default::default(),
            block_acceptors: Default::default(),
            block_children: Default::default(),
            last_progress: Timestamp::now(),
            local_tip,
        }
    }

    // #[allow(unused)] // todo!: Flush less aggressively. Obsolete with highest_complete_block?
    // pub(crate) fn flush(self, validator_matrix: ValidatorMatrix) -> Self {
    //     Self {
    //         already_handled: Default::default(),
    //         block_acceptors: Default::default(),
    //         block_children: Default::default(),
    //         validator_matrix,
    //         ..self
    //     }
    // }
    //
    // #[allow(unused)] // todo!
    // pub(crate) fn flush_already_handled(&mut self) {
    //     self.already_handled.clear();
    // }

    pub(crate) fn sync_instruction(&mut self, starting_with: StartingWith) -> SyncInstruction {
        // BEFORE the f-seq cant help you, LEAP
        // ? |------------- future chain ------------------------>
        // IN f-seq not in range of tip, LEAP
        // |------------- future chain ----?-ATTEMPT_EXECUTION_THRESHOLD->
        // IN f-seq in range of tip, CAUGHT UP (which will ultimately result in EXEC)
        // |------------- future chain ----?ATTEMPT_EXECUTION_THRESHOLD>
        // AFTER the f-seq cant help you, SYNC-all-state
        // |------------- future chain ------------------------> ?
        let should_fetch_execution_state = false == starting_with.have_block();

        let maybe_highest_usable_block_height = self.highest_usable_block_height();

        match starting_with {
            StartingWith::Nothing => {
                return SyncInstruction::Leap;
            }
            StartingWith::ExecutableBlock(block_hash, block_height) => {
                // keep up only
                match maybe_highest_usable_block_height {
                    None => {
                        return SyncInstruction::BlockExec {
                            next_block_hash: None,
                        };
                    }
                    Some(highest_perceived) => {
                        if block_height > highest_perceived {
                            self.block_acceptors
                                .insert(block_hash, BlockAcceptor::new(block_hash, vec![]));
                            return SyncInstruction::BlockExec {
                                next_block_hash: None,
                            };
                        }
                        if highest_perceived == block_height {
                            return SyncInstruction::BlockExec {
                                next_block_hash: None,
                            };
                        }
                        if highest_perceived.saturating_sub(self.attempt_execution_threshold)
                            <= block_height
                        {
                            return SyncInstruction::BlockExec {
                                next_block_hash: self.next_syncable_block_hash(block_hash),
                            };
                        }
                    }
                }
            }
            StartingWith::Hash(block_hash) => {
                let (block_hash, maybe_block_height) = match self.block_acceptors.get(&block_hash) {
                    None => {
                        // the accumulator is unaware of the starting-with block
                        return SyncInstruction::Leap;
                    }
                    Some(block_acceptor) => {
                        (block_acceptor.block_hash(), block_acceptor.block_height())
                    }
                };
                if self.should_sync(maybe_block_height, maybe_highest_usable_block_height) {
                    self.last_progress = Timestamp::now();
                    return SyncInstruction::BlockSync {
                        block_hash,
                        should_fetch_execution_state,
                    };
                }
            }
            StartingWith::BlockIdentifier(block_hash, block_height) => {
                // catch up only
                if self.should_sync(Some(block_height), maybe_highest_usable_block_height) {
                    self.last_progress = Timestamp::now();
                    return SyncInstruction::BlockSync {
                        block_hash,
                        should_fetch_execution_state,
                    };
                }
            }
            StartingWith::SyncedBlockIdentifier(block_hash, block_height) => {
                // catch up only
                if self.should_sync(Some(block_height), maybe_highest_usable_block_height) {
                    if let Some(child_hash) = self.next_syncable_block_hash(block_hash) {
                        self.last_progress = Timestamp::now();
                        return SyncInstruction::BlockSync {
                            block_hash: child_hash,
                            should_fetch_execution_state,
                        };
                    } else if self.last_progress.elapsed() < self.dead_air_interval {
                        return SyncInstruction::CaughtUp;
                    }
                }
            }
        }
        SyncInstruction::Leap
    }

    fn should_sync(
        &mut self,
        maybe_starting_with_block_height: Option<u64>,
        maybe_highest_usable_block_height: Option<u64>,
    ) -> bool {
        match (
            maybe_starting_with_block_height,
            maybe_highest_usable_block_height,
        ) {
            (None, _) | (_, None) => false,
            (Some(starting_with), Some(highest_usable_block_height)) => {
                let height_diff = highest_usable_block_height.saturating_sub(starting_with);
                if height_diff == 0 {
                    true
                } else {
                    height_diff <= self.attempt_execution_threshold
                }
            }
        }
    }

    fn next_syncable_block_hash(&mut self, parent_block_hash: BlockHash) -> Option<BlockHash> {
        let child_hash = self.block_children.get(&parent_block_hash)?;
        let block_acceptor = self.block_acceptors.get_mut(child_hash)?;
        block_acceptor
            .has_sufficient_finality()
            .then(|| *child_hash)
    }

    // NOT USED
    // fn register_block_by_identifier(&mut self, block_hash: BlockHash, era_id: EraId) {
    //     if self.already_handled.contains(&block_hash) {
    //         return;
    //     }
    //     let mut acceptor = BlockAcceptor::new(block_hash, vec![]);
    //     if let Some(evw) = self.validator_matrix.validator_weights(era_id) {
    //         if let Err(err) = acceptor.register_era_validator_weights(evw) {
    //             warn!(%err, "unable to register era_validator_weights");
    //             return;
    //         }
    //     }
    //     self.block_acceptors.insert(block_hash, acceptor);
    // }

    fn register_block<REv>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        block: Block,
        sender: NodeId,
    ) -> Effects<Event>
    where
        REv: Send + From<PeerBehaviorAnnouncement>,
    {
        let block_hash = block.hash();
        let era_id = block.header().era_id();

        if self
            .local_tip
            .map_or(false, |height| block.header().height() < height)
        {
            debug!(%block_hash, "ignoring outdated block");
            self.block_acceptors.remove(block_hash);
            return Effects::new();
        }

        if let Some(parent_hash) = block.parent() {
            self.block_children.insert(*parent_hash, *block_hash);
        }

        let acceptor = match self.get_or_register_acceptor_mut(*block_hash, era_id, vec![sender]) {
            Some(block_gossip_acceptor) => block_gossip_acceptor,
            None => {
                return Effects::new();
            }
        };

        let block_hash = *block.hash();
        if let Err(error) = acceptor.register_block(block, sender) {
            warn!(%error, %block_hash, "received invalid block");
            match error {
                Error::InvalidGossip(err) => {
                    return effect_builder
                        .announce_disconnect_from_peer(err.peer())
                        .ignore();
                }
                Error::EraMismatch(_err) => {
                    // TODO: Log?
                    // this block acceptor is borked; get rid of it
                    self.block_acceptors.remove(&block_hash);
                }
                Error::BlockHashMismatch {
                    expected: _,
                    actual: _,
                    peer,
                } => {
                    return effect_builder.announce_disconnect_from_peer(peer).ignore();
                }
                Error::InvalidState => {}
            }
        }
        Effects::new()
    }

    fn register_finality_signature<REv>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        finality_signature: FinalitySignature,
        sender: NodeId,
    ) -> Effects<Event>
    where
        REv: From<StorageRequest> + From<PeerBehaviorAnnouncement> + Send,
    {
        // TODO: Also ignore signatures for blocks older than the highest complete one?
        // TODO: Ignore signatures for `already_handled` blocks?

        let block_hash = finality_signature.block_hash;
        let era_id = finality_signature.era_id;

        let acceptor = match self.get_or_register_acceptor_mut(block_hash, era_id, vec![sender]) {
            Some(block_gossip_acceptor) => block_gossip_acceptor,
            None => {
                return Effects::new();
            }
        };

        match acceptor.register_finality_signature(finality_signature, sender) {
            Ok(ShouldStore::SufficientlySignedBlock { block, signatures }) => {
                let block_hash = Some(*block.hash());
                let mut block_signatures =
                    BlockSignatures::new(*block.hash(), block.header().era_id());
                let mut signature_ids = vec![];
                signatures.into_iter().for_each(|signature| {
                    signature_ids.push(signature.id());
                    block_signatures.insert_proof(signature.public_key, signature.signature);
                });
                effect_builder
                    .put_block_to_storage(Box::new(block))
                    .then(move |_| effect_builder.put_signatures_to_storage(block_signatures))
                    .event(move |_| Event::Stored {
                        block_hash,
                        finality_signature_ids: signature_ids,
                    })
            }
            Ok(ShouldStore::SingleSignature(signature)) => {
                let signature_ids = vec![signature.id()];
                effect_builder
                    .put_finality_signature_to_storage(signature)
                    .event(move |_| Event::Stored {
                        block_hash: None,
                        finality_signature_ids: signature_ids,
                    })
            }
            Ok(ShouldStore::Nothing) => Effects::new(),
            Err(Error::InvalidGossip(error)) => {
                warn!(%error, "received invalid finality_signature");
                effect_builder
                    .announce_disconnect_from_peer(error.peer())
                    .ignore()
            }
            Err(Error::EraMismatch(error)) => {
                // the acceptor logic purges finality signatures that don't match
                // the era validators, so in this case we can continue to
                // use the acceptor
                warn!(%error, "finality signature has mismatched era_id");
                Effects::new()
            }
            Err(ref error @ Error::BlockHashMismatch { peer, .. }) => {
                warn!(%error, "finality signature has mismatched block_hash");
                effect_builder.announce_disconnect_from_peer(peer).ignore()
            }
            Err(Error::InvalidState) => Effects::new(),
        }
    }

    pub(crate) fn register_updated_validator_matrix(&mut self) {
        for block_acceptor in self.block_acceptors.values_mut() {
            if let Some(era_id) = block_acceptor.era_id() {
                if let Some(weights) = self.validator_matrix.validator_weights(era_id) {
                    block_acceptor.refresh(weights);
                }
            }
        }
    }

    /// Drops all block acceptors older than this block, and will ignore them in the future.
    pub(crate) fn register_local_tip(&mut self, height: u64) {
        for block_hash in self
            .block_acceptors
            .iter()
            .filter(|(_, v)| v.block_height().unwrap_or_default() < height)
            .map(|(k, _)| *k)
            .collect_vec()
        {
            self.block_acceptors.remove(&block_hash);
            self.already_handled.insert(block_hash);
        }
        self.local_tip = self.local_tip.into_iter().chain(iter::once(height)).max();
    }

    pub(crate) fn block(&self, block_hash: BlockHash) -> Option<&Block> {
        if let Some(acceptor) = self.block_acceptors.get(&block_hash) {
            acceptor.block()
        } else {
            None
        }
    }

    fn highest_usable_block_height(&mut self) -> Option<u64> {
        let mut ret: Option<u64> = None;
        for (k, v) in &mut self.block_acceptors {
            if self.already_handled.contains(k) {
                error!(
                    "should not have a block acceptor for an already handled block_hash: {}",
                    k
                );
                continue;
            }
            if false == v.has_sufficient_finality() {
                continue;
            }
            match v.block_era_and_height() {
                None => {
                    continue;
                }
                Some((_, acceptor_height)) => {
                    if let Some(curr_height) = ret {
                        if acceptor_height <= curr_height {
                            continue;
                        }
                    }
                    ret = Some(acceptor_height);
                }
            };
        }
        ret
    }

    fn get_or_register_acceptor_mut(
        &mut self,
        block_hash: BlockHash,
        era_id: EraId,
        peers: Vec<NodeId>,
    ) -> Option<&mut BlockAcceptor> {
        if let Entry::Occupied(mut entry) = self.block_acceptors.entry(block_hash) {
            if self.already_handled.contains(&block_hash) {
                return None;
            }
            if let Some(evw) = self.validator_matrix.validator_weights(era_id) {
                let acceptor = BlockAcceptor::new_with_validator_weights(block_hash, evw, peers);
                entry.insert(acceptor);
            } else {
                entry.insert(BlockAcceptor::new(block_hash, peers));
            }
        }

        self.block_acceptors.get_mut(&block_hash)
    }

    fn get_peers(&self, block_hash: BlockHash) -> Option<Vec<NodeId>> {
        self.block_acceptors
            .get(&block_hash)
            .map(BlockAcceptor::peers)
    }

    fn handle_stored<REv>(
        &self,
        effect_builder: EffectBuilder<REv>,
        block_hash: Option<BlockHash>,
        finality_signature_ids: Vec<FinalitySignatureId>,
    ) -> Effects<Event>
    where
        REv: From<BlockAccumulatorAnnouncement> + Send,
    {
        let mut effects = if let Some(block_hash) = block_hash {
            effect_builder.announce_block_accepted(block_hash).ignore()
        } else {
            Effects::new()
        };
        for finality_signature_id in finality_signature_ids {
            effects.extend(
                effect_builder
                    .announce_finality_signature_accepted(finality_signature_id)
                    .ignore(),
            );
        }
        effects
    }
}

// TODO: is this even really a component?
impl<REv> Component<REv> for BlockAccumulator
where
    REv: From<StorageRequest>
        + From<PeerBehaviorAnnouncement>
        + From<BlockAccumulatorAnnouncement>
        + Send,
{
    type Event = Event;

    fn handle_event(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        _rng: &mut NodeRng,
        event: Self::Event,
    ) -> Effects<Self::Event> {
        match event {
            Event::Request(BlockAccumulatorRequest::GetPeersForBlock {
                block_hash,
                responder,
            }) => responder.respond(self.get_peers(block_hash)).ignore(),
            Event::ReceivedBlock { block, sender } => {
                self.register_block(effect_builder, *block, sender)
            }
            Event::ReceivedFinalitySignature {
                finality_signature,
                sender,
            } => self.register_finality_signature(effect_builder, *finality_signature, sender),
            Event::UpdatedValidatorMatrix { era_id } => {
                //self.handle_updated_validator_matrix(effect_builder, era_id)
                Effects::new()
            }
            Event::ExecutedBlock { block_header } => {
                self.register_local_tip(block_header.height());
                Effects::new()
            }
            Event::Stored {
                block_hash,
                finality_signature_ids,
            } => self.handle_stored(effect_builder, block_hash, finality_signature_ids),
        }
    }
}
