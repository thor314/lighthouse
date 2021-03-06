use crate::validator_store::ValidatorStore;
use environment::RuntimeContext;
use exit_future::Signal;
use futures::{future, Future, IntoFuture, Stream};
use parking_lot::RwLock;
use remote_beacon_node::{PublishStatus, RemoteBeaconNode};
use rest_types::{ValidatorDuty, ValidatorDutyBytes, ValidatorSubscription};
use slog::{crit, debug, error, info, trace, warn};
use slot_clock::SlotClock;
use std::collections::HashMap;
use std::convert::TryInto;
use std::ops::Deref;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::timer::Interval;
use types::{ChainSpec, CommitteeIndex, Epoch, EthSpec, PublicKey, SelectionProof, Slot};

/// Delay this period of time after the slot starts. This allows the node to process the new slot.
const TIME_DELAY_FROM_SLOT: Duration = Duration::from_millis(100);

/// Remove any duties where the `duties_epoch < current_epoch - PRUNE_DEPTH`.
const PRUNE_DEPTH: u64 = 4;

type BaseHashMap = HashMap<PublicKey, HashMap<Epoch, DutyAndProof>>;

#[derive(Debug, Clone)]
pub struct DutyAndProof {
    /// The validator duty.
    pub duty: ValidatorDuty,
    /// Stores the selection proof if the duty elects the validator to be an aggregator.
    pub selection_proof: Option<SelectionProof>,
}

impl DutyAndProof {
    /// Computes the selection proof for `self.validator_pubkey` and `self.duty.attestation_slot`,
    /// storing it in `self.selection_proof` _if_ the validator is an aggregator. If the validator
    /// is not an aggregator, `self.selection_proof` is set to `None`.
    ///
    /// ## Errors
    ///
    /// - `self.validator_pubkey` is not known in `validator_store`.
    /// - There's an arith error during computation.
    pub fn compute_selection_proof<T: SlotClock + 'static, E: EthSpec>(
        &mut self,
        validator_store: &ValidatorStore<T, E>,
    ) -> Result<(), String> {
        let (modulo, slot) = if let (Some(modulo), Some(slot)) =
            (self.duty.aggregator_modulo, self.duty.attestation_slot)
        {
            (modulo, slot)
        } else {
            // If there is no modulo or for the aggregator we assume they are not activated and
            // therefore not an aggregator.
            self.selection_proof = None;
            return Ok(());
        };

        let selection_proof = validator_store
            .produce_selection_proof(&self.duty.validator_pubkey, slot)
            .ok_or_else(|| "Validator pubkey missing from store".to_string())?;

        self.selection_proof = selection_proof
            .is_aggregator_from_modulo(modulo)
            .map_err(|e| format!("Invalid modulo: {:?}", e))
            .map(|is_aggregator| {
                if is_aggregator {
                    Some(selection_proof)
                } else {
                    None
                }
            })?;

        Ok(())
    }

    /// Returns `true` if the two `Self` instances would result in the same beacon subscription.
    pub fn subscription_eq(&self, other: &Self) -> bool {
        self.selection_proof_eq(other)
            && self.duty.validator_index == other.duty.validator_index
            && self.duty.attestation_committee_index == other.duty.attestation_committee_index
            && self.duty.attestation_slot == other.duty.attestation_slot
    }

    /// Returns `true` if the selection proof between `self` and `other` _should_ be equal.
    ///
    /// It's important to note that this doesn't actually check `self.selection_proof`, instead it
    /// checks to see if the inputs to computing the selection proof are equal.
    fn selection_proof_eq(&self, other: &Self) -> bool {
        self.duty.aggregator_modulo == other.duty.aggregator_modulo
            && self.duty.attestation_slot == other.duty.attestation_slot
    }

    /// Returns the information required for an attesting validator, if they are scheduled to
    /// attest.
    pub fn attestation_duties(&self) -> Option<(Slot, CommitteeIndex, usize, u64)> {
        Some((
            self.duty.attestation_slot?,
            self.duty.attestation_committee_index?,
            self.duty.attestation_committee_position?,
            self.duty.validator_index?,
        ))
    }

    pub fn validator_pubkey(&self) -> &PublicKey {
        &self.duty.validator_pubkey
    }
}

