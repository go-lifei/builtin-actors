use std::collections::{BTreeSet, HashMap};

use fil_actor_miner::{
    power_for_sectors, Deadline, PartitionSectorMap, PoStPartition, PowerPair, SectorOnChainInfo,
    TerminationResult,
};
use fil_actors_runtime::runtime::{Policy, Runtime};
use fil_actors_runtime::test_utils::{MessageAccumulator, MockRuntime};
use fil_actors_runtime::ActorError;
use fvm_ipld_bitfield::BitField;
use fvm_ipld_bitfield::UnvalidatedBitField;
use fvm_ipld_blockstore::Blockstore;
use fvm_shared::clock::ChainEpoch;
use fvm_shared::error::ExitCode;
use fvm_shared::{clock::QuantSpec, sector::SectorSize};

mod util;
use crate::util::*;

fn sectors() -> Vec<SectorOnChainInfo> {
    vec![
        test_sector(2, 1, 50, 60, 1000),
        test_sector(3, 2, 51, 61, 1001),
        test_sector(7, 3, 52, 62, 1002),
        test_sector(8, 4, 53, 63, 1003),
        test_sector(8, 5, 54, 64, 1004),
        test_sector(11, 6, 55, 65, 1005),
        test_sector(13, 7, 56, 66, 1006),
        test_sector(8, 8, 57, 67, 1007),
        test_sector(8, 9, 58, 68, 1008),
    ]
}

fn extra_sectors() -> Vec<SectorOnChainInfo> {
    vec![test_sector(8, 10, 58, 68, 1008)]
}

fn all_sectors() -> Vec<SectorOnChainInfo> {
    [sectors(), extra_sectors()].concat()
}

const SECTOR_SIZE: SectorSize = SectorSize::_32GiB;
const QUANT_SPEC: QuantSpec = QuantSpec { unit: 4, offset: 1 };
const PARTITION_SIZE: u64 = 4;

// Define some basic test scenarios that build one each other.
//

// Adds sectors, and proves them if requested.
//
// Partition 1: sectors 1, 2, 3, 4
// Partition 2: sectors 5, 6, 7, 8
// Partition 3: sectors 9
fn add_sectors(
    rt: &MockRuntime,
    deadline: &mut Deadline,
    prove: bool,
) -> (ExpectedDeadlineState, Vec<SectorOnChainInfo>) {
    let sectors = sectors();
    let store = rt.store();

    let power = power_for_sectors(SECTOR_SIZE, &sectors);
    let activated_power = deadline
        .add_sectors(store, PARTITION_SIZE, false, &sectors, SECTOR_SIZE, QUANT_SPEC)
        .expect("Couldn't add sectors");

    assert_eq!(activated_power, power);

    let deadline_state = deadline_state()
        .with_unproven(&[1, 2, 3, 4, 5, 6, 7, 8, 9])
        .with_partitions(vec![
            make_bitfield(&[1, 2, 3, 4]),
            make_bitfield(&[5, 6, 7, 8]),
            make_bitfield(&[9]),
        ])
        .assert(store, &sectors, deadline);

    if !prove {
        return (deadline_state, sectors);
    }

    let mut sector_array = sectors_array(rt, store, sectors.to_owned());

    //prove everything
    let result = deadline
        .record_proven_sectors(
            store,
            &sector_array,
            SECTOR_SIZE,
            QUANT_SPEC,
            0,
            &mut [
                PoStPartition {
                    index: 0,
                    skipped: UnvalidatedBitField::Validated(BitField::default()),
                },
                PoStPartition {
                    index: 1,
                    skipped: UnvalidatedBitField::Validated(BitField::default()),
                },
                PoStPartition {
                    index: 2,
                    skipped: UnvalidatedBitField::Validated(BitField::default()),
                },
            ],
        )
        .unwrap();

    assert_eq!(result.power_delta, power);

    let sectors_root = sector_array.amt.flush().unwrap();

    let (faulty_power, recovery_power) =
        deadline.process_deadline_end(store, QUANT_SPEC, 0, sectors_root).unwrap();

    assert!(faulty_power.is_zero());
    assert!(recovery_power.is_zero());

    let deadline_state = deadline_state
        .with_unproven(&[])
        .with_partitions(vec![
            make_bitfield(&[1, 2, 3, 4]),
            make_bitfield(&[5, 6, 7, 8]),
            make_bitfield(&[9]),
        ])
        .assert(store, &sectors, deadline);

    (deadline_state, sectors)
}

