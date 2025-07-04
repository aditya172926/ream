use std::{
    collections::{HashMap, hash_map::Entry},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
    vec,
};

use alloy_primitives::Address;
use anyhow::{anyhow, bail};
use ream_beacon_api_types::{
    block::{BroadcastValidation, ProduceBlockData},
    duties::{AttesterDuty, ProposerDuty, SyncCommitteeDuty},
    id::{ID, ValidatorID},
    request::SyncCommitteeRequestItem,
};
use ream_bls::{PublicKey, traits::Signable};
use ream_consensus::{
    attestation_data::AttestationData,
    constants::DOMAIN_SYNC_COMMITTEE,
    electra::beacon_state::BeaconState,
    misc::{compute_domain, compute_epoch_at_slot, compute_signing_root},
    single_attestation::SingleAttestation,
};
use ream_executor::ReamExecutor;
use ream_keystore::keystore::Keystore;
use ream_network_spec::networks::network_spec;
use reqwest::Url;
use tokio::time::{Instant, MissedTickBehavior, interval_at};
use tracing::{error, info, warn};
use tree_hash::TreeHash;

use crate::{
    aggregate_and_proof::{AggregateAndProof, SignedAggregateAndProof, sign_aggregate_and_proof},
    attestation::{get_selection_proof, sign_attestation_data},
    beacon_api_client::BeaconApiClient,
    block::{sign_beacon_block, sign_blinded_beacon_block},
    randao::sign_randao_reveal,
};

pub fn check_if_validator_active(
    state: &BeaconState,
    validator_index: u64,
) -> anyhow::Result<bool> {
    state
        .validators
        .get(validator_index as usize)
        .map(|validator| validator.is_active_validator(state.get_current_epoch()))
        .ok_or_else(|| anyhow!("Validator index out of bounds"))
}

pub fn is_proposer(state: &BeaconState, validator_index: u64) -> anyhow::Result<bool> {
    Ok(state.get_beacon_proposer_index(None)? == validator_index)
}

pub struct ValidatorService {
    pub beacon_api_client: Arc<BeaconApiClient>,
    pub validators: Vec<Arc<Keystore>>,
    pub suggested_fee_recipient: Arc<Address>,
    pub executor: ReamExecutor,
    pub active_validator_count: usize,
    pub public_key_to_index: HashMap<PublicKey, u64>,
    pub validator_index_to_keystore: HashMap<u64, Arc<Keystore>>,
    pub proposer_duties: Vec<ProposerDuty>,
    pub attester_duties: Vec<AttesterDuty>,
    pub sync_committee_duties: Vec<SyncCommitteeDuty>,
}

impl ValidatorService {
    pub fn new(
        keystores: Vec<Keystore>,
        suggested_fee_recipient: Address,
        beacon_api_endpoint: Url,
        request_timeout: Duration,
        executor: ReamExecutor,
    ) -> anyhow::Result<Self> {
        let validators = keystores.into_iter().map(Arc::new).collect::<Vec<_>>();

        Ok(Self {
            beacon_api_client: Arc::new(BeaconApiClient::new(
                beacon_api_endpoint,
                request_timeout,
            )?),
            validators,
            suggested_fee_recipient: Arc::new(suggested_fee_recipient),
            executor,
            active_validator_count: 0,
            public_key_to_index: HashMap::new(),
            validator_index_to_keystore: HashMap::new(),
            proposer_duties: Vec::new(),
            attester_duties: Vec::new(),
            sync_committee_duties: Vec::new(),
        })
    }

