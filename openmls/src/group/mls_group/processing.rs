//! Processing functions of an [`MlsGroup`] for incoming messages.

use std::mem;

use errors::{CommitToPendingProposalsError, MergePendingCommitError};
use openmls_traits::{crypto::OpenMlsCrypto, signatures::Signer, storage::StorageProvider as _};

use crate::{
    ciphersuite::Secret,
    framing::mls_content::FramedContentBody,
    group::{errors::MergeCommitError, StageCommitError, ValidationError},
    messages::group_info::GroupInfo,
    schedule::PreSharedKeyId,
    storage::OpenMlsProvider,
    tree::sender_ratchet::SenderRatchetConfiguration,
};

use super::{errors::ProcessMessageError, *};

impl MlsGroup {
    /// Parses incoming messages from the DS. Checks for syntactic errors and
    /// makes some semantic checks as well. If the input is an encrypted
    /// message, it will be decrypted. This processing function does syntactic
    /// and semantic validation of the message. It returns a [ProcessedMessage]
    /// enum.
    ///
    /// # Errors:
    /// Returns an [`ProcessMessageError`] when the validation checks fail
    /// with the exact reason of the failure.
    pub fn process_message<Provider: OpenMlsProvider>(
        &mut self,
        provider: &Provider,
        message: impl Into<ProtocolMessage>,
    ) -> Result<ProcessedMessage, ProcessMessageError> {
        let processing_state = self.init_message_processing(provider.crypto(), message)?;
        // If we keep the group reference in the processing state, we have to
        // perform the IO operations on the struct itself. We need some
        // information from the output of `init_message_processing` to perform
        // the IO operations anyway, so maybe this is a good idea? It does break
        // the pattern of just chain-calling the functions on the struct. But
        // then again, we have to perform IO operations here anyway.
        // Alternatively, we could have an extra state struct that holds both
        // the initial processing state AND the IO state. Then we could call
        // everything in a chain.
        let loaded_state = processing_state.perform_io(provider.storage())?;

        processing_state.finalize(provider.crypto(), loaded_state)
    }

    /// Stores a standalone proposal in the internal [ProposalStore]
    pub fn store_pending_proposal<Storage: StorageProvider>(
        &mut self,
        storage: &Storage,
        proposal: QueuedProposal,
    ) -> Result<(), Storage::Error> {
        storage.queue_proposal(self.group_id(), &proposal.proposal_reference(), &proposal)?;
        // Store the proposal in in the internal ProposalStore
        self.proposal_store_mut().add(proposal);

        Ok(())
    }

    /// Creates a Commit message that covers the pending proposals that are
    /// currently stored in the group's [ProposalStore]. The Commit message is
    /// created even if there are no valid pending proposals.
    ///
    /// Returns an error if there is a pending commit. Otherwise it returns a
    /// tuple of `Commit, Option<Welcome>, Option<GroupInfo>`, where `Commit`
    /// and [`Welcome`] are MlsMessages of the type [`MlsMessageOut`].
    ///
    /// [`Welcome`]: crate::messages::Welcome
    // FIXME: #1217
    #[allow(clippy::type_complexity)]
    pub fn commit_to_pending_proposals<Provider: OpenMlsProvider>(
        &mut self,
        provider: &Provider,
        signer: &impl Signer,
    ) -> Result<
        (MlsMessageOut, Option<MlsMessageOut>, Option<GroupInfo>),
        CommitToPendingProposalsError<Provider::StorageError>,
    > {
        self.is_operational()?;

        // Create Commit over all pending proposals
        // TODO #751
        let params = CreateCommitParams::builder()
            .framing_parameters(self.framing_parameters())
            .build();
        let create_commit_result = self.create_commit(params, provider, signer)?;

        // Convert PublicMessage messages to MLSMessage and encrypt them if required by
        // the configuration
        let mls_message = self.content_to_mls_message(create_commit_result.commit, provider)?;

        // Set the current group state to [`MlsGroupState::PendingCommit`],
        // storing the current [`StagedCommit`] from the commit results
        self.group_state = MlsGroupState::PendingCommit(Box::new(PendingCommitState::Member(
            create_commit_result.staged_commit,
        )));
        provider
            .storage()
            .write_group_state(self.group_id(), &self.group_state)
            .map_err(CommitToPendingProposalsError::StorageError)?;

        self.reset_aad();
        Ok((
            mls_message,
            create_commit_result
                .welcome_option
                .map(|w| MlsMessageOut::from_welcome(w, self.version())),
            create_commit_result.group_info,
        ))
    }

