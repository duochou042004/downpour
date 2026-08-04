//! Generated damage proofs for prefix-consistent journal replay.

use downpour_storage::journal::{
    FileHeader, FramedRecord, HEADER_LEN, JournalRecord, replay_bytes,
};
use proptest::prelude::*;

fn record_strategy() -> impl Strategy<Value = JournalRecord> {
    prop_oneof![
        (any::<u64>(), 1_u32..=4096, any::<[u8; 32]>()).prop_map(|(offset, len, blake3)| {
            JournalRecord::BlockComplete {
                offset,
                len,
                blake3,
            }
        }),
        (any::<u64>(), any::<u64>()).prop_map(|(covered_bytes, wall_clock)| {
            JournalRecord::Checkpoint {
                covered_bytes,
                wall_clock,
            }
        }),
        prop::collection::vec(any::<u8>(), 0..64)
            .prop_map(|cbor| JournalRecord::IdentityUpdate { cbor }),
        any::<u64>().prop_map(|new_length| JournalRecord::Truncate { new_length }),
        any::<[u8; 32]>().prop_map(|final_blake3| JournalRecord::Sealed { final_blake3 }),
    ]
}

#[derive(Clone, Copy, Debug)]
enum Damage {
    Truncate(usize),
    Flip(usize),
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 10_000,
        max_shrink_iters: 10_000,
        .. ProptestConfig::default()
    })]

    #[test]
    fn replay_never_panics_and_yields_a_prefix(
        records in prop::collection::vec(record_strategy(), 0..40),
        damage_selector in any::<bool>(),
        damage_index in any::<usize>(),
    ) {
        let header = FileHeader::new([0x17; 16], u64::MAX, 1024, [0x29; 32]);
        let mut bytes = header.encode().to_vec();
        let mut boundaries = vec![HEADER_LEN as u64];
        let framed: Vec<_> = records
            .iter()
            .cloned()
            .enumerate()
            .map(|(sequence, record)| FramedRecord::new(sequence as u64, record))
            .collect();
        for record in &framed {
            bytes.extend_from_slice(&record.encode().unwrap());
            boundaries.push(bytes.len() as u64);
        }

        let damage = if damage_selector {
            Damage::Truncate(damage_index % (bytes.len() + 1))
        } else {
            Damage::Flip(damage_index % bytes.len())
        };
        let damaged_at = match damage {
            Damage::Truncate(at) => {
                bytes.truncate(at);
                at
            }
            Damage::Flip(at) => {
                bytes[at] ^= 0x01;
                at
            }
        };

        let replayed = replay_bytes(&bytes);
        if damaged_at < HEADER_LEN {
            prop_assert!(replayed.is_err());
        } else {
            let replayed = replayed.unwrap();
            let expected_prefix_len = boundaries
                .iter()
                .skip(1)
                .take_while(|boundary| **boundary <= damaged_at as u64)
                .count();
            prop_assert_eq!(
                replayed.records(),
                &framed[..expected_prefix_len],
                "damage {:?} at byte {} must preserve exactly {} records",
                damage,
                damaged_at,
                expected_prefix_len,
            );
            prop_assert_eq!(replayed.valid_bytes(), boundaries[expected_prefix_len]);
        }
    }
}
