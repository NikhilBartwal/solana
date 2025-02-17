use {
    crate::{
        accounts::Accounts,
        accounts_db::{
            AccountShrinkThreshold, AccountStorageEntry, AccountsDb, AppendVecId, BankHashInfo,
        },
        accounts_index::AccountSecondaryIndexes,
        ancestors::Ancestors,
        append_vec::{AppendVec, StoredMetaWriteVersion},
        bank::{Bank, BankFieldsToDeserialize, BankRc, Builtins},
        blockhash_queue::BlockhashQueue,
        epoch_stakes::EpochStakes,
        hardened_unpack::UnpackedAppendVecMap,
        message_processor::MessageProcessor,
        rent_collector::RentCollector,
        serde_snapshot::future::SerializableStorage,
        stakes::Stakes,
    },
    bincode,
    bincode::{config::Options, Error},
    log::*,
    rayon::prelude::*,
    serde::{de::DeserializeOwned, Deserialize, Serialize},
    solana_sdk::{
        clock::{Epoch, Slot, UnixTimestamp},
        epoch_schedule::EpochSchedule,
        fee_calculator::{FeeCalculator, FeeRateGovernor},
        genesis_config::ClusterType,
        genesis_config::GenesisConfig,
        hard_forks::HardForks,
        hash::Hash,
        inflation::Inflation,
        pubkey::Pubkey,
    },
    std::{
        collections::{HashMap, HashSet},
        io::{self, BufReader, BufWriter, Read, Write},
        path::{Path, PathBuf},
        result::Result,
        sync::{atomic::Ordering, Arc, RwLock},
    },
};

#[cfg(RUSTC_WITH_SPECIALIZATION)]
use solana_frozen_abi::abi_example::IgnoreAsHelper;

mod common;
mod future;
mod tests;
mod utils;

use future::Context as TypeContextFuture;
#[allow(unused_imports)]
use utils::{serialize_iter_as_map, serialize_iter_as_seq, serialize_iter_as_tuple};

// a number of test cases in accounts_db use this
#[cfg(test)]
pub(crate) use self::tests::reconstruct_accounts_db_via_serialization;

pub(crate) use crate::accounts_db::{SnapshotStorage, SnapshotStorages};

#[derive(Copy, Clone, Eq, PartialEq)]
pub(crate) enum SerdeStyle {
    Newer,
}

const MAX_STREAM_SIZE: u64 = 32 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Serialize, AbiExample)]
struct AccountsDbFields<T>(
    HashMap<Slot, Vec<T>>,
    StoredMetaWriteVersion,
    Slot,
    BankHashInfo,
);

/// Helper type to wrap BufReader streams when deserializing and reconstructing from either just a
/// full snapshot, or both a full and incremental snapshot
pub struct SnapshotStreams<'a, R> {
    pub full_snapshot_stream: &'a mut BufReader<R>,
    pub incremental_snapshot_stream: Option<&'a mut BufReader<R>>,
}

/// Helper type to wrap AccountsDbFields when reconstructing AccountsDb from either just a full
/// snapshot, or both a full and incremental snapshot
#[derive(Debug)]
struct SnapshotAccountsDbFields<T> {
    full_snapshot_accounts_db_fields: AccountsDbFields<T>,
    incremental_snapshot_accounts_db_fields: Option<AccountsDbFields<T>>,
}

impl<T> SnapshotAccountsDbFields<T> {
    /// Collapse the SnapshotAccountsDbFields into a single AccountsDbFields.  If there is no
    /// incremental snapshot, this returns the AccountsDbFields from the full snapshot.  Otherwise
    /// this uses the version, slot, and bank hash info from the incremental snapshot, then the
    /// combination of the storages from both the full and incremental snapshots.
    fn collapse_into(self) -> Result<AccountsDbFields<T>, Error> {
        match self.incremental_snapshot_accounts_db_fields {
            None => Ok(self.full_snapshot_accounts_db_fields),
            Some(AccountsDbFields(
                mut incremental_snapshot_storages,
                incremental_snapshot_version,
                incremental_snapshot_slot,
                incremental_snapshot_bank_hash_info,
            )) => {
                let full_snapshot_storages = self.full_snapshot_accounts_db_fields.0;
                let full_snapshot_slot = self.full_snapshot_accounts_db_fields.2;

                // filter out incremental snapshot storages with slot <= full snapshot slot
                incremental_snapshot_storages.retain(|slot, _| *slot > full_snapshot_slot);

                // There must not be any overlap in the slots of storages between the full snapshot and the incremental snapshot
                incremental_snapshot_storages
                    .iter()
                    .all(|storage_entry| !full_snapshot_storages.contains_key(storage_entry.0)).then(|| ()).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "Snapshots are incompatible: There are storages for the same slot in both the full snapshot and the incremental snapshot!")
                    })?;

