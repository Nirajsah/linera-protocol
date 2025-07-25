// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Operations that persist changes to the chain state when they are successful.

use std::{borrow::Cow, collections::BTreeMap};

use futures::future::Either;
use linera_base::{
    crypto::ValidatorPublicKey,
    data_types::{Blob, BlockHeight, Epoch, Timestamp},
    ensure,
    identifiers::{ChainId, EventId, StreamId},
};
use linera_chain::{
    data_types::{
        BlockExecutionOutcome, BlockProposal, MessageBundle, OriginalProposal, ProposalContent,
    },
    manager,
    types::{ConfirmedBlockCertificate, TimeoutCertificate, ValidatedBlockCertificate},
    ChainExecutionContext, ChainStateView, ExecutionResultExt as _,
};
use linera_execution::{committee::Committee, system::EPOCH_STREAM_NAME};
use linera_storage::{Clock as _, Storage};
use linera_views::{
    context::Context,
    views::{RootView, View},
};
use tokio::sync::oneshot;
use tracing::{debug, instrument, trace, warn};

use super::{check_block_epoch, ChainWorkerConfig, ChainWorkerState};
use crate::{
    data_types::ChainInfoResponse,
    worker::{NetworkActions, Notification, Reason, WorkerError},
};

/// Wrapper type that tracks if the changes to the `chain` state should be rolled back when
/// dropped.
pub struct ChainWorkerStateWithAttemptedChanges<'state, StorageClient>
where
    StorageClient: Storage + Clone + Send + Sync + 'static,
{
    state: &'state mut ChainWorkerState<StorageClient>,
    succeeded: bool,
}

