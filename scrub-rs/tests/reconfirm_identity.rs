//! Reconfirmation must not authorize repair from checksum-valid metadata whose
//! identity the reader already rejected. Tiny regular-file fixtures; no mounts.
use scrub_rs::btrfs::chunk::{ChunkItem, ChunkMap, ChunkRecord, Stripe};
use scrub_rs::btrfs::csum_strategy::{CsumStrategy, csum_type};
use scrub_rs::btrfs::extent::reconfirm_mismatch;
use scrub_rs::btrfs::key::{Key, bg_flag, key_type, objectid};
use scrub_rs::btrfs::node::HEADER_SIZE;
use scrub_rs::btrfs::reader::{FsReader, GEN_DONT_CHECK};
use scrub_rs::fs::Reconfirm;
use std::io::{Seek, SeekFrom, Write};

const SIZE: usize = 4096;
const EXTENT_ROOT: u64 = 0x1000;
const CSUM_ROOT: u64 = 0x2000;
const LOGICAL: u64 = 0x100000;
const FSID: [u8; 16] = [0x11; 16];
const STRATEGY: CsumStrategy = CsumStrategy {
    csum_type: csum_type::CRC32C,
    name: "crc32c",
    hash_len: 4,
    sector_size: SIZE as u64,
};

fn seal(buf: &mut [u8]) {
    let digest = STRATEGY.compute(&buf[32..]);
    buf[..digest.len()].copy_from_slice(&digest);
    assert!(STRATEGY.verify_node_header(buf));
}

fn leaf(address: u64, owner: u64, key: Key, payload: &[u8]) -> Vec<u8> {
    let mut buf = vec![0; SIZE];
    buf[32..48].copy_from_slice(&FSID);
    buf[48..56].copy_from_slice(&address.to_le_bytes());
    buf[80..88].copy_from_slice(&1u64.to_le_bytes());
    buf[88..96].copy_from_slice(&owner.to_le_bytes());
    buf[96..100].copy_from_slice(&1u32.to_le_bytes());
    let slot = HEADER_SIZE;
    buf[slot..slot + 8].copy_from_slice(&key.objectid.to_le_bytes());
    buf[slot + 8] = key.ty;
    buf[slot + 9..slot + 17].copy_from_slice(&key.offset.to_le_bytes());
    let start = SIZE - payload.len();
    buf[slot + 17..slot + 21].copy_from_slice(&((start - HEADER_SIZE) as u32).to_le_bytes());
    buf[slot + 21..slot + 25].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    buf[start..].copy_from_slice(payload);
    seal(&mut buf);
    buf
}

fn leaves() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let stored = STRATEGY.compute(&[0x5a; SIZE]);
    let mut extent = [0; 24];
    extent[..8].copy_from_slice(&1u64.to_le_bytes());
    extent[8..16].copy_from_slice(&1u64.to_le_bytes());
    extent[16..24].copy_from_slice(&1u64.to_le_bytes());
    (
        leaf(
            EXTENT_ROOT,
            objectid::EXTENT_TREE,
            Key::new(LOGICAL, key_type::EXTENT_ITEM, SIZE as u64),
            &extent,
        ),
        leaf(
            CSUM_ROOT,
            objectid::CSUM_TREE,
            Key::new(
                objectid::EXTENT_CSUM_OBJECTID,
                key_type::EXTENT_CSUM,
                LOGICAL,
            ),
            &stored,
        ),
        stored,
    )
}

fn check(extent: &[u8], csum: &[u8], stored: &[u8], rejected_root: Option<u64>) -> Reconfirm {
    let mut image = tempfile::tempfile().unwrap();
    image.seek(SeekFrom::Start(EXTENT_ROOT)).unwrap();
    image.write_all(extent).unwrap();
    image.seek(SeekFrom::Start(CSUM_ROOT)).unwrap();
    image.write_all(csum).unwrap();
    let mut map = ChunkMap::default();
    map.insert(&ChunkRecord {
        logical: EXTENT_ROOT,
        chunk: ChunkItem {
            length: 2 * SIZE as u64,
            stripe_len: SIZE as u64,
            ty: bg_flag::METADATA,
            num_stripes: 1,
            stripes: vec![Stripe {
                devid: 1,
                offset: EXTENT_ROOT,
            }],
        },
    });
    let mut reader = FsReader::new(image, SIZE, 0, Some(STRATEGY))
        .with_devid(1)
        .with_fsid(FSID);
    if let Some(root) = rejected_root {
        let result = reader
            .read_node(&map, root, GEN_DONT_CHECK, None, None)
            .unwrap();
        assert!(result.node.is_some());
        assert!(!result.all_mirrors_failed);
        assert!(
            result.generation_mismatch,
            "reader must detect the identity failure"
        );
    }
    reconfirm_mismatch(
        &mut reader,
        &map,
        EXTENT_ROOT,
        CSUM_ROOT,
        LOGICAL,
        stored,
        STRATEGY.hash_len,
        STRATEGY.sector_size,
    )
}

fn wrong_identity(root: u64, field: usize) {
    let (mut extent, mut csum, stored) = leaves();
    let buf = if root == EXTENT_ROOT {
        &mut extent
    } else {
        &mut csum
    };
    buf[field] ^= 1;
    seal(buf); // Identity differs, but this is NOT a metadata checksum failure.
    assert_eq!(
        check(&extent, &csum, &stored, Some(root)),
        Reconfirm::Unverifiable
    );
}

#[test]
fn valid_metadata_reconfirms_corruption() {
    let (extent, csum, stored) = leaves();
    assert_eq!(check(&extent, &csum, &stored, None), Reconfirm::Corruption);
}

#[test]
fn extent_wrong_bytenr_is_unverifiable() {
    wrong_identity(EXTENT_ROOT, 48);
}
#[test]
fn extent_wrong_fsid_is_unverifiable() {
    wrong_identity(EXTENT_ROOT, 32);
}
#[test]
fn checksum_wrong_bytenr_is_unverifiable() {
    wrong_identity(CSUM_ROOT, 48);
}
#[test]
fn checksum_wrong_fsid_is_unverifiable() {
    wrong_identity(CSUM_ROOT, 32);
}

#[test]
fn broken_metadata_checksums_are_unverifiable() {
    for extent_bad in [true, false] {
        let (mut extent, mut csum, stored) = leaves();
        if extent_bad {
            extent[0] ^= 1;
        } else {
            csum[0] ^= 1;
        }
        assert_eq!(
            check(&extent, &csum, &stored, None),
            Reconfirm::Unverifiable
        );
    }
}

#[test]
fn changed_live_checksum_is_stale() {
    let (extent, mut csum, stored) = leaves();
    csum[SIZE - STRATEGY.hash_len] ^= 1;
    seal(&mut csum);
    assert_eq!(check(&extent, &csum, &stored, None), Reconfirm::Stale);
}

#[test]
fn freed_extent_is_stale() {
    let (mut extent, csum, stored) = leaves();
    extent[96..100].copy_from_slice(&0u32.to_le_bytes());
    seal(&mut extent);
    assert_eq!(check(&extent, &csum, &stored, None), Reconfirm::Stale);
}