    pub async fn start(mut self) {
        let genesis_info = self
            .beacon_api_client
            .get_genesis()
            .await
            .expect("Could not retrieve genesis information");

        let seconds_per_slot = network_spec().seconds_per_slot;
        let genesis_instant = UNIX_EPOCH + Duration::from_secs(genesis_info.data.genesis_time);
        let elapsed = SystemTime::now()
            .duration_since(genesis_instant)
            .expect("System Time is before the genesis time");

        let mut slot = elapsed.as_secs() / seconds_per_slot;
        let mut epoch = compute_epoch_at_slot(slot);

        let mut interval = {
            let interval_start =
                Instant::now() - (elapsed - Duration::from_secs(slot * seconds_per_slot));
            interval_at(interval_start, Duration::from_secs(seconds_per_slot))
        };
        interval.set_missed_tick_behavior(MissedTickBehavior::Burst);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    slot += 1;
                    let current_epoch = compute_epoch_at_slot(slot);

                    if current_epoch != epoch {
                        epoch = current_epoch;
                        self.on_epoch(epoch).await;
                    }
                    self.on_slot(slot);
                }
            }
        }
    }

    pub fn on_slot(&self, slot: u64) {
        info!("Current Slot: {slot}");
    }

    pub async fn fetch_validator_indicies(&mut self) {
        if self.active_validator_count < self.validators.len() {
            let validator_states = self
                .beacon_api_client
                .get_state_validator_list(
                    ID::Head,
                    Some(
                        self.validators
                            .iter()
                            .map(|validator_info| {
                                ValidatorID::Address(validator_info.public_key.clone())
                            })
                            .collect::<Vec<_>>(),
                    ),
                    None,
                )
                .await;

            if let Ok(validator_infos) = validator_states {
                validator_infos.data.into_iter().for_each(|validator_data| {
                    if let Entry::Vacant(entry) = self
                        .public_key_to_index
                        .entry(validator_data.validator.public_key.clone())
                    {
                        entry.insert(validator_data.index);

                        if let Some(keystore) = self
                            .validators
                            .iter()
                            .find(|keystore| {
                                keystore.public_key == validator_data.validator.public_key
                            })
                            .cloned()
                        {
                            self.validator_index_to_keystore
                                .insert(validator_data.index, keystore);
                        }

                        self.active_validator_count += 1;
                    }
                });
            }
        }
    }

    pub async fn fetch_duties(&mut self, epoch: u64) {
        let validator_indices: Vec<u64> = self.public_key_to_index.values().cloned().collect();

        if validator_indices.is_empty() {
            warn!("No active validators found, skipping duty fetch");
            return;
        }

        self.fetch_proposer_duties(epoch, &validator_indices).await;
        self.fetch_attester_duties(epoch + 1, &validator_indices)
            .await;
        self.fetch_sync_committee_duties(epoch, &validator_indices)
            .await;
    }

    pub async fn fetch_proposer_duties(&mut self, epoch: u64, validator_indices: &[u64]) {
        match self.beacon_api_client.get_proposer_duties(epoch).await {
            Ok(duties_response) => {
                self.proposer_duties = duties_response
                    .data
                    .into_iter()
                    .filter(|duty| validator_indices.contains(&duty.validator_index))
                    .collect();
            }
            Err(err) => {
                error!("Failed to fetch proposer duties for epoch {epoch}: {err:?}");
            }
        }
    }

    pub async fn fetch_attester_duties(&mut self, epoch: u64, validator_indices: &[u64]) {
        match self
            .beacon_api_client
            .get_attester_duties(epoch, validator_indices)
            .await
        {
            Ok(duties_response) => {
                self.attester_duties = duties_response.data;
            }
            Err(err) => {
                error!("Failed to fetch attester duties for epoch {epoch}: {err:?}");
            }
        }
    }

    pub async fn fetch_sync_committee_duties(&mut self, epoch: u64, validator_indices: &[u64]) {
        match self
            .beacon_api_client
            .get_sync_committee_duties(epoch, validator_indices)
            .await
        {
            Ok(duties_response) => {
                self.sync_committee_duties = duties_response.data;
            }
            Err(err) => {
                error!("Failed to fetch sync committee duties for epoch {epoch}: {err:?}");
            }
        }
    }

    pub async fn propose_block(&self, slot: u64, validator_index: u64) -> anyhow::Result<()> {
        let keystore = self
            .validator_index_to_keystore
            .get(&validator_index)
            .cloned()
            .ok_or_else(|| anyhow!("Keystore not found for validator: {validator_index}"))?;
        let randao_reveal = sign_randao_reveal(slot, &keystore.private_key)?;
        let block_response = self
            .beacon_api_client
            .produce_block(slot, randao_reveal, None, None, None)
            .await?;

        match block_response.data {
            ProduceBlockData::Full(full_block) => {
                let signed_beacon_block =
                    sign_beacon_block(slot, full_block.block, &keystore.private_key)?;

                self.beacon_api_client
                    .publish_block(BroadcastValidation::Gossip, signed_beacon_block)
                    .await?;
            }
            ProduceBlockData::Blinded(blinded_block) => {
                let signed_blinded_block =
                    sign_blinded_beacon_block(slot, blinded_block, &keystore.private_key)?;

                self.beacon_api_client
                    .publish_blinded_block(BroadcastValidation::Gossip, signed_blinded_block)
                    .await?;
            }
        }

        Ok(())
    }

    pub async fn submit_sync_committee(
        &self,
        slot: u64,
        validator_indices: &[u64],
    ) -> anyhow::Result<()> {
        let domain = compute_domain(
            DOMAIN_SYNC_COMMITTEE,
            Some(network_spec().electra_fork_version),
            None,
        );
        let beacon_block_root = self
            .beacon_api_client
            .get_block_root(ID::Slot(slot))
            .await?
            .data
            .root;
        let signing_root = compute_signing_root(beacon_block_root, domain);

        let payload = validator_indices
            .iter()
            .filter_map(|&validator_index| {
                if let Some(keystore) = self.validator_index_to_keystore.get(&validator_index) {
                    return match keystore.private_key.sign(signing_root.as_ref()) {
                        Ok(signature) => Some(Ok(SyncCommitteeRequestItem {
                            slot,
                            beacon_block_root,
                            validator_index,
                            signature,
                        })),
                        Err(signing_error) => Some(Err(anyhow!(
                            "Signing failed for validator {validator_index:?}: {signing_error:?}"
                        ))),
                    };
                }
                None
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(self
            .beacon_api_client
            .publish_sync_committee_signature(payload)
            .await?)
    }

    pub async fn make_attestation(
        &self,
        slot: u64,
        validator_index: u64,
        committee_index: u64,
    ) -> anyhow::Result<()> {
        let Some(keystore) = self.validator_index_to_keystore.get(&validator_index) else {
            bail!("Keystore not found for validator: {validator_index}");
        };

        let attestation_data = self
            .beacon_api_client
            .get_attestation_data(slot, committee_index)
            .await?
            .data;
        Ok(self
            .beacon_api_client
            .submit_attestation(vec![SingleAttestation {
                attester_index: validator_index,
                committee_index,
                signature: sign_attestation_data(&attestation_data, &keystore.private_key)?,
                data: attestation_data,
            }])
            .await?)
    }

    pub async fn submit_aggregate_and_proof(
        &self,
        attestation_data: AttestationData,
        slot: u64,
        committee_index: u64,
        aggregator_index: u64,
    ) -> anyhow::Result<()> {
        let keystore = self
            .validator_index_to_keystore
            .get(&aggregator_index)
            .cloned()
            .ok_or_else(|| anyhow!("Keystore not found for validator: {aggregator_index}"))?;

        let aggregate_and_proof = AggregateAndProof {
            aggregator_index,
            aggregate: self
                .beacon_api_client
                .get_aggregated_attestation(
                    attestation_data.tree_hash_root(),
                    slot,
                    committee_index,
                )
                .await?
                .data,
            selection_proof: get_selection_proof(slot, &keystore.private_key)?,
        };

        Ok(self
            .beacon_api_client
            .publish_aggregate_and_proofs(vec![SignedAggregateAndProof {
                signature: sign_aggregate_and_proof(&aggregate_and_proof, &keystore.private_key)?,
                message: aggregate_and_proof,
            }])
            .await?)
    }

    pub async fn on_epoch(&mut self, epoch: u64) {
        self.fetch_validator_indicies().await;
        info!("Current Epoch: {epoch}");
    }
}