                let mut combined_storages = full_snapshot_storages;
                combined_storages.extend(incremental_snapshot_storages.into_iter());

                Ok(AccountsDbFields(
                    combined_storages,
                    incremental_snapshot_version,
                    incremental_snapshot_slot,
                    incremental_snapshot_bank_hash_info,
                ))
            }
        }
    }
}

trait TypeContext<'a> {
    type SerializableAccountStorageEntry: Serialize
        + DeserializeOwned
        + From<&'a AccountStorageEntry>
        + SerializableStorage
        + Sync;

    fn serialize_bank_and_storage<S: serde::ser::Serializer>(
        serializer: S,
        serializable_bank: &SerializableBankAndStorage<'a, Self>,
    ) -> std::result::Result<S::Ok, S::Error>
    where
        Self: std::marker::Sized;

    fn serialize_accounts_db_fields<S: serde::ser::Serializer>(
        serializer: S,
        serializable_db: &SerializableAccountsDb<'a, Self>,
    ) -> std::result::Result<S::Ok, S::Error>
    where
        Self: std::marker::Sized;

    fn deserialize_bank_fields<R>(
        stream: &mut BufReader<R>,
    ) -> Result<
        (
            BankFieldsToDeserialize,
            AccountsDbFields<Self::SerializableAccountStorageEntry>,
        ),
        Error,
    >
    where
        R: Read;

    fn deserialize_accounts_db_fields<R>(
        stream: &mut BufReader<R>,
    ) -> Result<AccountsDbFields<Self::SerializableAccountStorageEntry>, Error>
    where
        R: Read;
}

fn deserialize_from<R, T>(reader: R) -> bincode::Result<T>
where
    R: Read,
    T: DeserializeOwned,
{
    bincode::options()
        .with_limit(MAX_STREAM_SIZE)
        .with_fixint_encoding()
        .allow_trailing_bytes()
        .deserialize_from::<R, T>(reader)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn bank_from_streams<R>(
    serde_style: SerdeStyle,
    snapshot_streams: &mut SnapshotStreams<R>,
    account_paths: &[PathBuf],
    unpacked_append_vec_map: UnpackedAppendVecMap,
    genesis_config: &GenesisConfig,
    frozen_account_pubkeys: &[Pubkey],
    debug_keys: Option<Arc<HashSet<Pubkey>>>,
    additional_builtins: Option<&Builtins>,
    account_secondary_indexes: AccountSecondaryIndexes,
    caching_enabled: bool,
    limit_load_slot_count_from_snapshot: Option<usize>,
    shrink_ratio: AccountShrinkThreshold,
    verify_index: bool,
    accounts_index_bins: Option<usize>,
) -> std::result::Result<Bank, Error>
where
    R: Read,
{
    macro_rules! INTO {
        ($x:ident) => {{
            let (full_snapshot_bank_fields, full_snapshot_accounts_db_fields) =
                $x::deserialize_bank_fields(snapshot_streams.full_snapshot_stream)?;
            let (incremental_snapshot_bank_fields, incremental_snapshot_accounts_db_fields) =
                if let Some(ref mut incremental_snapshot_stream) =
                    snapshot_streams.incremental_snapshot_stream
                {
                    let (bank_fields, accounts_db_fields) =
                        $x::deserialize_bank_fields(incremental_snapshot_stream)?;
                    (Some(bank_fields), Some(accounts_db_fields))
                } else {
                    (None, None)
                };

            let snapshot_accounts_db_fields = SnapshotAccountsDbFields {
                full_snapshot_accounts_db_fields,
                incremental_snapshot_accounts_db_fields,
            };
            let bank = reconstruct_bank_from_fields(
                incremental_snapshot_bank_fields.unwrap_or(full_snapshot_bank_fields),
                snapshot_accounts_db_fields,
                genesis_config,
                frozen_account_pubkeys,
                account_paths,
                unpacked_append_vec_map,
                debug_keys,
                additional_builtins,
                account_secondary_indexes,
                caching_enabled,
                limit_load_slot_count_from_snapshot,
                shrink_ratio,
                verify_index,
                accounts_index_bins,
            )?;
            Ok(bank)
        }};
    }
    match serde_style {
        SerdeStyle::Newer => INTO!(TypeContextFuture),
    }
    .map_err(|err| {
        warn!("bankrc_from_stream error: {:?}", err);
        err
    })
}

pub(crate) fn bank_to_stream<W>(
    serde_style: SerdeStyle,
    stream: &mut BufWriter<W>,
    bank: &Bank,
    snapshot_storages: &[SnapshotStorage],
) -> Result<(), Error>
where
    W: Write,
{
    macro_rules! INTO {
        ($x:ident) => {
            bincode::serialize_into(
                stream,
                &SerializableBankAndStorage::<$x> {
                    bank,
                    snapshot_storages,
                    phantom: std::marker::PhantomData::default(),
                },
            )
        };
    }
    match serde_style {
        SerdeStyle::Newer => INTO!(TypeContextFuture),
    }
    .map_err(|err| {
        warn!("bankrc_to_stream error: {:?}", err);
        err
    })
}

struct SerializableBankAndStorage<'a, C> {
    bank: &'a Bank,
    snapshot_storages: &'a [SnapshotStorage],
    phantom: std::marker::PhantomData<C>,
}