    /// Merge a [StagedCommit] into the group after inspection. As this advances
    /// the epoch of the group, it also clears any pending commits.
    pub fn merge_staged_commit<Provider: OpenMlsProvider>(
        &mut self,
        provider: &Provider,
        staged_commit: StagedCommit,
    ) -> Result<(), MergeCommitError<Provider::StorageError>> {
        // Check if we were removed from the group
        if staged_commit.self_removed() {
            self.group_state = MlsGroupState::Inactive;
        }
        provider
            .storage()
            .write_group_state(self.group_id(), &self.group_state)
            .map_err(MergeCommitError::StorageError)?;

        // Merge staged commit
        self.merge_commit(provider, staged_commit)?;

        // Extract and store the resumption psk for the current epoch
        let resumption_psk = self.group_epoch_secrets().resumption_psk();
        self.resumption_psk_store
            .add(self.context().epoch(), resumption_psk.clone());
        provider
            .storage()
            .write_resumption_psk_store(self.group_id(), &self.resumption_psk_store)
            .map_err(MergeCommitError::StorageError)?;

        // Delete own KeyPackageBundles
        self.own_leaf_nodes.clear();
        provider
            .storage()
            .delete_own_leaf_nodes(self.group_id())
            .map_err(MergeCommitError::StorageError)?;

        // Delete a potential pending commit
        self.clear_pending_commit(provider.storage())
            .map_err(MergeCommitError::StorageError)?;

        Ok(())
    }

    /// Merges the pending [`StagedCommit`] if there is one, and
    /// clears the field by setting it to `None`.
    pub fn merge_pending_commit<Provider: OpenMlsProvider>(
        &mut self,
        provider: &Provider,
    ) -> Result<(), MergePendingCommitError<Provider::StorageError>> {
        match &self.group_state {
            MlsGroupState::PendingCommit(_) => {
                let old_state = mem::replace(&mut self.group_state, MlsGroupState::Operational);
                if let MlsGroupState::PendingCommit(pending_commit_state) = old_state {
                    self.merge_staged_commit(provider, (*pending_commit_state).into())?;
                }
                Ok(())
            }
            MlsGroupState::Inactive => Err(MlsGroupStateError::UseAfterEviction)?,
            MlsGroupState::Operational => Ok(()),
        }
    }

    /// Helper function to read decryption keypairs.
    pub(super) fn read_decryption_keypairs(
        &self,
        storage: &impl StorageProvider,
    ) -> Result<(Vec<EncryptionKeyPair>, Vec<EncryptionKeyPair>), StageCommitError> {
        // All keys from the previous epoch are potential decryption keypairs.
        let old_epoch_keypairs = self.read_epoch_keypairs(storage);

        // If we are processing an update proposal that originally came from
        // us, the keypair corresponding to the leaf in the update is also a
        // potential decryption keypair.
        let leaf_node_keypairs = self
            .own_leaf_nodes
            .iter()
            .map(|leaf_node| {
                EncryptionKeyPair::read(storage, leaf_node.encryption_key())
                    .ok_or(StageCommitError::MissingDecryptionKey)
            })
            .collect::<Result<Vec<EncryptionKeyPair>, StageCommitError>>()?;

        Ok((old_epoch_keypairs, leaf_node_keypairs))
    }