// Adds sectors according to addSectors, then terminates them:
//
// From partition 0: sectors 1 & 3
// From partition 1: sectors 6
fn add_then_terminate(
    rt: &MockRuntime,
    deadline: &mut Deadline,
    prove: bool,
) -> (ExpectedDeadlineState, Vec<SectorOnChainInfo>) {
    let (deadline_state, sectors) = add_sectors(rt, deadline, prove);

    let removed_power = terminate_sectors(
        rt,
        deadline,
        15,
        sectors.to_owned(),
        HashMap::from([(0, make_bitfield(&[1, 3])), (1, make_bitfield(&[6]))]),
    )
    .unwrap();

    let (expected_power, unproven) = if prove {
        (sector_power(&[1, 3, 6]), vec![])
    } else {
        (PowerPair::zero(), vec![2, 4, 5, 7, 8, 9]) // not 1,3,6
    };

    assert_eq!(expected_power, removed_power);

    let deadline_state = deadline_state
        .with_terminations(&[1, 3, 6])
        .with_unproven(&unproven)
        .with_partitions(vec![
            make_bitfield(&[1, 2, 3, 4]),
            make_bitfield(&[5, 6, 7, 8]),
            make_bitfield(&[9]),
        ])
        .assert(rt.store(), &sectors, deadline);

    (deadline_state, sectors)
}

// Adds and terminates sectors according to the previous two functions,
// then pops early terminations.
fn add_then_terminate_then_pop_early(
    rt: &MockRuntime,
    deadline: &mut Deadline,
) -> (ExpectedDeadlineState, Vec<SectorOnChainInfo>) {
    let (deadline_state, sectors) = add_then_terminate(rt, deadline, true);
    let store = rt.store();

    let (early_terminations, has_more) = deadline.pop_early_terminations(store, 100, 100).unwrap();

    assert!(!has_more);
    assert_eq!(2, early_terminations.partitions_processed);
    assert_eq!(3, early_terminations.sectors_processed);
    assert_eq!(1, early_terminations.sectors.len());

    assert_bitfield_equals(early_terminations.sectors.get(&15).unwrap(), &[1, 3, 6]);

    // Popping early terminations doesn't affect the terminations bitfield.
    let deadline_state = deadline_state
        .with_terminations(&[1, 3, 6])
        .with_partitions(vec![
            make_bitfield(&[1, 2, 3, 4]),
            make_bitfield(&[5, 6, 7, 8]),
            make_bitfield(&[9]),
        ])
        .assert(store, &sectors, deadline);

    (deadline_state, sectors)
}

// Runs the above scenarios, then removes partition 0.
fn add_then_terminate_then_remove_partition(
    rt: &MockRuntime,
    deadline: &mut Deadline,
) -> (ExpectedDeadlineState, Vec<SectorOnChainInfo>) {
    let (deadline_state, sectors) = add_then_terminate_then_pop_early(rt, deadline);
    let store = rt.store();

    let (live, dead, removed_power) = deadline
        .remove_partitions(store, &make_bitfield(&[0]), QUANT_SPEC)
        .expect("should have removed partitions");

    assert_bitfield_equals(&live, &[2, 4]);
    assert_bitfield_equals(&dead, &[1, 3]);

    let live_power = power_for_sectors(SECTOR_SIZE, &select_sectors(&sectors, &live));
    assert_eq!(live_power, removed_power);

    let deadline_state = deadline_state
        .with_terminations(&[6])
        .with_partitions(vec![make_bitfield(&[5, 6, 7, 8]), make_bitfield(&[9])])
        .assert(store, &sectors, deadline);

    (deadline_state, sectors)
}
// Adds sectors according to addSectors, then marks sectors 1, 5, 6
// faulty, expiring at epoch 9.
//
// Sector 5 will expire on-time at epoch 9 while 6 will expire early at epoch 9.
fn add_then_mark_faulty(
    rt: &MockRuntime,
    deadline: &mut Deadline,
    prove: bool,
) -> (ExpectedDeadlineState, Vec<SectorOnChainInfo>) {
    let (deadline_state, sectors) = add_sectors(rt, deadline, prove);

    let store = rt.store();
    let sectors_array = sectors_array(rt, store, sectors.to_owned());

    let mut partition_sector_map = PartitionSectorMap::default();
    partition_sector_map.add(0, UnvalidatedBitField::Validated(make_bitfield(&[1]))).unwrap();
    partition_sector_map.add(1, UnvalidatedBitField::Validated(make_bitfield(&[5, 6]))).unwrap();

    // mark faulty
    let power_delta = deadline
        .record_faults(store, &sectors_array, SECTOR_SIZE, QUANT_SPEC, 9, &mut partition_sector_map)
        .unwrap();

    let (expected_power, unproven) = if prove {
        (sector_power(&[1, 5, 6]), vec![])
    } else {
        (PowerPair::zero(), vec![2, 3, 4, 7, 8, 9]) // not 1,5,6
    };

    assert_eq!(power_delta, -expected_power);

    let deadline_state = deadline_state
        .with_faults(&[1, 5, 6])
        .with_unproven(&unproven)
        .with_partitions(vec![
            make_bitfield(&[1, 2, 3, 4]),
            make_bitfield(&[5, 6, 7, 8]),
            make_bitfield(&[9]),
        ])
        .assert(store, &sectors, deadline);
    (deadline_state, sectors)
}