impl TryInto<DutyAndProof> for ValidatorDutyBytes {
    type Error = String;

    fn try_into(self) -> Result<DutyAndProof, Self::Error> {
        let duty = ValidatorDuty {
            validator_pubkey: (&self.validator_pubkey)
                .try_into()
                .map_err(|e| format!("Invalid pubkey bytes from server: {:?}", e))?,
            validator_index: self.validator_index,
            attestation_slot: self.attestation_slot,
            attestation_committee_index: self.attestation_committee_index,
            attestation_committee_position: self.attestation_committee_position,
            block_proposal_slots: self.block_proposal_slots,
            aggregator_modulo: self.aggregator_modulo,
        };
        Ok(DutyAndProof {
            duty,
            selection_proof: None,
        })
    }
}

/// The outcome of inserting some `ValidatorDuty` into the `DutiesStore`.
enum InsertOutcome {
    /// These are the first duties received for this validator.
    NewValidator,
    /// The duties for this given epoch were previously unknown and have been stored.
    NewEpoch,
    /// The duties were identical to some already in the store.
    Identical,
    /// There were duties for this validator and epoch in the store that were different to the ones
    /// provided. The existing duties were replaced.
    Replaced { should_resubscribe: bool },
    /// The given duties were invalid.
    Invalid,
}

impl InsertOutcome {
    /// Returns `true` if the outcome indicates that the validator _might_ require a subscription.
    pub fn is_subscription_candidate(self) -> bool {
        match self {
            InsertOutcome::Replaced { should_resubscribe } => should_resubscribe,
            InsertOutcome::NewValidator => true,
            InsertOutcome::NewEpoch => true,
            InsertOutcome::Identical => false,
            InsertOutcome::Invalid => false,
        }
    }
}

#[derive(Default)]
pub struct DutiesStore {
    store: RwLock<BaseHashMap>,
}

impl DutiesStore {
    /// Returns the total number of validators that should propose in the given epoch.
    fn proposer_count(&self, epoch: Epoch) -> usize {
        self.store
            .read()
            .iter()
            .filter(|(_validator_pubkey, validator_map)| {
                validator_map
                    .get(&epoch)
                    .map(|duties| !duties.duty.block_proposal_slots.is_empty())
                    .unwrap_or_else(|| false)
            })
            .count()
    }

    /// Returns the total number of validators that should attest in the given epoch.
    fn attester_count(&self, epoch: Epoch) -> usize {
        self.store
            .read()
            .iter()
            .filter(|(_validator_pubkey, validator_map)| {
                validator_map
                    .get(&epoch)
                    .map(|duties| duties.duty.attestation_slot.is_some())
                    .unwrap_or_else(|| false)
            })
            .count()
    }

    fn block_producers(&self, slot: Slot, slots_per_epoch: u64) -> Vec<PublicKey> {
        self.store
            .read()
            .iter()
            // As long as a `HashMap` iterator does not return duplicate keys, neither will this
            // function.
            .filter_map(|(_validator_pubkey, validator_map)| {
                let epoch = slot.epoch(slots_per_epoch);

                validator_map.get(&epoch).and_then(|duties| {
                    if duties.duty.block_proposal_slots.contains(&slot) {
                        Some(duties.duty.validator_pubkey.clone())
                    } else {
                        None
                    }
                })
            })
            .collect()
    }

