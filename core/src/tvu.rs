//! The `tvu` module implements the Transaction Validation Unit, a multi-stage transaction
//! validation pipeline in software.

use crate::{
    accounts_background_service::AccountsBackgroundService,
    accounts_hash_verifier::AccountsHashVerifier,
    broadcast_stage::RetransmitSlotsSender,
    cluster_info::ClusterInfo,
    cluster_info_vote_listener::{VerifiedVoteReceiver, VoteTracker},
    cluster_slots::ClusterSlots,
    ledger_cleanup_service::LedgerCleanupService,
    poh_recorder::PohRecorder,
    replay_stage::{ReplayStage, ReplayStageConfig},
    retransmit_stage::RetransmitStage,
    rewards_recorder_service::RewardsRecorderSender,
    rpc_subscriptions::RpcSubscriptions,
    shred_fetch_stage::ShredFetchStage,
    sigverify_shreds::ShredSigVerifier,
    sigverify_stage::SigVerifyStage,
};
use crossbeam_channel::unbounded;
use solana_ledger::{
    blockstore::{Blockstore, CompletedSlotsReceiver},
    blockstore_processor::TransactionStatusSender,
    leader_schedule_cache::LeaderScheduleCache,
};
use solana_runtime::{
    bank_forks::BankForks, commitment::BlockCommitmentCache,
    snapshot_package::AccountsPackageSender, vote_sender_types::ReplayVoteSender,
};
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};
use std::{
    collections::HashSet,
    net::UdpSocket,
    sync::{
        atomic::AtomicBool,
        mpsc::{channel, Receiver},
        Arc, Mutex, RwLock,
    },
    thread,
};

pub struct Tvu {
    fetch_stage: ShredFetchStage,
    sigverify_stage: SigVerifyStage,
    retransmit_stage: RetransmitStage,
    replay_stage: ReplayStage,
    ledger_cleanup_service: Option<LedgerCleanupService>,
    accounts_background_service: AccountsBackgroundService,
    accounts_hash_verifier: AccountsHashVerifier,
}

pub struct Sockets {
    pub fetch: Vec<UdpSocket>,
    pub repair: UdpSocket,
    pub retransmit: Vec<UdpSocket>,
    pub forwards: Vec<UdpSocket>,
}

#[derive(Default)]
pub struct TvuConfig {
    pub max_ledger_shreds: Option<u64>,
    pub shred_version: u16,
    pub halt_on_trusted_validators_accounts_hash_mismatch: bool,
    pub trusted_validators: Option<HashSet<Pubkey>>,
    pub repair_validators: Option<HashSet<Pubkey>>,
    pub accounts_hash_fault_injection_slots: u64,
}