#[test]
fn adds_sectors() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();

    add_sectors(&rt, &mut deadline, false);
}

#[test]
fn adds_sectors_and_proves() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();

    add_sectors(&rt, &mut deadline, true);
}

#[test]
fn terminates_sectors() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();

    add_then_terminate(&rt, &mut deadline, true);
}

#[test]
fn terminates_unproven_sectors() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();

    add_then_terminate(&rt, &mut deadline, false);
}

#[test]
fn pops_early_terminations() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();

    add_then_terminate_then_pop_early(&rt, &mut deadline);
}

#[test]
fn removes_partitions() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();

    add_then_terminate_then_remove_partition(&rt, &mut deadline);
}

#[test]
fn marks_faulty() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();

    add_then_mark_faulty(&rt, &mut deadline, true);
}

#[test]
fn marks_unproven_sectors_faulty() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();

    add_then_mark_faulty(&rt, &mut deadline, false);
}

#[test]
fn cannot_remove_partitions_with_early_terminations() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();

    add_then_terminate(&rt, &mut deadline, false);

    let store = rt.store();
    assert!(deadline.remove_partitions(store, &make_bitfield(&[0]), QUANT_SPEC).is_err());
}

#[test]
fn can_pop_early_terminations_in_multiple_steps() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();

    let (deadline_state, sectors) = add_then_terminate(&rt, &mut deadline, true);

    let mut result = TerminationResult::new();

    let store = rt.store();

    // process 1 sector, 2 partitions (should pop 1 sector)
    let (partial, has_more) = deadline.pop_early_terminations(store, 2, 1).unwrap();
    assert!(has_more);
    result += partial;

    // process 2 sectors, 1 partition (should pop 1 sector)
    let (partial, has_more) = deadline.pop_early_terminations(store, 2, 1).unwrap();
    assert!(has_more);
    result += partial;

    // process 1 sector, 1 partition (should pop 1 sector)
    let (partial, has_more) = deadline.pop_early_terminations(store, 2, 1).unwrap();
    assert!(!has_more);
    result += partial;

    assert_eq!(3, result.partitions_processed);
    assert_eq!(3, result.sectors_processed);
    assert_eq!(1, result.sectors.len());
    assert_bitfield_equals(result.sectors.get(&15).unwrap(), &[1, 3, 6]);

    // Popping early terminations doesn't affect the terminations bitfield.
    deadline_state
        .with_terminations(&[1, 3, 6])
        .with_partitions(vec![
            make_bitfield(&[1, 2, 3, 4]),
            make_bitfield(&[5, 6, 7, 8]),
            make_bitfield(&[9]),
        ])
        .assert(store, &sectors, &deadline);
}

#[test]
fn cannot_remove_missing_partition() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();

    add_then_terminate_then_remove_partition(&rt, &mut deadline);
    assert!(deadline.remove_partitions(rt.store(), &make_bitfield(&[2]), QUANT_SPEC).is_err());
}