    fn attesters(&self, slot: Slot, slots_per_epoch: u64) -> Vec<DutyAndProof> {
        self.store
            .read()
            .iter()
            // As long as a `HashMap` iterator does not return duplicate keys, neither will this
            // function.
            .filter_map(|(_validator_pubkey, validator_map)| {
                let epoch = slot.epoch(slots_per_epoch);

                validator_map.get(&epoch).and_then(|duties| {
                    if duties.duty.attestation_slot == Some(slot) {
                        Some(duties)
                    } else {
                        None
                    }
                })
            })
            .cloned()
            .collect()
    }

    fn insert<T: SlotClock + 'static, E: EthSpec>(
        &self,
        epoch: Epoch,
        mut duties: DutyAndProof,
        slots_per_epoch: u64,
        validator_store: &ValidatorStore<T, E>,
    ) -> Result<InsertOutcome, String> {
        let mut store = self.store.write();

        if !duties_match_epoch(&duties.duty, epoch, slots_per_epoch) {
            return Ok(InsertOutcome::Invalid);
        }

        // TODO: refactor with Entry.

        if let Some(validator_map) = store.get_mut(&duties.duty.validator_pubkey) {
            if let Some(known_duties) = validator_map.get_mut(&epoch) {
                if known_duties.duty == duties.duty {
                    Ok(InsertOutcome::Identical)
                } else {
                    // Compute the selection proof.
                    duties.compute_selection_proof(validator_store)?;

                    // Determine if a re-subscription is required.
                    let should_resubscribe = duties.subscription_eq(known_duties);

                    // Replace the existing duties.
                    *known_duties = duties;

                    Ok(InsertOutcome::Replaced { should_resubscribe })
                }
            } else {
                // Compute the selection proof.
                duties.compute_selection_proof(validator_store)?;

                validator_map.insert(epoch, duties);

                Ok(InsertOutcome::NewEpoch)
            }
        } else {
            // Compute the selection proof.
            duties.compute_selection_proof(validator_store)?;

            let validator_pubkey = duties.duty.validator_pubkey.clone();

            let mut validator_map = HashMap::new();
            validator_map.insert(epoch, duties);

            store.insert(validator_pubkey, validator_map);

            Ok(InsertOutcome::NewValidator)
        }
    }

    fn prune(&self, prior_to: Epoch) {
        self.store
            .write()
            .retain(|_validator_pubkey, validator_map| {
                validator_map.retain(|epoch, _duties| *epoch >= prior_to);
                !validator_map.is_empty()
            });
    }
}

pub struct DutiesServiceBuilder<T, E: EthSpec> {
    validator_store: Option<ValidatorStore<T, E>>,
    slot_clock: Option<T>,
    beacon_node: Option<RemoteBeaconNode<E>>,
    context: Option<RuntimeContext<E>>,
    allow_unsynced_beacon_node: bool,
}

impl<T: SlotClock + 'static, E: EthSpec> DutiesServiceBuilder<T, E> {
    pub fn new() -> Self {
        Self {
            validator_store: None,
            slot_clock: None,
            beacon_node: None,
            context: None,
            allow_unsynced_beacon_node: false,
        }
    }

    pub fn validator_store(mut self, store: ValidatorStore<T, E>) -> Self {
        self.validator_store = Some(store);
        self
    }

    pub fn slot_clock(mut self, slot_clock: T) -> Self {
        self.slot_clock = Some(slot_clock);
        self
    }

    pub fn beacon_node(mut self, beacon_node: RemoteBeaconNode<E>) -> Self {
        self.beacon_node = Some(beacon_node);
        self
    }

    pub fn runtime_context(mut self, context: RuntimeContext<E>) -> Self {
        self.context = Some(context);
        self
    }

    /// Set to `true` to allow polling for duties when the beacon node is not synced.
    pub fn allow_unsynced_beacon_node(mut self, allow_unsynced_beacon_node: bool) -> Self {
        self.allow_unsynced_beacon_node = allow_unsynced_beacon_node;
        self
    }

    pub fn build(self) -> Result<DutiesService<T, E>, String> {
        Ok(DutiesService {
            inner: Arc::new(Inner {
                store: Arc::new(DutiesStore::default()),
                validator_store: self
                    .validator_store
                    .ok_or_else(|| "Cannot build DutiesService without validator_store")?,
                slot_clock: self
                    .slot_clock
                    .ok_or_else(|| "Cannot build DutiesService without slot_clock")?,
                beacon_node: self
                    .beacon_node
                    .ok_or_else(|| "Cannot build DutiesService without beacon_node")?,
                context: self
                    .context
                    .ok_or_else(|| "Cannot build DutiesService without runtime_context")?,
                allow_unsynced_beacon_node: self.allow_unsynced_beacon_node,
            }),
        })
    }
}