impl<'a, C: TypeContext<'a>> Serialize for SerializableBankAndStorage<'a, C> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        C::serialize_bank_and_storage(serializer, self)
    }
}

struct SerializableAccountsDb<'a, C> {
    accounts_db: &'a AccountsDb,
    slot: Slot,
    account_storage_entries: &'a [SnapshotStorage],
    phantom: std::marker::PhantomData<C>,
}

impl<'a, C: TypeContext<'a>> Serialize for SerializableAccountsDb<'a, C> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        C::serialize_accounts_db_fields(serializer, self)
    }
}

#[cfg(RUSTC_WITH_SPECIALIZATION)]
impl<'a, C> IgnoreAsHelper for SerializableAccountsDb<'a, C> {}

#[allow(clippy::too_many_arguments)]
fn reconstruct_bank_from_fields<E>(
    bank_fields: BankFieldsToDeserialize,
    snapshot_accounts_db_fields: SnapshotAccountsDbFields<E>,
    genesis_config: &GenesisConfig,
    frozen_account_pubkeys: &[Pubkey],
    account_paths: &[PathBuf],
    unpacked_append_vec_map: UnpackedAppendVecMap,
    debug_keys: Option<Arc<HashSet<Pubkey>>>,
    additional_builtins: Option<&Builtins>,
    account_secondary_indexes: AccountSecondaryIndexes,
    caching_enabled: bool,
    limit_load_slot_count_from_snapshot: Option<usize>,
    shrink_ratio: AccountShrinkThreshold,
    verify_index: bool,
    accounts_index_bins: Option<usize>,
) -> Result<Bank, Error>
where
    E: SerializableStorage + std::marker::Sync,
{
    let mut accounts_db = reconstruct_accountsdb_from_fields(
        snapshot_accounts_db_fields,
        account_paths,
        unpacked_append_vec_map,
        &genesis_config.cluster_type,
        account_secondary_indexes,
        caching_enabled,
        limit_load_slot_count_from_snapshot,
        shrink_ratio,
        verify_index,
        accounts_index_bins,
    )?;
    accounts_db.freeze_accounts(
        &Ancestors::from(&bank_fields.ancestors),
        frozen_account_pubkeys,
    );

    let bank_rc = BankRc::new(Accounts::new_empty(accounts_db), bank_fields.slot);

    // if limit_load_slot_count_from_snapshot is set, then we need to side-step some correctness checks beneath this call
    let debug_do_not_add_builtins = limit_load_slot_count_from_snapshot.is_some();
    let bank = Bank::new_from_fields(
        bank_rc,
        genesis_config,
        bank_fields,
        debug_keys,
        additional_builtins,
        debug_do_not_add_builtins,
    );

    Ok(bank)
}