#[test]
fn removing_no_partitions_does_nothing() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();

    let (deadline_state, sectors) = add_then_terminate_then_pop_early(&rt, &mut deadline);
    let (live, dead, removed_power) = deadline
        .remove_partitions(rt.store(), &make_bitfield(&[]), QUANT_SPEC)
        .expect("should not have failed to remove partitions");

    assert!(removed_power.is_zero());
    assert!(live.is_empty());
    assert!(dead.is_empty());

    // Popping early terminations doesn't affect the terminations bitfield.
    deadline_state
        .with_terminations(&[1, 3, 6])
        .with_partitions(vec![
            make_bitfield(&[1, 2, 3, 4]),
            make_bitfield(&[5, 6, 7, 8]),
            make_bitfield(&[9]),
        ])
        .assert(rt.store(), &sectors, &deadline);
}

#[test]
fn fails_to_remove_partitions_with_faulty_sectors() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();

    add_then_mark_faulty(&rt, &mut deadline, false);

    // Try to remove a partition with faulty sectors.
    assert!(deadline.remove_partitions(rt.store(), &make_bitfield(&[1]), QUANT_SPEC).is_err());
}

#[test]
fn terminate_proven_and_faulty() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();

    let (deadline_state, sectors) = add_then_mark_faulty(&rt, &mut deadline, true); // 1,5,6 faulty

    let removed_power = terminate_sectors(
        &rt,
        &mut deadline,
        15,
        sectors.to_owned(),
        HashMap::from([(0, make_bitfield(&[1, 3])), (1, make_bitfield(&[6]))]),
    )
    .unwrap();

    // Sector 3 active, 1, 6 faulty
    let expected_power_loss =
        power_for_sectors(SECTOR_SIZE, &select_sectors(&sectors, &make_bitfield(&[3])));
    assert_eq!(
        expected_power_loss, removed_power,
        "deadline state to remove power for terminated sectors"
    );

    deadline_state
        .with_terminations(&[1, 3, 6])
        .with_faults(&[5])
        .with_partitions(vec![
            make_bitfield(&[1, 2, 3, 4]),
            make_bitfield(&[5, 6, 7, 8]),
            make_bitfield(&[9]),
        ])
        .assert(rt.store(), &sectors, &deadline);
}

fn terminate_sectors(
    rt: &MockRuntime,
    deadline: &mut Deadline,
    epoch: ChainEpoch,
    sectors: Vec<SectorOnChainInfo>,
    partition_sectors: HashMap<u64, BitField>,
) -> anyhow::Result<PowerPair> {
    let store = rt.store();
    let sectors_array = sectors_array(rt, &store, sectors);

    let mut partition_sector_map = PartitionSectorMap::default();
    for (partition, sectors) in partition_sectors {
        partition_sector_map.add(partition, UnvalidatedBitField::Validated(sectors)).unwrap();
    }

    deadline.terminate_sectors(
        &Policy::default(),
        &store,
        &sectors_array,
        epoch,
        &mut partition_sector_map,
        SECTOR_SIZE,
        QUANT_SPEC,
    )
}

#[test]
fn terminate_unproven_and_faulty() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();

    let (deadline_state, sectors) = add_then_mark_faulty(&rt, &mut deadline, false); // 1,5,6 faulty

    let removed_power = terminate_sectors(
        &rt,
        &mut deadline,
        15,
        sectors.to_owned(),
        HashMap::from([(0, make_bitfield(&[1, 3])), (1, make_bitfield(&[6]))]),
    )
    .unwrap();

    // sector 3 unproven, 1, 6 faulty
    assert!(removed_power.is_zero(), "should remove no power");

    deadline_state
        .with_terminations(&[1, 3, 6])
        .with_faults(&[5])
        .with_unproven(&[2, 4, 7, 8, 9]) // not 1, 3, 5, & 6
        .with_partitions(vec![
            make_bitfield(&[1, 2, 3, 4]),
            make_bitfield(&[5, 6, 7, 8]),
            make_bitfield(&[9]),
        ])
        .assert(rt.store(), &sectors, &deadline);
}

#[test]
fn fails_to_terminate_missing_sector() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();
    let (_, sectors) = add_then_mark_faulty(&rt, &mut deadline, false); // 1,5,6 faulty

    let ret = terminate_sectors(
        &rt,
        &mut deadline,
        15,
        sectors,
        HashMap::from([(0, make_bitfield(&[6]))]),
    );

    assert!(ret.is_err());
    let err = ret
        .err()
        .expect("can only terminate live sectors")
        .downcast::<ActorError>()
        .expect("Invalid error");
    assert_eq!(err.exit_code(), ExitCode::USR_ILLEGAL_ARGUMENT);
}