    /// Decrypts the message and verifies its signature.
    ///
    /// Returns the [`AuthenticatedContent`] of the message, as well as the
    /// [`Credential`] of the sender if successful, or a [`ProcessMessageError`]
    /// if not.
    ///
    /// Checks the following semantic validation:
    ///  - ValSem002
    ///  - ValSem003
    ///  - ValSem006
    ///  - ValSem007 MembershipTag presence
    ///  - ValSem010
    ///  - ValSem246 (as part of ValSem010)
    pub(crate) fn decrypt_and_verify_message<Crypto: OpenMlsCrypto>(
        &mut self,
        crypto: &Crypto,
        message: impl Into<ProtocolMessage>,
    ) -> Result<(AuthenticatedContent, Credential), ProcessMessageError> {
        // Make sure we are still a member of the group
        if !self.is_active() {
            return Err(ProcessMessageError::GroupStateError(
                MlsGroupStateError::UseAfterEviction,
            ));
        }
        let message = message.into();

        // Check that handshake messages are compatible with the incoming wire format policy
        if !message.is_external()
            && message.is_handshake_message()
            && !self
                .configuration()
                .wire_format_policy()
                .incoming()
                .is_compatible_with(message.wire_format())
        {
            return Err(ProcessMessageError::IncompatibleWireFormat);
        }

        // Parse the message
        let sender_ratchet_configuration =
            self.configuration().sender_ratchet_configuration().clone();

        // Checks the following semantic validation:
        //  - ValSem002
        //  - ValSem003
        //  - ValSem006
        //  - ValSem007 MembershipTag presence
        let decrypted_message =
            self.decrypt_message(crypto, message, &sender_ratchet_configuration)?;

        let unverified_message = self
            .public_group
            .parse_message(decrypted_message, &self.message_secrets_store)
            .map_err(ProcessMessageError::from)?;

        // Checks the following semantic validation:
        //  - ValSem010
        //  - ValSem246 (as part of ValSem010)
        let (content, credential) =
            unverified_message.verify(self.ciphersuite(), crypto, self.version())?;

        Ok((content, credential))
    }