fn reconstruct_single_storage<E>(
    slot: &Slot,
    append_vec_path: &Path,
    storage_entry: &E,
    new_slot_storage: &mut HashMap<AppendVecId, Arc<AccountStorageEntry>>,
) -> Result<(), Error>
where
    E: SerializableStorage,
{
    let (accounts, num_accounts) =
        AppendVec::new_from_file(append_vec_path, storage_entry.current_len())?;
    let u_storage_entry =
        AccountStorageEntry::new_existing(*slot, storage_entry.id(), accounts, num_accounts);

    new_slot_storage.insert(storage_entry.id(), Arc::new(u_storage_entry));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn reconstruct_accountsdb_from_fields<E>(
    snapshot_accounts_db_fields: SnapshotAccountsDbFields<E>,
    account_paths: &[PathBuf],
    unpacked_append_vec_map: UnpackedAppendVecMap,
    cluster_type: &ClusterType,
    account_secondary_indexes: AccountSecondaryIndexes,
    caching_enabled: bool,
    limit_load_slot_count_from_snapshot: Option<usize>,
    shrink_ratio: AccountShrinkThreshold,
    verify_index: bool,
    accounts_index_bins: Option<usize>,
) -> Result<AccountsDb, Error>
where
    E: SerializableStorage + std::marker::Sync,
{
    let mut accounts_db = AccountsDb::new_with_config(
        account_paths.to_vec(),
        cluster_type,
        account_secondary_indexes,
        caching_enabled,
        shrink_ratio,
        accounts_index_bins,
    );

    let AccountsDbFields(
        snapshot_storages,
        snapshot_version,
        snapshot_slot,
        snapshot_bank_hash_info,
    ) = snapshot_accounts_db_fields.collapse_into()?;

    let snapshot_storages = snapshot_storages.into_iter().collect::<Vec<_>>();

    // Ensure all account paths exist
    for path in &accounts_db.paths {
        std::fs::create_dir_all(path)
            .unwrap_or_else(|err| panic!("Failed to create directory {}: {}", path.display(), err));
    }

    // Remap the deserialized AppendVec paths to point to correct local paths
    let mut storage = (0..snapshot_storages.len())
        .into_par_iter()
        .map(|i| {
            let (slot, slot_storage) = &snapshot_storages[i];
            let mut new_slot_storage = HashMap::new();
            for storage_entry in slot_storage {
                let file_name = AppendVec::file_name(*slot, storage_entry.id());

                let append_vec_path = unpacked_append_vec_map.get(&file_name).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("{} not found in unpacked append vecs", file_name),
                    )
                })?;

                reconstruct_single_storage(
                    slot,
                    append_vec_path,
                    storage_entry,
                    &mut new_slot_storage,
                )?;
            }
            Ok((*slot, new_slot_storage))
        })
        .collect::<Result<HashMap<Slot, _>, Error>>()?;

    // discard any slots with no storage entries
    // this can happen if a non-root slot was serialized
    // but non-root stores should not be included in the snapshot
    storage.retain(|_slot, stores| !stores.is_empty());

    accounts_db
        .bank_hashes
        .write()
        .unwrap()
        .insert(snapshot_slot, snapshot_bank_hash_info);

    // Process deserialized data, set necessary fields in self
    let max_id: usize = *storage
        .values()
        .flat_map(HashMap::keys)
        .max()
        .expect("At least one storage entry must exist from deserializing stream");

    {
        accounts_db.storage.0.extend(
            storage.into_iter().map(|(slot, slot_storage_entry)| {
                (slot, Arc::new(RwLock::new(slot_storage_entry)))
            }),
        );
    }

    if max_id > AppendVecId::MAX / 2 {
        panic!("Storage id {} larger than allowed max", max_id);
    }

    accounts_db.next_id.store(max_id + 1, Ordering::Relaxed);
    accounts_db
        .write_version
        .fetch_add(snapshot_version, Ordering::Relaxed);
    accounts_db.generate_index(limit_load_slot_count_from_snapshot, verify_index);
    Ok(accounts_db)
}
