//! Minimal btrfs superblock parsing: magic, header checksum, and tree roots.

use std::io::{self, Read, Seek};

use super::csum_strategy::CsumStrategy;
use super::util::{le_u16, le_u32, le_u64, read_at};

/// Byte offset of the primary superblock (64 KiB).
pub const SUPERBLOCK_OFFSET: u64 = 0x10_000;

/// Mirror copies of the superblock, probed in order when a lower copy fails.
pub const SUPERBLOCK_BACKUP_OFFSETS: &[u64] =
    &[0x10_000, 0x400_0000, 0x40_0000_0000, 0x100_0000_0000];

pub const BTRFS_MAGIC: [u8; 8] = *b"_BHRfS_M";

/// True if `buf` holds the btrfs magic at `offset`. Used by the
/// filesystem's `block_has_magic` probe.
pub fn has_magic_at(buf: &[u8], offset: usize) -> bool {
    buf.len() >= offset + BTRFS_MAGIC.len()
        && buf[offset..offset + BTRFS_MAGIC.len()] == BTRFS_MAGIC
}

pub const BTRFS_SECTOR_SIZE: usize = 4096;

/// The superblock fields needed to navigate the on-disk trees.
#[derive(Debug)]
pub struct Superblock {
    pub fsid: [u8; 16],
    pub metadata_uuid: [u8; 16],
    pub incompat_flags: u64,
    pub bytenr: u64,
    pub magic: [u8; 8],
    pub generation: u64,

    /// Logical address of the root tree's root node.
    pub root: u64,

    /// Logical address of the chunk tree's root node.
    pub chunk_root: u64,
    pub total_bytes: u64,
    pub bytes_used: u64,
    pub num_devices: u64,
    pub sector_size: u32,
    pub node_size: u32,
    pub stripesize: u32,
    pub sys_chunk_array_size: u32,
    pub chunk_root_generation: u64,
    pub csum_type: u16,

    /// This device's id. Each NonRAID slot hosts its own single-device
    /// filesystem, so there is exactly one device.
    pub devid: u64,

    /// Raw bytes of the system chunk array, as stored (parsed in chunk.rs).
    pub sys_chunks: Vec<u8>,
}

// Field offsets within the 4 KiB superblock block.
const OFF_FSID: usize = 32;
const OFF_BYTENR: usize = 48;

pub const OFF_MAGIC: usize = 64;
const OFF_GENERATION: usize = 72;
const OFF_ROOT: usize = 80;
const OFF_CHUNK_ROOT: usize = 88;
const OFF_TOTAL_BYTES: usize = 112;
const OFF_BYTES_USED: usize = 120;
const OFF_NUM_DEVICES: usize = 136;
const OFF_SECTOR_SIZE: usize = 144;
const OFF_NODE_SIZE: usize = 148;
const OFF_STRIPESIZE: usize = 156;
const OFF_SYS_CHUNK_ARRAY_SIZE: usize = 160;
const OFF_CHUNK_ROOT_GENERATION: usize = 164;
const OFF_INCOMPAT_FLAGS: usize = 188;
const OFF_CSUM_TYPE: usize = 196;

// dev_item.devid: csum_type ends at 198, then 3 *_level bytes, then the
// 98-byte dev_item starts (its first field is devid).
const OFF_DEVID: usize = 198 + 3;

const OFF_METADATA_UUID: usize = 571;

// The system chunk array follows the fixed portion: level bytes, dev_item,
// label, uuid fields, and reserved bytes.
const OFF_SYS_CHUNKS: usize = 811;

// BTRFS feature flags
const BTRFS_FEATURE_INCOMPAT_METADATA_UUID: u64 = 0x400;