impl Tvu {
    /// This service receives messages from a leader in the network and processes the transactions
    /// on the bank state.
    /// # Arguments
    /// * `cluster_info` - The cluster_info state.
    /// * `sockets` - fetch, repair, and retransmit sockets
    /// * `blockstore` - the ledger itself
    #[allow(clippy::new_ret_no_self, clippy::too_many_arguments)]
    pub fn new(
        vote_account: &Pubkey,
        authorized_voter_keypairs: Vec<Arc<Keypair>>,
        bank_forks: &Arc<RwLock<BankForks>>,
        cluster_info: &Arc<ClusterInfo>,
        sockets: Sockets,
        blockstore: Arc<Blockstore>,
        ledger_signal_receiver: Receiver<bool>,
        subscriptions: &Arc<RpcSubscriptions>,
        poh_recorder: &Arc<Mutex<PohRecorder>>,
        leader_schedule_cache: &Arc<LeaderScheduleCache>,
        exit: &Arc<AtomicBool>,
        completed_slots_receiver: CompletedSlotsReceiver,
        block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
        cfg: Option<Arc<AtomicBool>>,
        transaction_status_sender: Option<TransactionStatusSender>,
        rewards_recorder_sender: Option<RewardsRecorderSender>,
        snapshot_package_sender: Option<AccountsPackageSender>,
        vote_tracker: Arc<VoteTracker>,
        retransmit_slots_sender: RetransmitSlotsSender,
        verified_vote_receiver: VerifiedVoteReceiver,
        replay_vote_sender: ReplayVoteSender,
        tvu_config: TvuConfig,
    ) -> Self {
        let keypair: Arc<Keypair> = cluster_info.keypair.clone();

        let Sockets {
            repair: repair_socket,
            fetch: fetch_sockets,
            retransmit: retransmit_sockets,
            forwards: tvu_forward_sockets,
        } = sockets;

        let (fetch_sender, fetch_receiver) = channel();

        let repair_socket = Arc::new(repair_socket);
        let fetch_sockets: Vec<Arc<UdpSocket>> = fetch_sockets.into_iter().map(Arc::new).collect();
        let forward_sockets: Vec<Arc<UdpSocket>> =
            tvu_forward_sockets.into_iter().map(Arc::new).collect();
        let fetch_stage = ShredFetchStage::new(
            fetch_sockets,
            forward_sockets,
            repair_socket.clone(),
            &fetch_sender,
            Some(bank_forks.clone()),
            &exit,
        );

        let (verified_sender, verified_receiver) = unbounded();
        let sigverify_stage = SigVerifyStage::new(
            fetch_receiver,
            verified_sender,
            ShredSigVerifier::new(bank_forks.clone(), leader_schedule_cache.clone()),
        );

        let cluster_slots = Arc::new(ClusterSlots::default());
        let (duplicate_slots_reset_sender, duplicate_slots_reset_receiver) = unbounded();
        let retransmit_stage = RetransmitStage::new(
            bank_forks.clone(),
            leader_schedule_cache,
            blockstore.clone(),
            &cluster_info,
            Arc::new(retransmit_sockets),
            repair_socket,
            verified_receiver,
            &exit,
            completed_slots_receiver,
            *bank_forks.read().unwrap().working_bank().epoch_schedule(),
            cfg,
            tvu_config.shred_version,
            cluster_slots.clone(),
            duplicate_slots_reset_sender,
            verified_vote_receiver,
            tvu_config.repair_validators,
        );

        let (ledger_cleanup_slot_sender, ledger_cleanup_slot_receiver) = channel();

        let snapshot_interval_slots = {
            if let Some(config) = bank_forks.read().unwrap().snapshot_config() {
                config.snapshot_interval_slots
            } else {
                std::u64::MAX
            }
        };
        info!("snapshot_interval_slots: {}", snapshot_interval_slots);
        let (accounts_hash_sender, accounts_hash_receiver) = channel();
        let accounts_hash_verifier = AccountsHashVerifier::new(
            accounts_hash_receiver,
            snapshot_package_sender,
            exit,
            &cluster_info,
            tvu_config.trusted_validators.clone(),
            tvu_config.halt_on_trusted_validators_accounts_hash_mismatch,
            tvu_config.accounts_hash_fault_injection_slots,
            snapshot_interval_slots,
        );

        let replay_stage_config = ReplayStageConfig {
            my_pubkey: keypair.pubkey(),
            vote_account: *vote_account,
            authorized_voter_keypairs,
            exit: exit.clone(),
            subscriptions: subscriptions.clone(),
            leader_schedule_cache: leader_schedule_cache.clone(),
            latest_root_senders: vec![ledger_cleanup_slot_sender],
            accounts_hash_sender: Some(accounts_hash_sender),
            block_commitment_cache,
            transaction_status_sender,
            rewards_recorder_sender,
        };

        let replay_stage = ReplayStage::new(
            replay_stage_config,
            blockstore.clone(),
            bank_forks.clone(),
            cluster_info.clone(),
            ledger_signal_receiver,
            poh_recorder.clone(),
            vote_tracker,
            cluster_slots,
            retransmit_slots_sender,
            duplicate_slots_reset_receiver,
            replay_vote_sender,
        );

        let ledger_cleanup_service = tvu_config.max_ledger_shreds.map(|max_ledger_shreds| {
            LedgerCleanupService::new(
                ledger_cleanup_slot_receiver,
                blockstore.clone(),
                max_ledger_shreds,
                &exit,
            )
        });

        let accounts_background_service = AccountsBackgroundService::new(bank_forks.clone(), &exit);

        Tvu {
            fetch_stage,
            sigverify_stage,
            retransmit_stage,
            replay_stage,
            ledger_cleanup_service,
            accounts_background_service,
            accounts_hash_verifier,
        }
    }