impl<'state, StorageClient> ChainWorkerStateWithAttemptedChanges<'state, StorageClient>
where
    StorageClient: Storage + Clone + Send + Sync + 'static,
{
    /// Creates a new [`ChainWorkerStateWithAttemptedChanges`] instance to change the
    /// `state`.
    pub(super) async fn new(state: &'state mut ChainWorkerState<StorageClient>) -> Self {
        assert!(
            !state.chain.has_pending_changes().await,
            "`ChainStateView` has unexpected leftover changes"
        );

        ChainWorkerStateWithAttemptedChanges {
            state,
            succeeded: false,
        }
    }

    /// Processes a leader timeout issued for this multi-owner chain.
    pub(super) async fn process_timeout(
        &mut self,
        certificate: TimeoutCertificate,
    ) -> Result<(ChainInfoResponse, NetworkActions), WorkerError> {
        // Check that the chain is active and ready for this timeout.
        // Verify the certificate. Returns a catch-all error to make client code more robust.
        self.state.ensure_is_active().await?;
        let (chain_epoch, committee) = self.state.chain.current_committee()?;
        ensure!(
            certificate.inner().epoch() == chain_epoch,
            WorkerError::InvalidEpoch {
                chain_id: certificate.inner().chain_id(),
                chain_epoch,
                epoch: certificate.inner().epoch()
            }
        );
        certificate.check(committee)?;
        let mut actions = NetworkActions::default();
        if self
            .state
            .chain
            .tip_state
            .get()
            .already_validated_block(certificate.inner().height())?
        {
            return Ok((
                ChainInfoResponse::new(&self.state.chain, self.state.config.key_pair()),
                actions,
            ));
        }
        let old_round = self.state.chain.manager.current_round();
        let timeout_chain_id = certificate.inner().chain_id();
        let timeout_height = certificate.inner().height();
        self.state
            .chain
            .manager
            .handle_timeout_certificate(certificate, self.state.storage.clock().current_time());
        let round = self.state.chain.manager.current_round();
        if round > old_round {
            actions.notifications.push(Notification {
                chain_id: timeout_chain_id,
                reason: Reason::NewRound {
                    height: timeout_height,
                    round,
                },
            })
        }
        let info = ChainInfoResponse::new(&self.state.chain, self.state.config.key_pair());
        self.save().await?;
        Ok((info, actions))
    }

    /// Tries to load all blobs published in this proposal.
    ///
    /// If they cannot be found, it creates an entry in `pending_proposed_blobs` so they can be
    /// submitted one by one.
    pub(super) async fn load_proposal_blobs(
        &mut self,
        proposal: &BlockProposal,
    ) -> Result<Vec<Blob>, WorkerError> {
        let owner = proposal.owner();
        let BlockProposal {
            content:
                ProposalContent {
                    block,
                    round,
                    outcome: _,
                },
            original_proposal,
            signature: _,
        } = proposal;

        let mut maybe_blobs = self
            .state
            .maybe_get_required_blobs(proposal.required_blob_ids(), None)
            .await?;
        let missing_blob_ids = super::missing_blob_ids(&maybe_blobs);
        if !missing_blob_ids.is_empty() {
            let chain = &mut self.state.chain;
            if chain.ownership().open_multi_leader_rounds {
                // TODO(#3203): Allow multiple pending proposals on permissionless chains.
                chain.pending_proposed_blobs.clear();
            }
            let validated = matches!(original_proposal, Some(OriginalProposal::Regular { .. }));
            chain
                .pending_proposed_blobs
                .try_load_entry_mut(&owner)
                .await?
                .update(*round, validated, maybe_blobs)
                .await?;
            self.save().await?;
            return Err(WorkerError::BlobsNotFound(missing_blob_ids));
        }
        let published_blobs = block
            .published_blob_ids()
            .iter()
            .filter_map(|blob_id| maybe_blobs.remove(blob_id).flatten())
            .collect::<Vec<_>>();
        Ok(published_blobs)
    }

    /// Votes for a block proposal for the next block for this chain.
    pub(super) async fn vote_for_block_proposal(
        &mut self,
        proposal: BlockProposal,
        outcome: BlockExecutionOutcome,
        local_time: Timestamp,
    ) -> Result<(), WorkerError> {
        // Create the vote and store it in the chain state.
        let block = outcome.with(proposal.content.block.clone());
        let created_blobs: BTreeMap<_, _> = block.iter_created_blobs().collect();
        let blobs = self
            .state
            .get_required_blobs(proposal.expected_blob_ids(), &created_blobs)
            .await?;
        let key_pair = self.state.config.key_pair();
        let manager = &mut self.state.chain.manager;
        match manager.create_vote(proposal, block, key_pair, local_time, blobs)? {
            // Cache the value we voted on, so the client doesn't have to send it again.
            Some(Either::Left(vote)) => {
                self.state
                    .block_values
                    .insert(Cow::Borrowed(vote.value.inner()));
            }
            Some(Either::Right(vote)) => {
                self.state
                    .block_values
                    .insert(Cow::Borrowed(vote.value.inner()));
            }
            None => (),
        }
        self.save().await?;
        Ok(())
    }

    /// Processes a validated block issued for this multi-owner chain.
    pub(super) async fn process_validated_block(
        &mut self,
        certificate: ValidatedBlockCertificate,
    ) -> Result<(ChainInfoResponse, NetworkActions, bool), WorkerError> {
        let block = certificate.block();

        let header = &block.header;
        let height = header.height;
        // Check that the chain is active and ready for this validated block.
        // Verify the certificate. Returns a catch-all error to make client code more robust.
        self.state.ensure_is_active().await?;
        let (epoch, committee) = self.state.chain.current_committee()?;
        check_block_epoch(epoch, header.chain_id, header.epoch)?;
        certificate.check(committee)?;
        let mut actions = NetworkActions::default();
        let already_committed_block = self
            .state
            .chain
            .tip_state
            .get()
            .already_validated_block(height)?;
        let should_skip_validated_block = || {
            self.state
                .chain
                .manager
                .check_validated_block(&certificate)
                .map(|outcome| outcome == manager::Outcome::Skip)
        };
        if already_committed_block || should_skip_validated_block()? {
            // If we just processed the same pending block, return the chain info unchanged.
            return Ok((
                ChainInfoResponse::new(&self.state.chain, self.state.config.key_pair()),
                actions,
                true,
            ));
        }

        self.state
            .block_values
            .insert(Cow::Borrowed(certificate.inner().inner()));
        let required_blob_ids = block.required_blob_ids();
        let maybe_blobs = self
            .state
            .maybe_get_required_blobs(required_blob_ids, Some(&block.created_blobs()))
            .await?;
        let missing_blob_ids = super::missing_blob_ids(&maybe_blobs);
        if !missing_blob_ids.is_empty() {
            self.state
                .chain
                .pending_validated_blobs
                .update(certificate.round, true, maybe_blobs)
                .await?;
            self.save().await?;
            return Err(WorkerError::BlobsNotFound(missing_blob_ids));
        }
        let blobs = maybe_blobs
            .into_iter()
            .filter_map(|(blob_id, maybe_blob)| Some((blob_id, maybe_blob?)))
            .collect();
        let old_round = self.state.chain.manager.current_round();
        self.state.chain.manager.create_final_vote(
            certificate,
            self.state.config.key_pair(),
            self.state.storage.clock().current_time(),
            blobs,
        )?;
        let info = ChainInfoResponse::new(&self.state.chain, self.state.config.key_pair());
        self.save().await?;
        let round = self.state.chain.manager.current_round();
        if round > old_round {
            actions.notifications.push(Notification {
                chain_id: self.state.chain_id(),
                reason: Reason::NewRound { height, round },
            })
        }
        Ok((info, actions, false))
    }

    /// Processes a confirmed block (aka a commit).
    pub(super) async fn process_confirmed_block(
        &mut self,
        certificate: ConfirmedBlockCertificate,
        notify_when_messages_are_delivered: Option<oneshot::Sender<()>>,
    ) -> Result<(ChainInfoResponse, NetworkActions), WorkerError> {
        let block = certificate.block();
        let height = block.header.height;
        let chain_id = block.header.chain_id;

        // Check that the chain is active and ready for this confirmation.
        let tip = self.state.chain.tip_state.get().clone();
        if tip.next_block_height > height {
            // We already processed this block.
            let actions = self.state.create_network_actions().await?;
            self.register_delivery_notifier(height, &actions, notify_when_messages_are_delivered)
                .await;
            let info = ChainInfoResponse::new(&self.state.chain, self.state.config.key_pair());
            return Ok((info, actions));
        }

        // We haven't processed the block - verify the certificate first
        let epoch = block.header.epoch;
        // Get the committee for the block's epoch from storage.
        if let Some(committee) = self
            .state
            .chain
            .execution_state
            .system
            .committees
            .get()
            .get(&epoch)
        {
            certificate.check(committee)?;
        } else {
            let committees = self.state.storage.committees_for(epoch..=epoch).await?;
            let Some(committee) = committees.get(&epoch) else {
                let net_description = self
                    .state
                    .storage
                    .read_network_description()
                    .await?
                    .ok_or_else(|| WorkerError::MissingNetworkDescription)?;
                return Err(WorkerError::EventsNotFound(vec![EventId {
                    chain_id: net_description.admin_chain_id,
                    stream_id: StreamId::system(EPOCH_STREAM_NAME),
                    index: epoch.0,
                }]));
            };
            // This line is duplicated, but this avoids cloning and a lifetimes error.
            certificate.check(committee)?;
        }

        // Certificate check passed - which means the blobs the block requires are legitimate and
        // we can take note of it, so that if any are missing, we will accept them when the client
        // sends them.
        let required_blob_ids = block.required_blob_ids();
        let created_blobs: BTreeMap<_, _> = block.iter_created_blobs().collect();
        let blobs_result = self
            .state
            .get_required_blobs(required_blob_ids.iter().copied(), &created_blobs)
            .await
            .map(|blobs| blobs.into_values().collect::<Vec<_>>());

        if let Ok(blobs) = &blobs_result {
            self.state
                .storage
                .write_blobs_and_certificate(blobs, &certificate)
                .await?;
            let events = block
                .body
                .events
                .iter()
                .flatten()
                .map(|event| (event.id(chain_id), event.value.clone()));
            self.state.storage.write_events(events).await?;
        }

        // Update the blob state with last used certificate hash.
        let blob_state = certificate.value().to_blob_state(blobs_result.is_ok());
        let blob_ids = required_blob_ids.into_iter().collect::<Vec<_>>();
        self.state
            .storage
            .maybe_write_blob_states(&blob_ids, blob_state)
            .await?;

        let mut blobs = blobs_result?
            .into_iter()
            .map(|blob| (blob.id(), blob))
            .collect::<BTreeMap<_, _>>();

        // If this block is higher than the next expected block in this chain, we're going
        // to have a gap: do not execute this block, only update the outboxes and return.
        if tip.next_block_height < height {
            // Update the outboxes.
            self.state
                .chain
                .preprocess_block(certificate.value())
                .await?;
            // Persist chain.
            self.save().await?;
            let actions = self.state.create_network_actions().await?;
            trace!("Preprocessed confirmed block {height} on chain {chain_id:.8}");
            self.register_delivery_notifier(height, &actions, notify_when_messages_are_delivered)
                .await;
            let info = ChainInfoResponse::new(&self.state.chain, self.state.config.key_pair());
            return Ok((info, actions));
        }

        // This should always be true for valid certificates.
        ensure!(
            tip.block_hash == block.header.previous_block_hash,
            WorkerError::InvalidBlockChaining
        );

        // If we got here, `height` is equal to `tip.next_block_height` and the block is
        // properly chained. Verify that the chain is active and that the epoch we used for
        // verifying the certificate is actually the active one on the chain.
        self.state.ensure_is_active().await?;
        let (epoch, _) = self.state.chain.current_committee()?;
        check_block_epoch(epoch, chain_id, block.header.epoch)?;

        let published_blobs = block
            .published_blob_ids()
            .iter()
            .filter_map(|blob_id| blobs.remove(blob_id))
            .collect::<Vec<_>>();

        // If height is zero, we haven't initialized the chain state or verified the epoch before -
        // do it now.
        // This will fail if the chain description blob is still missing - but that's alright,
        // because we already wrote the blob state above, so the client can now upload the
        // blob, which will get accepted, and retry.
        if height == BlockHeight::ZERO {
            self.state.ensure_is_active().await?;
            let (epoch, _) = self.state.chain.current_committee()?;
            check_block_epoch(epoch, chain_id, block.header.epoch)?;
        }

        // Execute the block and update inboxes.
        let local_time = self.state.storage.clock().current_time();
        let chain = &mut self.state.chain;
        chain
            .remove_bundles_from_inboxes(block.header.timestamp, &block.body.incoming_bundles)
            .await?;
        let oracle_responses = Some(block.body.oracle_responses.clone());
        let (proposed_block, outcome) = block.clone().into_proposal();
        let verified_outcome = if let Some(execution_state) =
            self.state.execution_state_cache.remove(&outcome.state_hash)
        {
            chain.execution_state = execution_state;
            outcome.clone()
        } else {
            chain
                .execute_block(
                    &proposed_block,
                    local_time,
                    None,
                    &published_blobs,
                    oracle_responses,
                )
                .await?
        };
        // We should always agree on the messages and state hash.
        ensure!(
            outcome == verified_outcome,
            WorkerError::IncorrectOutcome {
                submitted: Box::new(outcome),
                computed: Box::new(verified_outcome),
            }
        );
        // Update the rest of the chain state.
        chain
            .apply_confirmed_block(certificate.value(), local_time)
            .await?;
        self.state
            .track_newly_created_chains(&proposed_block, &outcome);
        let mut actions = self.state.create_network_actions().await?;
        trace!("Processed confirmed block {height} on chain {chain_id:.8}");
        let hash = certificate.hash();
        actions.notifications.push(Notification {
            chain_id,
            reason: Reason::NewBlock { height, hash },
        });
        // Persist chain.
        self.save().await?;

        self.state
            .block_values
            .insert(Cow::Owned(certificate.into_inner().into_inner()));

        self.register_delivery_notifier(height, &actions, notify_when_messages_are_delivered)
            .await;
        let info = ChainInfoResponse::new(&self.state.chain, self.state.config.key_pair());

        Ok((info, actions))
    }

    /// Schedules a notification for when cross-chain messages are delivered up to the given
    /// `height`.
    #[instrument(level = "trace", skip(self, notify_when_messages_are_delivered))]
    async fn register_delivery_notifier(
        &mut self,
        height: BlockHeight,
        actions: &NetworkActions,
        notify_when_messages_are_delivered: Option<oneshot::Sender<()>>,
    ) {
        if let Some(notifier) = notify_when_messages_are_delivered {
            if actions
                .cross_chain_requests
                .iter()
                .any(|request| request.has_messages_lower_or_equal_than(height))
            {
                self.state.delivery_notifier.register(height, notifier);
            } else {
                // No need to wait. Also, cross-chain requests may not trigger the
                // notifier later, even if we register it.
                if let Err(()) = notifier.send(()) {
                    warn!("Failed to notify message delivery to caller");
                }
            }
        }
    }

    /// Updates the chain's inboxes, receiving messages from a cross-chain update.
    pub(super) async fn process_cross_chain_update(
        &mut self,
        origin: ChainId,
        bundles: Vec<(Epoch, MessageBundle)>,
    ) -> Result<Option<BlockHeight>, WorkerError> {
        // Only process certificates with relevant heights and epochs.
        let next_height_to_receive = self
            .state
            .chain
            .next_block_height_to_receive(&origin)
            .await?;
        let last_anticipated_block_height = self
            .state
            .chain
            .last_anticipated_block_height(&origin)
            .await?;
        let helper = CrossChainUpdateHelper::new(&self.state.config, &self.state.chain);
        let recipient = self.state.chain_id();
        let bundles = helper.select_message_bundles(
            &origin,
            recipient,
            next_height_to_receive,
            last_anticipated_block_height,
            bundles,
        )?;
        let Some(last_updated_height) = bundles.last().map(|bundle| bundle.height) else {
            return Ok(None);
        };
        // Process the received messages in certificates.
        let local_time = self.state.storage.clock().current_time();
        let mut previous_height = None;
        for bundle in bundles {
            let add_to_received_log = previous_height != Some(bundle.height);
            previous_height = Some(bundle.height);
            // Update the staged chain state with the received block.
            self.state
                .chain
                .receive_message_bundle(&origin, bundle, local_time, add_to_received_log)
                .await?;
        }
        if !self.state.config.allow_inactive_chains && !self.state.chain.is_active() {
            // Refuse to create a chain state if the chain is still inactive by
            // now. Accordingly, do not send a confirmation, so that the
            // cross-chain update is retried later.
            warn!(
                "Refusing to deliver messages to {recipient:?} from {origin:?} \
                at height {last_updated_height} because the recipient is still inactive",
            );
            return Ok(None);
        }
        // Save the chain.
        self.save().await?;
        Ok(Some(last_updated_height))
    }

    /// Handles the cross-chain request confirming that the recipient was updated.
    pub(super) async fn confirm_updated_recipient(
        &mut self,
        recipient: ChainId,
        latest_height: BlockHeight,
    ) -> Result<(), WorkerError> {
        let fully_delivered = self
            .state
            .chain
            .mark_messages_as_received(&recipient, latest_height)
            .await?
            && self
                .state
                .all_messages_to_tracked_chains_delivered_up_to(latest_height)
                .await?;

        self.save().await?;

        if fully_delivered {
            self.state.delivery_notifier.notify(latest_height);
        }

        Ok(())
    }

    pub async fn update_received_certificate_trackers(
        &mut self,
        new_trackers: BTreeMap<ValidatorPublicKey, u64>,
    ) -> Result<(), WorkerError> {
        self.state
            .chain
            .update_received_certificate_trackers(new_trackers);
        self.save().await?;
        Ok(())
    }

    /// Attempts to vote for a leader timeout, if possible.
    pub(super) async fn vote_for_leader_timeout(&mut self) -> Result<(), WorkerError> {
        let chain = &mut self.state.chain;
        let epoch = chain.execution_state.system.epoch.get();
        let chain_id = chain.chain_id();
        let height = chain.tip_state.get().next_block_height;
        let key_pair = self.state.config.key_pair();
        let local_time = self.state.storage.clock().current_time();
        if chain
            .manager
            .vote_timeout(chain_id, height, *epoch, key_pair, local_time)
        {
            self.save().await?;
        }
        Ok(())
    }

    /// Votes for falling back to a public chain.
    pub(super) async fn vote_for_fallback(&mut self) -> Result<(), WorkerError> {
        let chain = &mut self.state.chain;
        if let (epoch, Some(entry)) = (
            chain.execution_state.system.epoch.get(),
            chain.unskippable_bundles.front(),
        ) {
            let elapsed = self
                .state
                .storage
                .clock()
                .current_time()
                .delta_since(entry.seen);
            if elapsed >= chain.ownership().timeout_config.fallback_duration {
                let chain_id = chain.chain_id();
                let height = chain.tip_state.get().next_block_height;
                let key_pair = self.state.config.key_pair();
                if chain
                    .manager
                    .vote_fallback(chain_id, height, *epoch, key_pair)
                {
                    self.save().await?;
                }
            }
        }
        Ok(())
    }

    pub(super) async fn handle_pending_blob(
        &mut self,
        blob: Blob,
    ) -> Result<ChainInfoResponse, WorkerError> {
        let mut was_expected = self
            .state
            .chain
            .pending_validated_blobs
            .maybe_insert(&blob)
            .await?;
        for (_, mut pending_blobs) in self
            .state
            .chain
            .pending_proposed_blobs
            .try_load_all_entries_mut()
            .await?
        {
            if !pending_blobs.validated.get() {
                let (_, committee) = self.state.chain.current_committee()?;
                let policy = committee.policy();
                policy
                    .check_blob_size(blob.content())
                    .with_execution_context(ChainExecutionContext::Block)?;
                ensure!(
                    u64::try_from(pending_blobs.pending_blobs.count().await?)
                        .is_ok_and(|count| count < policy.maximum_published_blobs),
                    WorkerError::TooManyPublishedBlobs(policy.maximum_published_blobs)
                );
            }
            was_expected = was_expected || pending_blobs.maybe_insert(&blob).await?;
        }
        ensure!(was_expected, WorkerError::UnexpectedBlob);
        self.save().await?;
        Ok(ChainInfoResponse::new(
            &self.state.chain,
            self.state.config.key_pair(),
        ))
    }

    /// Stores the chain state in persistent storage.
    ///
    /// Waits until the [`ChainStateView`] is no longer shared before persisting the changes.
    async fn save(&mut self) -> Result<(), WorkerError> {
        self.state.clear_shared_chain_view().await;
        self.state.chain.save().await?;
        self.succeeded = true;
        Ok(())
    }
}