/// Helper to minimise `Arc` usage.
pub struct Inner<T, E: EthSpec> {
    store: Arc<DutiesStore>,
    validator_store: ValidatorStore<T, E>,
    pub(crate) slot_clock: T,
    beacon_node: RemoteBeaconNode<E>,
    context: RuntimeContext<E>,
    /// If true, the duties service will poll for duties from the beacon node even if it is not
    /// synced.
    allow_unsynced_beacon_node: bool,
}

/// Maintains a store of the duties for all voting validators in the `validator_store`.
///
/// Polls the beacon node at the start of each epoch, collecting duties for the current and next
/// epoch.
pub struct DutiesService<T, E: EthSpec> {
    inner: Arc<Inner<T, E>>,
}

impl<T, E: EthSpec> Clone for DutiesService<T, E> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T, E: EthSpec> Deref for DutiesService<T, E> {
    type Target = Inner<T, E>;

    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

impl<T: SlotClock + 'static, E: EthSpec> DutiesService<T, E> {
    /// Returns the total number of validators known to the duties service.
    pub fn total_validator_count(&self) -> usize {
        self.validator_store.num_voting_validators()
    }

    /// Returns the total number of validators that should propose in the given epoch.
    pub fn proposer_count(&self, epoch: Epoch) -> usize {
        self.store.proposer_count(epoch)
    }

    /// Returns the total number of validators that should attest in the given epoch.
    pub fn attester_count(&self, epoch: Epoch) -> usize {
        self.store.attester_count(epoch)
    }

    /// Returns the pubkeys of the validators which are assigned to propose in the given slot.
    ///
    /// In normal cases, there should be 0 or 1 validators returned. In extreme cases (i.e., deep forking)
    ///
    /// It is possible that multiple validators have an identical proposal slot, however that is
    /// likely the result of heavy forking (lol) or inconsistent beacon node connections.
    pub fn block_producers(&self, slot: Slot) -> Vec<PublicKey> {
        self.store.block_producers(slot, E::slots_per_epoch())
    }

    /// Returns all `ValidatorDuty` for the given `slot`.
    pub fn attesters(&self, slot: Slot) -> Vec<DutyAndProof> {
        self.store.attesters(slot, E::slots_per_epoch())
    }

    /// Start the service that periodically polls the beacon node for validator duties.
    pub fn start_update_service(&self, spec: &ChainSpec) -> Result<Signal, String> {
        let log = self.context.log.clone();

        let duration_to_next_slot = self
            .slot_clock
            .duration_to_next_slot()
            .ok_or_else(|| "Unable to determine duration to next slot".to_string())?;

        let interval = {
            let slot_duration = Duration::from_millis(spec.milliseconds_per_slot);
            Interval::new(
                Instant::now() + duration_to_next_slot + TIME_DELAY_FROM_SLOT,
                slot_duration,
            )
        };

        let (exit_signal, exit_fut) = exit_future::signal();
        let service = self.clone();
        let log_1 = log.clone();
        let log_2 = log.clone();

        // Run an immediate update before starting the updater service.
        self.context.executor.spawn(service.clone().do_update());

        self.context.executor.spawn(
            exit_fut
                .until(
                    interval
                        .map_err(move |e| {
                            crit! {
                                log_1,
                                "Timer thread failed";
                                "error" => format!("{}", e)
                            }
                        })
                        .for_each(move |_| service.clone().do_update().then(|_| Ok(()))),
                )
                .map(move |_| info!(log_2, "Shutdown complete")),
        );

        Ok(exit_signal)
    }

