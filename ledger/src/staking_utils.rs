use solana_runtime::bank::Bank;
use solana_sdk::{
    clock::{Epoch, Slot},
    pubkey::Pubkey,
};
use std::collections::HashMap;

/// Looks through vote accounts, and finds the latest slot that has achieved
/// supermajority lockout
pub fn get_supermajority_slot(bank: &Bank, epoch: Epoch) -> Option<u64> {
    // Find the amount of stake needed for supermajority
    let stakes_and_lockouts = epoch_stakes_and_lockouts(bank, epoch);
    let total_stake: u64 = stakes_and_lockouts.iter().map(|s| s.0).sum();
    let supermajority_stake = total_stake * 2 / 3;

    // Filter out the states that don't have a max lockout
    find_supermajority_slot(supermajority_stake, stakes_and_lockouts.iter())
}

pub fn vote_account_stakes(bank: &Bank) -> HashMap<Pubkey, u64> {
    bank.vote_accounts()
        .into_iter()
        .map(|(id, (stake, _))| (id, stake))
        .collect()
}

fn epoch_stakes_and_lockouts(bank: &Bank, epoch: Epoch) -> Vec<(u64, Option<u64>)> {
    bank.epoch_vote_accounts(epoch)
        .expect("Bank state for epoch is missing")
        .iter()
        .filter_map(|(_ /*vote pubkey*/, (stake, vote_account))| {
            let root_slot = vote_account.vote_state().as_ref().ok()?.root_slot;
            Some((*stake, root_slot))
        })
        .collect()
}

fn find_supermajority_slot<'a, I>(supermajority_stake: u64, stakes_and_lockouts: I) -> Option<Slot>
where
    I: Iterator<Item = &'a (u64, Option<u64>)>,
{
    // Filter out the states that don't have a max lockout
    let mut stakes_and_lockouts: Vec<_> = stakes_and_lockouts
        .filter_map(|(stake, slot)| slot.map(|s| (stake, s)))
        .collect();

    // Sort by the root slot, in descending order
    stakes_and_lockouts.sort_unstable_by(|s1, s2| s1.1.cmp(&s2.1).reverse());

    // Find if any slot has achieved sufficient votes for supermajority lockout
    let mut total = 0;
    for (stake, slot) in stakes_and_lockouts {
        total += *stake;
        if total > supermajority_stake {
            return Some(slot);
        }
    }

    None
}

#[cfg(test)]
pub(crate) mod tests {
    use {
        super::*,
        crate::genesis_utils::{
            bootstrap_validator_stake_lamports, create_genesis_config, GenesisConfigInfo,
        },
        rand::Rng,
        solana_runtime::vote_account::{VoteAccount, VoteAccounts},
        solana_sdk::{
            account::{from_account, AccountSharedData},
            clock::Clock,
            instruction::Instruction,
            pubkey::Pubkey,
            signature::{Keypair, Signer},
            signers::Signers,
            stake::{
                instruction as stake_instruction,
                state::{Authorized, Delegation, Lockup, Stake},
            },
            sysvar::stake_history::{self, StakeHistory},
            transaction::Transaction,
        },
        solana_vote_program::{
            vote_instruction,
            vote_state::{VoteInit, VoteState, VoteStateVersions},
        },
        std::sync::Arc,
    };

    fn new_from_parent(parent: &Arc<Bank>, slot: Slot) -> Bank {
        Bank::new_from_parent(parent, &Pubkey::default(), slot)
    }

    pub(crate) fn setup_vote_and_stake_accounts(
        bank: &Bank,
        from_account: &Keypair,
        vote_account: &Keypair,
        validator_identity_account: &Keypair,
        amount: u64,
    ) {
        let vote_pubkey = vote_account.pubkey();
        fn process_instructions<T: Signers>(bank: &Bank, keypairs: &T, ixs: &[Instruction]) {
            let tx = Transaction::new_signed_with_payer(
                ixs,
                Some(&keypairs.pubkeys()[0]),
                keypairs,
                bank.last_blockhash(),
            );
            bank.process_transaction(&tx).unwrap();
        }

        process_instructions(
            bank,
            &[from_account, vote_account, validator_identity_account],
            &vote_instruction::create_account(
                &from_account.pubkey(),
                &vote_pubkey,
                &VoteInit {
                    node_pubkey: validator_identity_account.pubkey(),
                    authorized_voter: vote_pubkey,
                    authorized_withdrawer: vote_pubkey,
                    commission: 0,
                },
                amount,
            ),
        );

        let stake_account_keypair = Keypair::new();
        let stake_account_pubkey = stake_account_keypair.pubkey();

        process_instructions(
            bank,
            &[from_account, &stake_account_keypair],
            &stake_instruction::create_account_and_delegate_stake(
                &from_account.pubkey(),
                &stake_account_pubkey,
                &vote_pubkey,
                &Authorized::auto(&stake_account_pubkey),
                &Lockup::default(),
                amount,
            ),
        );
    }

    #[test]
    fn test_epoch_stakes_and_lockouts() {
        solana_logger::setup();
        let stake = bootstrap_validator_stake_lamports();
        let leader_stake = Stake {
            delegation: Delegation {
                stake: bootstrap_validator_stake_lamports(),
                activation_epoch: std::u64::MAX, // mark as bootstrap
                ..Delegation::default()
            },
            ..Stake::default()
        };

        let validator = Keypair::new();

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000 * bootstrap_validator_stake_lamports());