#[test]
fn fails_to_terminate_missing_partition() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();
    let (_, sectors) = add_then_mark_faulty(&rt, &mut deadline, false); // 1,5,6 faulty

    let ret = terminate_sectors(
        &rt,
        &mut deadline,
        15,
        sectors,
        HashMap::from([(4, make_bitfield(&[6]))]),
    );

    assert!(ret.is_err());
    let err = ret
        .err()
        .expect("can only terminate existing partitions")
        .downcast::<ActorError>()
        .expect("Invalid error");
    assert_eq!(err.exit_code(), ExitCode::USR_NOT_FOUND);
}

#[test]
fn fails_to_terminate_already_terminated_sector() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();
    let (_, sectors) = add_then_terminate(&rt, &mut deadline, false); // terminates 1,3,6

    let ret = terminate_sectors(
        &rt,
        &mut deadline,
        15,
        sectors,
        HashMap::from([(0, make_bitfield(&[1, 2]))]),
    );

    assert!(ret.is_err());
    let err = ret
        .err()
        .expect("cannot terminate already terminated sector")
        .downcast::<ActorError>()
        .expect("Invalid error");
    assert_eq!(err.exit_code(), ExitCode::USR_ILLEGAL_ARGUMENT);
}

#[test]
fn faulty_sectors_expire() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();

    // mark sectors 5&6 faulty, expiring at epoch 9
    let (_, sectors) = add_then_mark_faulty(&rt, &mut deadline, true);

    // we expect all sectors but 7 to have expired at this point
    let expired = deadline
        .pop_expired_sectors(rt.store(), 9, QUANT_SPEC)
        .expect("failed to pop expired sectors");

    assert_bitfield_equals(&expired.on_time_sectors, &[1, 2, 3, 4, 5, 8, 9]);
    assert_bitfield_equals(&expired.early_sectors, &[6]);

    deadline_state()
        .with_terminations(&[1, 2, 3, 4, 5, 6, 8, 9])
        .with_faults(&[])
        .with_partitions(vec![
            make_bitfield(&[1, 2, 3, 4]),
            make_bitfield(&[5, 6, 7, 8]),
            make_bitfield(&[9]),
        ])
        .assert(rt.store(), &sectors, &deadline);

    // check early terminations
    let (early_terminations, has_more) = deadline
        .pop_early_terminations(rt.store(), 100, 100)
        .expect("failed to pop early_terminations");
    assert!(!has_more);
    assert_eq!(early_terminations.partitions_processed, 1);
    assert_eq!(early_terminations.sectors_processed, 1);
    assert_eq!(early_terminations.sectors.len(), 1);
    assert_bitfield_equals(early_terminations.sectors.get(&9).unwrap(), &[6]);

    // popping early_terminations doesn't affect the terminations bitfield
    deadline_state()
        .with_terminations(&[1, 2, 3, 4, 5, 6, 8, 9])
        .with_faults(&[])
        .with_partitions(vec![
            make_bitfield(&[1, 2, 3, 4]),
            make_bitfield(&[5, 6, 7, 8]),
            make_bitfield(&[9]),
        ])
        .assert(rt.store(), &sectors, &deadline);
}

#[test]
fn cannot_pop_expired_sectors_before_proving() {
    let (_, rt) = setup();
    let mut deadline = Deadline::new(rt.store()).unwrap();

    // add sectors, but don't prove
    add_sectors(&rt, &mut deadline, false);

    // try to pop some expirations
    let ret = deadline.pop_expired_sectors(rt.store(), 9, QUANT_SPEC);
    assert!(ret.is_err());
    let err = ret.err().expect("cannot pop expired sectors from a partition with unproven sectors");

    assert!(err
        .to_string()
        .to_lowercase()
        .contains("cannot pop expired sectors from a partition with unproven sectors"));
}

fn deadline_state() -> ExpectedDeadlineState {
    ExpectedDeadlineState {
        quant: QUANT_SPEC,
        partition_size: PARTITION_SIZE,
        sector_size: SECTOR_SIZE,
        sectors: all_sectors(),
        ..Default::default()
    }
}

fn sector_power(sector_numbers: &[u64]) -> PowerPair {
    power_for_sectors(SECTOR_SIZE, &select_sectors(&all_sectors(), &make_bitfield(sector_numbers)))
}