    /// Attempt to download the duties of all managed validators for this epoch and the next.
    fn do_update(&self) -> impl Future<Item = (), Error = ()> {
        let service_1 = self.clone();
        let service_2 = self.clone();
        let service_3 = self.clone();
        let service_4 = self.clone();
        let log_1 = self.context.log.clone();
        let log_2 = self.context.log.clone();

        self.slot_clock
            .now()
            .ok_or_else(move || {
                error!(log_1, "Duties manager failed to read slot clock");
            })
            .into_future()
            .map(move |slot| {
                let epoch = slot.epoch(E::slots_per_epoch());

                if slot % E::slots_per_epoch() == 0 {
                    let prune_below = epoch - PRUNE_DEPTH;

                    trace!(
                        log_2,
                        "Pruning duties cache";
                        "pruning_below" => prune_below.as_u64(),
                        "current_epoch" => epoch.as_u64(),
                    );

                    service_1.store.prune(prune_below);
                }

                epoch
            })
            .and_then(move |epoch| {
                let log = service_2.context.log.clone();

                service_2
                    .beacon_node
                    .http
                    .beacon()
                    .get_head()
                    .map(move |head| (epoch, head.slot.epoch(E::slots_per_epoch())))
                    .map_err(move |e| {
                        error!(
                                log,
                                "Failed to contact beacon node";
                                "error" => format!("{:?}", e)
                        )
                    })
            })
            .and_then(move |(current_epoch, beacon_head_epoch)| {
                let log = service_3.context.log.clone();

                let future: Box<dyn Future<Item = (), Error = ()> + Send> = if beacon_head_epoch + 1
                    < current_epoch
                    && !service_3.allow_unsynced_beacon_node
                {
                    error!(
                        log,
                        "Beacon node is not synced";
                        "node_head_epoch" => format!("{}", beacon_head_epoch),
                        "current_epoch" => format!("{}", current_epoch),
                    );

                    Box::new(future::ok(()))
                } else {
                    Box::new(service_3.update_epoch(current_epoch).then(move |result| {
                        if let Err(e) = result {
                            error!(
                                log,
                                "Failed to get current epoch duties";
                                "http_error" => format!("{:?}", e)
                            );
                        }

                        let log = service_4.context.log.clone();
                        service_4.update_epoch(current_epoch + 1).map_err(move |e| {
                            error!(
                                log,
                                "Failed to get next epoch duties";
                                "http_error" => format!("{:?}", e)
                            );
                        })
                    }))
                };

                future
            })
            .map(|_| ())
    }