        let bank = Bank::new_for_tests(&genesis_config);
        let vote_account = Keypair::new();

        // Give the validator some stake but don't setup a staking account
        // Validator has no lamports staked, so they get filtered out. Only the bootstrap validator
        // created by the genesis config will get included
        bank.transfer(1, &mint_keypair, &validator.pubkey())
            .unwrap();

        // Make a mint vote account. Because the mint has nonzero stake, this
        // should show up in the active set
        setup_vote_and_stake_accounts(&bank, &mint_keypair, &vote_account, &mint_keypair, stake);

        // simulated stake
        let other_stake = Stake {
            delegation: Delegation {
                stake,
                activation_epoch: bank.epoch(),
                ..Delegation::default()
            },
            ..Stake::default()
        };

        let first_leader_schedule_epoch = bank.get_leader_schedule_epoch(bank.slot());
        // find the first slot in the next leader schedule epoch
        let mut slot = bank.slot();
        loop {
            slot += 1;
            if bank.get_leader_schedule_epoch(slot) != first_leader_schedule_epoch {
                break;
            }
        }
        let bank = new_from_parent(&Arc::new(bank), slot);
        let next_leader_schedule_epoch = bank.get_leader_schedule_epoch(slot);

        let result: Vec<_> = epoch_stakes_and_lockouts(&bank, first_leader_schedule_epoch);
        assert_eq!(
            result,
            vec![(
                leader_stake.stake(first_leader_schedule_epoch, None, true),
                None
            )]
        );

        // epoch stakes and lockouts are saved off for the future epoch, should
        //  match current bank state
        let mut result: Vec<_> = epoch_stakes_and_lockouts(&bank, next_leader_schedule_epoch);
        result.sort();
        let stake_history =
            from_account::<StakeHistory, _>(&bank.get_account(&stake_history::id()).unwrap())
                .unwrap();
        let mut expected = vec![
            (
                leader_stake.stake(bank.epoch(), Some(&stake_history), true),
                None,
            ),
            (
                other_stake.stake(bank.epoch(), Some(&stake_history), true),
                None,
            ),
        ];

        expected.sort();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_find_supermajority_slot() {
        let supermajority = 10;

        let stakes_and_slots = vec![];
        assert_eq!(
            find_supermajority_slot(supermajority, stakes_and_slots.iter()),
            None
        );

        let stakes_and_slots = vec![(5, None), (5, None)];
        assert_eq!(
            find_supermajority_slot(supermajority, stakes_and_slots.iter()),
            None
        );

        let stakes_and_slots = vec![(5, None), (5, None), (9, Some(2))];
        assert_eq!(
            find_supermajority_slot(supermajority, stakes_and_slots.iter()),
            None
        );

        let stakes_and_slots = vec![(5, None), (5, None), (9, Some(2)), (1, Some(3))];
        assert_eq!(
            find_supermajority_slot(supermajority, stakes_and_slots.iter()),
            None
        );

        let stakes_and_slots = vec![(5, None), (5, None), (9, Some(2)), (2, Some(3))];
        assert_eq!(
            find_supermajority_slot(supermajority, stakes_and_slots.iter()),
            Some(2)
        );

        let stakes_and_slots = vec![(9, Some(2)), (2, Some(3)), (9, None)];
        assert_eq!(
            find_supermajority_slot(supermajority, stakes_and_slots.iter()),
            Some(2)
        );

        let stakes_and_slots = vec![(9, Some(2)), (2, Some(3)), (9, Some(3))];
        assert_eq!(
            find_supermajority_slot(supermajority, stakes_and_slots.iter()),
            Some(3)
        );
    }

    #[test]
    fn test_to_staked_nodes() {
        let mut stakes = Vec::new();
        let node1 = solana_sdk::pubkey::new_rand();

        // Node 1 has stake of 3
        for i in 0..3 {
            stakes.push((
                i,
                VoteState::new(
                    &VoteInit {
                        node_pubkey: node1,
                        ..VoteInit::default()
                    },
                    &Clock::default(),
                ),
            ));
        }

        // Node 1 has stake of 5
        let node2 = solana_sdk::pubkey::new_rand();

        stakes.push((
            5,
            VoteState::new(
                &VoteInit {
                    node_pubkey: node2,
                    ..VoteInit::default()
                },
                &Clock::default(),
            ),
        ));
        let mut rng = rand::thread_rng();
        let vote_accounts = stakes.into_iter().map(|(stake, vote_state)| {
            let account = AccountSharedData::new_data(
                rng.gen(), // lamports
                &VoteStateVersions::new_current(vote_state),
                &Pubkey::new_unique(), // owner
            )
            .unwrap();
            let vote_pubkey = Pubkey::new_unique();
            (vote_pubkey, (stake, VoteAccount::from(account)))
        });
        let result = vote_accounts.collect::<VoteAccounts>().staked_nodes();
        assert_eq!(result.len(), 2);
        assert_eq!(result[&node1], 3);
        assert_eq!(result[&node2], 5);
    }
}