fn make_bitfield(sector_numbers: &[u64]) -> BitField {
    BitField::try_from_bits(sector_numbers.iter().copied()).unwrap()
}

fn select_sectors(sectors: &[SectorOnChainInfo], field: &BitField) -> Vec<SectorOnChainInfo> {
    let mut to_include: BTreeSet<_> = field.iter().collect();
    let included =
        sectors.iter().filter(|sector| to_include.remove(&sector.sector_number)).cloned().collect();

    assert!(to_include.is_empty(), "failed to find {} expected sectors", to_include.len());

    included
}

// Helper type for validating deadline state.
//
// All methods take the state by value so one can (and should) construct a
// sane base-state.
struct ExpectedDeadlineState {
    quant: QuantSpec,
    #[allow(dead_code)]
    sector_size: SectorSize,
    #[allow(dead_code)]
    partition_size: u64,
    #[allow(dead_code)]
    sectors: Vec<SectorOnChainInfo>,
    faults: BitField,
    recovering: BitField,
    terminations: BitField,
    unproven: BitField,
    posts: BitField,
    partition_sectors: Vec<BitField>,
}

impl Default for ExpectedDeadlineState {
    fn default() -> Self {
        Self {
            quant: QuantSpec { offset: 0, unit: 0 },
            sector_size: SectorSize::_32GiB,
            partition_size: 0,
            sectors: vec![],
            faults: BitField::default(),
            recovering: BitField::default(),
            terminations: BitField::default(),
            unproven: BitField::default(),
            posts: BitField::default(),
            partition_sectors: vec![],
        }
    }
}

impl ExpectedDeadlineState {
    #[allow(dead_code)]
    fn with_quant_spec(mut self, quant: QuantSpec) -> Self {
        self.quant = quant;
        self
    }

    fn with_faults(mut self, faults: &[u64]) -> Self {
        self.faults = make_bitfield(faults);
        self
    }

    #[allow(dead_code)]
    fn with_recovering(mut self, recovering: &[u64]) -> Self {
        self.recovering = make_bitfield(recovering);
        self
    }

    fn with_terminations(mut self, terminations: &[u64]) -> Self {
        self.terminations = make_bitfield(terminations);
        self
    }

    fn with_unproven(mut self, unproven: &[u64]) -> Self {
        self.unproven = make_bitfield(unproven);
        self
    }

    #[allow(dead_code)]
    fn with_posts(mut self, posts: &[u64]) -> Self {
        self.posts = make_bitfield(posts);
        self
    }

    fn with_partitions(mut self, partitions: Vec<BitField>) -> Self {
        self.partition_sectors = partitions;
        self
    }

    // Assert that the deadline's state matches the expected state.
    fn assert<BS: Blockstore>(
        self,
        store: &BS,
        sectors: &[SectorOnChainInfo],
        deadline: &Deadline,
    ) -> Self {
        let summary = self.check_deadline_invariants(store, sectors, deadline);

        assert_eq!(self.faults, summary.faulty_sectors);
        assert_eq!(self.recovering, summary.recovering_sectors);
        assert_eq!(self.terminations, summary.terminated_sectors);
        assert_eq!(self.unproven, summary.unproven_sectors);
        assert_eq!(self.posts, deadline.partitions_posted);

        let partitions = deadline.partitions_amt(store).unwrap();
        assert_eq!(self.partition_sectors.len() as u64, partitions.count());

        for (i, partition_sectors) in self.partition_sectors.iter().enumerate() {
            let partitions = partitions.get(i as u64).unwrap().unwrap();
            assert_eq!(partition_sectors, &partitions.sectors);
        }

        self
    }

    // check the deadline's invariants, returning all contained sectors, faults,
    // recoveries, terminations, and partition/sector assignments.
    fn check_deadline_invariants<BS: Blockstore>(
        &self,
        store: &BS,
        sectors: &[SectorOnChainInfo],
        deadline: &Deadline,
    ) -> DeadlineStateSummary {
        let acc = MessageAccumulator::default();
        let summary = check_deadline_state_invariants(
            deadline,
            store,
            QUANT_SPEC,
            SECTOR_SIZE,
            &sectors_as_map(sectors),
            &acc,
        );

        assert!(acc.is_empty(), "{}", acc.messages().join("\n"));

        summary
    }
}