    /// Attempt to download the duties of all managed validators for the given `epoch`.
    fn update_epoch(self, epoch: Epoch) -> impl Future<Item = (), Error = String> {
        let service_1 = self.clone();
        let service_2 = self.clone();
        let service_3 = self;

        let pubkeys = service_1.validator_store.voting_pubkeys();
        service_1
            .beacon_node
            .http
            .validator()
            .get_duties(epoch, pubkeys.as_slice())
            .map(move |all_duties| (epoch, all_duties))
            .map_err(move |e| format!("Failed to get duties for epoch {}: {:?}", epoch, e))
            .and_then(move |(epoch, all_duties)| {
                let log = service_2.context.log.clone();

                let mut new_validator = 0;
                let mut new_epoch = 0;
                let mut identical = 0;
                let mut replaced = 0;
                let mut invalid = 0;

                // For each of the duties, attempt to insert them into our local store and build a
                // list of new or changed selections proofs for any aggregating validators.
                let validator_subscriptions = all_duties.into_iter().filter_map(|remote_duties| {
                    // Convert the remote duties into our local representation.
                    let duties: DutyAndProof = remote_duties
                        .try_into()
                        .map_err(|e| error!(
                            log,
                            "Unable to convert remote duties";
                            "error" => e
                        ))
                        .ok()?;

                    // Attempt to update our local store.
                    let outcome = service_2
                        .store
                        .insert(epoch, duties.clone(), E::slots_per_epoch(), &service_2.validator_store)
                        .map_err(|e| error!(
                            log,
                            "Unable to store duties";
                            "error" => e
                        ))
                        .ok()?;

                    match &outcome {
                        InsertOutcome::NewValidator => {
                            debug!(
                                log,
                                "First duty assignment for validator";
                                "proposal_slots" => format!("{:?}", &duties.duty.block_proposal_slots),
                                "attestation_slot" => format!("{:?}", &duties.duty.attestation_slot),
                                "validator" => format!("{:?}", &duties.duty.validator_pubkey)
                            );
                            new_validator += 1;
                        }
                        InsertOutcome::NewEpoch => new_epoch += 1,
                        InsertOutcome::Identical => identical += 1,
                        InsertOutcome::Replaced { .. } => replaced += 1,
                        InsertOutcome::Invalid => invalid += 1,
                    };

                    if outcome.is_subscription_candidate() {
                        Some(ValidatorSubscription {
                            validator_index: duties.duty.validator_index?,
                            attestation_committee_index: duties.duty.attestation_committee_index?,
                            slot: duties.duty.attestation_slot?,
                            is_aggregator: duties.selection_proof.is_some(),
                        })
                    } else {
                        None
                    }
                }).collect::<Vec<_>>();

                if invalid > 0 {
                    error!(
                        log,
                        "Received invalid duties from beacon node";
                        "bad_duty_count" => invalid,
                        "info" => "Duties are from wrong epoch."
                    )
                }

                trace!(
                    log,
                    "Performed duties update";
                    "identical" => identical,
                    "new_epoch" => new_epoch,
                    "new_validator" => new_validator,
                    "replaced" => replaced,
                    "epoch" => format!("{}", epoch)
                );

                if replaced > 0 {
                    warn!(
                        log,
                        "Duties changed during routine update";
                        "info" => "Chain re-org likely occurred."
                    )
                }

                Ok(validator_subscriptions)
            })
            .and_then::<_, Box<dyn Future<Item = _, Error = _> + Send>>(move |validator_subscriptions| {
                let log = service_3.context.log.clone();
                let count = validator_subscriptions.len();

                if count == 0 {
                    debug!(
                        log,
                        "No new subscriptions required"
                    );

                    Box::new(future::ok(()))
                } else {
                    Box::new(service_3.beacon_node
                        .http
                        .validator()
                        .subscribe(validator_subscriptions)
                        .map_err(|e| format!("Failed to subscribe validators: {:?}", e))
                        .map(move |status| {
                            match status {
                                PublishStatus::Valid => {
                                    debug!(
                                        log,
                                        "Successfully subscribed validators";
                                        "count" => count
                                    )
                                },
                                PublishStatus::Unknown => {
                                    error!(
                                        log,
                                        "Unknown response from subscription";
                                    )
                                },
                                PublishStatus::Invalid(e) => {
                                    error!(
                                        log,
                                        "Failed to subscribe validator";
                                        "error" => e
                                    )
                                },
                            };
                        }))
                }

            })
    }
}

/// Returns `true` if the slots in the `duties` are from the given `epoch`
fn duties_match_epoch(duties: &ValidatorDuty, epoch: Epoch, slots_per_epoch: u64) -> bool {
    duties
        .attestation_slot
        .map_or(true, |slot| slot.epoch(slots_per_epoch) == epoch)
        && duties
            .block_proposal_slots
            .iter()
            .all(|slot| slot.epoch(slots_per_epoch) == epoch)
}