    pub fn join(self) -> thread::Result<()> {
        self.retransmit_stage.join()?;
        self.fetch_stage.join()?;
        self.sigverify_stage.join()?;
        if self.ledger_cleanup_service.is_some() {
            self.ledger_cleanup_service.unwrap().join()?;
        }
        self.accounts_background_service.join()?;
        self.replay_stage.join()?;
        self.accounts_hash_verifier.join()?;
        Ok(())
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::{
        banking_stage::create_test_recorder,
        cluster_info::{ClusterInfo, Node},
    };
    use serial_test_derive::serial;
    use solana_ledger::{
        create_new_tmp_ledger,
        genesis_utils::{create_genesis_config, GenesisConfigInfo},
    };
    use solana_runtime::bank::Bank;
    use std::sync::atomic::Ordering;

    #[ignore]
    #[test]
    #[serial]
    fn test_tvu_exit() {
        solana_logger::setup();
        let leader = Node::new_localhost();
        let target1_keypair = Keypair::new();
        let target1 = Node::new_localhost_with_pubkey(&target1_keypair.pubkey());

        let starting_balance = 10_000;
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(starting_balance);

        let bank_forks = BankForks::new(Bank::new(&genesis_config));

        //start cluster_info1
        let cluster_info1 = ClusterInfo::new_with_invalid_keypair(target1.info.clone());
        cluster_info1.insert_info(leader.info);
        let cref1 = Arc::new(cluster_info1);

        let (blockstore_path, _) = create_new_tmp_ledger!(&genesis_config);
        let (blockstore, l_receiver, completed_slots_receiver) =
            Blockstore::open_with_signal(&blockstore_path, None)
                .expect("Expected to successfully open ledger");
        let blockstore = Arc::new(blockstore);
        let bank = bank_forks.working_bank();
        let (exit, poh_recorder, poh_service, _entry_receiver) =
            create_test_recorder(&bank, &blockstore, None);
        let vote_keypair = Keypair::new();
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let block_commitment_cache = Arc::new(RwLock::new(BlockCommitmentCache::default()));
        let (retransmit_slots_sender, _retransmit_slots_receiver) = unbounded();
        let (_verified_vote_sender, verified_vote_receiver) = unbounded();
        let (replay_vote_sender, _replay_vote_receiver) = unbounded();
        let bank_forks = Arc::new(RwLock::new(bank_forks));
        let tvu = Tvu::new(
            &vote_keypair.pubkey(),
            vec![Arc::new(vote_keypair)],
            &bank_forks,
            &cref1,
            {
                Sockets {
                    repair: target1.sockets.repair,
                    retransmit: target1.sockets.retransmit_sockets,
                    fetch: target1.sockets.tvu,
                    forwards: target1.sockets.tvu_forwards,
                }
            },
            blockstore,
            l_receiver,
            &Arc::new(RpcSubscriptions::new(
                &exit,
                bank_forks.clone(),
                block_commitment_cache.clone(),
            )),
            &poh_recorder,
            &leader_schedule_cache,
            &exit,
            completed_slots_receiver,
            block_commitment_cache,
            None,
            None,
            None,
            None,
            Arc::new(VoteTracker::new(&bank)),
            retransmit_slots_sender,
            verified_vote_receiver,
            replay_vote_sender,
            TvuConfig::default(),
        );
        exit.store(true, Ordering::Relaxed);
        tvu.join().unwrap();
        poh_service.join().unwrap();
    }
}