impl Superblock {
    /// Read the superblock, trying each reachable mirror copy in order and
    /// using the first that passes magic + header-checksum verification.
    pub fn read<R: Read + Seek>(fp: &mut R, offset: u64) -> std::io::Result<Self> {
        let mut last_err: Option<io::Error> = None;
        for &sb_off in SUPERBLOCK_BACKUP_OFFSETS {
            let abs_off = offset + sb_off;
            match Self::read_one(fp, abs_off) {
                Ok(sb) => return Ok(sb),

                // Short read: the copy lies past the device end — skip it.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => continue,

                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "no readable btrfs superblock found",
            )
        }))
    }

    /// Read only the primary copy, without fallback — lets the open path
    /// detect a wiped primary that still has an intact backup.
    pub fn read_primary<R: Read + Seek>(fp: &mut R, offset: u64) -> std::io::Result<Self> {
        let primary_off = SUPERBLOCK_BACKUP_OFFSETS
            .first()
            .copied()
            .unwrap_or(0x10_000);
        Self::read_one(fp, offset + primary_off)
    }

    /// Read and verify one superblock copy at an absolute byte offset.
    fn read_one<R: Read + Seek>(fp: &mut R, abs_off: u64) -> std::io::Result<Self> {
        let buf = read_at(fp, abs_off, 4096)?;

        let mut magic = [0u8; 8];
        magic.copy_from_slice(&buf[OFF_MAGIC..OFF_MAGIC + 8]);
        if magic != BTRFS_MAGIC {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "not a btrfs superblock: bad magic {:?} (expected {:?})",
                    magic, BTRFS_MAGIC
                ),
            ));
        }

        // The header checksum covers csum_type itself, so a corrupt
        // strategy id fails verification here.
        let csum_type = le_u16(&buf, OFF_CSUM_TYPE);
        if !CsumStrategy::verify_header(csum_type, &buf)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "superblock header checksum mismatch (block corrupt or not a btrfs superblock)",
            ));
        }
        let mut fsid = [0u8; 16];
        fsid.copy_from_slice(&buf[OFF_FSID..OFF_FSID + 16]);

        let incompat_flags = le_u64(&buf, OFF_INCOMPAT_FLAGS);

        let mut metadata_uuid = [0u8; 16];
        metadata_uuid.copy_from_slice(&buf[OFF_METADATA_UUID..OFF_METADATA_UUID + 16]);

        let sys_chunk_array_size = le_u32(&buf, OFF_SYS_CHUNK_ARRAY_SIZE) as usize;
        let sys_chunks = if OFF_SYS_CHUNKS + sys_chunk_array_size <= buf.len() {
            buf[OFF_SYS_CHUNKS..OFF_SYS_CHUNKS + sys_chunk_array_size].to_vec()
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "system chunk array overruns superblock",
            ));
        };

        Ok(Self {
            fsid,
            metadata_uuid,
            incompat_flags,
            bytenr: le_u64(&buf, OFF_BYTENR),
            magic,
            generation: le_u64(&buf, OFF_GENERATION),
            root: le_u64(&buf, OFF_ROOT),
            chunk_root: le_u64(&buf, OFF_CHUNK_ROOT),
            total_bytes: le_u64(&buf, OFF_TOTAL_BYTES),
            bytes_used: le_u64(&buf, OFF_BYTES_USED),
            num_devices: le_u64(&buf, OFF_NUM_DEVICES),
            sector_size: le_u32(&buf, OFF_SECTOR_SIZE),
            node_size: le_u32(&buf, OFF_NODE_SIZE),
            stripesize: le_u32(&buf, OFF_STRIPESIZE),
            sys_chunk_array_size: le_u32(&buf, OFF_SYS_CHUNK_ARRAY_SIZE),
            chunk_root_generation: le_u64(&buf, OFF_CHUNK_ROOT_GENERATION),
            csum_type: le_u16(&buf, OFF_CSUM_TYPE),
            devid: le_u64(&buf, OFF_DEVID),
            sys_chunks,
        })
    }

    /// UUID expected in metadata block headers.
    /// Uses metadata_uuid when METADATA_UUID is enabled, otherwise fsid.
    pub fn expected_metadata_uuid(&self) -> [u8; 16] {
        if self.incompat_flags & BTRFS_FEATURE_INCOMPAT_METADATA_UUID != 0 {
            self.metadata_uuid
        } else {
            self.fsid
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const TEST_FSID: [u8; 16] = [
        0x68, 0xc2, 0xfc, 0x39, 0x2d, 0x33, 0x43, 0xef, 0x8a, 0x47, 0xa0, 0x3f, 0x77, 0x4b, 0x3c,
        0xe3,
    ];

    const TEST_METADATA_UUID: [u8; 16] = [
        0xe8, 0xa2, 0xec, 0x31, 0x83, 0xcf, 0x43, 0x59, 0x97, 0x6f, 0xe5, 0x8c, 0xc0, 0x3d, 0x9a,
        0x96,
    ];

    fn make_superblock(metadata_uuid_enabled: bool) -> Vec<u8> {
        let mut buf = vec![0u8; 4096];

        buf[OFF_MAGIC..OFF_MAGIC + 8].copy_from_slice(&BTRFS_MAGIC);
        buf[OFF_FSID..OFF_FSID + 16].copy_from_slice(&TEST_FSID);

        // Deliberately use the literal on-disk offset, not
        // OFF_METADATA_UUID, so the test catches offset regressions.
        buf[0x23b..0x23b + 16].copy_from_slice(&TEST_METADATA_UUID);

        if metadata_uuid_enabled {
            buf[OFF_INCOMPAT_FLAGS..OFF_INCOMPAT_FLAGS + 8]
                .copy_from_slice(&BTRFS_FEATURE_INCOMPAT_METADATA_UUID.to_le_bytes());
        }

        // Btrfs CRC32C: checksum of bytes 32..4096, stored at bytes 0..4.
        let crc = crc32c::crc32c(&buf[32..]);
        buf[0..4].copy_from_slice(&crc.to_le_bytes());

        buf
    }

    fn parse_test_superblock(buf: Vec<u8>) -> Superblock {
        // read_one() accepts an absolute offset, so no 64 KiB
        // superblock padding is necessary.
        let mut cursor = Cursor::new(buf);
        Superblock::read_one(&mut cursor, 0).expect("test superblock should parse")
    }

    #[test]
    fn metadata_uuid_feature_uses_metadata_uuid() {
        let sb = parse_test_superblock(make_superblock(true));

        assert_eq!(sb.fsid, TEST_FSID);
        assert_eq!(sb.metadata_uuid, TEST_METADATA_UUID);
        assert_eq!(sb.expected_metadata_uuid(), TEST_METADATA_UUID);
    }

    #[test]
    fn without_metadata_uuid_feature_uses_fsid() {
        let sb = parse_test_superblock(make_superblock(false));

        assert_eq!(sb.fsid, TEST_FSID);
        assert_eq!(sb.metadata_uuid, TEST_METADATA_UUID);
        assert_eq!(sb.expected_metadata_uuid(), TEST_FSID);
    }
}