impl<StorageClient> Drop for ChainWorkerStateWithAttemptedChanges<'_, StorageClient>
where
    StorageClient: Storage + Clone + Send + Sync + 'static,
{
    fn drop(&mut self) {
        if !self.succeeded {
            self.state.chain.rollback();
        }
    }
}

/// Helper type for handling cross-chain updates.
pub(crate) struct CrossChainUpdateHelper<'a> {
    pub allow_messages_from_deprecated_epochs: bool,
    pub current_epoch: Epoch,
    pub committees: &'a BTreeMap<Epoch, Committee>,
}

impl<'a> CrossChainUpdateHelper<'a> {
    /// Creates a new [`CrossChainUpdateHelper`].
    pub fn new<C>(config: &ChainWorkerConfig, chain: &'a ChainStateView<C>) -> Self
    where
        C: Context + Clone + Send + Sync + 'static,
    {
        CrossChainUpdateHelper {
            allow_messages_from_deprecated_epochs: config.allow_messages_from_deprecated_epochs,
            current_epoch: *chain.execution_state.system.epoch.get(),
            committees: chain.execution_state.system.committees.get(),
        }
    }

    /// Checks basic invariants and deals with repeated heights and deprecated epochs.
    /// * Returns a range of message bundles that are both new to us and not relying on
    ///   an untrusted set of validators.
    /// * In the case of validators, if the epoch(s) of the highest bundles are not
    ///   trusted, we only accept bundles that contain messages that were already
    ///   executed by anticipation (i.e. received in certified blocks).
    /// * Basic invariants are checked for good measure. We still crucially trust
    ///   the worker of the sending chain to have verified and executed the blocks
    ///   correctly.
    pub fn select_message_bundles(
        &self,
        origin: &'a ChainId,
        recipient: ChainId,
        next_height_to_receive: BlockHeight,
        last_anticipated_block_height: Option<BlockHeight>,
        mut bundles: Vec<(Epoch, MessageBundle)>,
    ) -> Result<Vec<MessageBundle>, WorkerError> {
        let mut latest_height = None;
        let mut skipped_len = 0;
        let mut trusted_len = 0;
        for (i, (epoch, bundle)) in bundles.iter().enumerate() {
            // Make sure that heights are not decreasing.
            ensure!(
                latest_height <= Some(bundle.height),
                WorkerError::InvalidCrossChainRequest
            );
            latest_height = Some(bundle.height);
            // Check if the block has been received already.
            if bundle.height < next_height_to_receive {
                skipped_len = i + 1;
            }
            // Check if the height is trusted or the epoch is trusted.
            if self.allow_messages_from_deprecated_epochs
                || Some(bundle.height) <= last_anticipated_block_height
                || *epoch >= self.current_epoch
                || self.committees.contains_key(epoch)
            {
                trusted_len = i + 1;
            }
        }
        if skipped_len > 0 {
            let (_, sample_bundle) = &bundles[skipped_len - 1];
            debug!(
                "Ignoring repeated messages to {recipient:.8} from {origin:} at height {}",
                sample_bundle.height,
            );
        }
        if skipped_len < bundles.len() && trusted_len < bundles.len() {
            let (sample_epoch, sample_bundle) = &bundles[trusted_len];
            warn!(
                "Refusing messages to {recipient:.8} from {origin:} at height {} \
                 because the epoch {} is not trusted any more",
                sample_bundle.height, sample_epoch,
            );
        }
        let bundles = if skipped_len < trusted_len {
            bundles
                .drain(skipped_len..trusted_len)
                .map(|(_, bundle)| bundle)
                .collect()
        } else {
            vec![]
        };
        Ok(bundles)
    }
}