    /// This processing function does most of the semantic verifications.
    /// It returns a [ProcessedMessage] enum.
    /// Checks the following semantic validation:
    ///  - ValSem008
    ///  - ValSem101
    ///  - ValSem102
    ///  - ValSem104
    ///  - ValSem106
    ///  - ValSem107
    ///  - ValSem108
    ///  - ValSem110
    ///  - ValSem111
    ///  - ValSem112
    ///  - ValSem113: All Proposals: The proposal type must be supported by all
    ///               members of the group
    ///  - ValSem200
    ///  - ValSem201
    ///  - ValSem202: Path must be the right length
    ///  - ValSem203: Path secrets must decrypt correctly
    ///  - ValSem204: Public keys from Path must be verified and match the
    ///               private keys from the direct path
    ///  - ValSem205
    ///  - ValSem240
    ///  - ValSem241
    ///  - ValSem242
    ///  - ValSem244
    pub(crate) fn process_authenticated_content<Crypto: OpenMlsCrypto>(
        &self,
        crypto: &Crypto,
        content: AuthenticatedContent,
        credential: Credential,
        psks: Vec<(PreSharedKeyId, Secret)>,
        old_epoch_keypairs: Vec<EncryptionKeyPair>,
        leaf_node_keypairs: Vec<EncryptionKeyPair>,
    ) -> Result<ProcessedMessage, ProcessMessageError> {
        match content.sender() {
            Sender::Member(_) | Sender::NewMemberCommit | Sender::NewMemberProposal => {
                let sender = content.sender().clone();
                let authenticated_data = content.authenticated_data().to_owned();

                let content = match content.content() {
                    FramedContentBody::Application(application_message) => {
                        ProcessedMessageContent::ApplicationMessage(ApplicationMessage::new(
                            application_message.as_slice().to_owned(),
                        ))
                    }
                    FramedContentBody::Proposal(_) => {
                        let proposal = Box::new(QueuedProposal::from_authenticated_content_by_ref(
                            self.ciphersuite(),
                            crypto,
                            content,
                        )?);

                        if matches!(sender, Sender::NewMemberProposal) {
                            ProcessedMessageContent::ExternalJoinProposalMessage(proposal)
                        } else {
                            ProcessedMessageContent::ProposalMessage(proposal)
                        }
                    }
                    FramedContentBody::Commit(_) => {
                        let staged_commit = self.stage_commit(
                            &content,
                            old_epoch_keypairs,
                            leaf_node_keypairs,
                            psks,
                            crypto,
                        )?;
                        ProcessedMessageContent::StagedCommitMessage(Box::new(staged_commit))
                    }
                };

                Ok(ProcessedMessage::new(
                    self.group_id().clone(),
                    self.context().epoch(),
                    sender,
                    authenticated_data,
                    content,
                    credential,
                ))
            }
            Sender::External(_) => {
                let sender = content.sender().clone();
                let data = content.authenticated_data().to_owned();
                match content.content() {
                    FramedContentBody::Application(_) => {
                        Err(ProcessMessageError::UnauthorizedExternalApplicationMessage)
                    }
                    FramedContentBody::Proposal(Proposal::Remove(_)) => {
                        let content = ProcessedMessageContent::ProposalMessage(Box::new(
                            QueuedProposal::from_authenticated_content_by_ref(
                                self.ciphersuite(),
                                crypto,
                                content,
                            )?,
                        ));
                        Ok(ProcessedMessage::new(
                            self.group_id().clone(),
                            self.context().epoch(),
                            sender,
                            data,
                            content,
                            credential,
                        ))
                    }
                    // TODO #151/#106
                    FramedContentBody::Proposal(_) => {
                        Err(ProcessMessageError::UnsupportedProposalType)
                    }
                    FramedContentBody::Commit(_) => unimplemented!(),
                }
            }
        }
    }

    /// Performs framing validation and, if necessary, decrypts the given message.
    ///
    /// Returns the [`DecryptedMessage`] if processing is successful, or a
    /// [`ValidationError`] if it is not.
    ///
    /// Checks the following semantic validation:
    ///  - ValSem002
    ///  - ValSem003
    ///  - ValSem006
    ///  - ValSem007 MembershipTag presence
    pub(crate) fn decrypt_message(
        &mut self,
        crypto: &impl OpenMlsCrypto,
        message: ProtocolMessage,
        sender_ratchet_configuration: &SenderRatchetConfiguration,
    ) -> Result<DecryptedMessage, ValidationError> {
        // Checks the following semantic validation:
        //  - ValSem002
        //  - ValSem003
        self.public_group.validate_framing(&message)?;

        let epoch = message.epoch();

        // Checks the following semantic validation:
        //  - ValSem006
        //  - ValSem007 MembershipTag presence
        match message {
            ProtocolMessage::PublicMessage(public_message) => {
                // If the message is older than the current epoch, we need to fetch the correct secret tree first.
                let message_secrets =
                    self.message_secrets_for_epoch(epoch).map_err(|e| match e {
                        SecretTreeError::TooDistantInThePast => ValidationError::NoPastEpochData,
                        _ => LibraryError::custom(
                            "Unexpected error while retrieving message secrets for epoch.",
                        )
                        .into(),
                    })?;
                DecryptedMessage::from_inbound_public_message(
                    public_message,
                    message_secrets,
                    message_secrets.serialized_context().to_vec(),
                    crypto,
                    self.ciphersuite(),
                )
            }
            ProtocolMessage::PrivateMessage(ciphertext) => {
                // If the message is older than the current epoch, we need to fetch the correct secret tree first
                DecryptedMessage::from_inbound_ciphertext(
                    ciphertext,
                    crypto,
                    self,
                    sender_ratchet_configuration,
                )
            }
        }
    }
}
