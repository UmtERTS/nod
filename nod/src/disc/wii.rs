use std::{
    cmp::min,
    ffi::CStr,
    io,
    io::{Read, Seek, SeekFrom},
    mem::size_of,
};

use sha1::{digest, Digest, Sha1};
use zerocopy::{big_endian::*, AsBytes, FromBytes, FromZeroes};

use crate::{
    array_ref,
    disc::{
        gcn::{read_part_meta, PartitionGC},
        PartitionBase, PartitionKind, PartitionMeta, SECTOR_SIZE,
    },
    fst::{Node, NodeKind},
    io::{
        aes_decrypt,
        block::{Block, BlockIO, PartitionInfo},
        KeyBytes,
    },
    static_assert,
    streams::{ReadStream, SharedWindowedReadStream},
    util::{div_rem, read::read_box_slice},
    DiscHeader, Error, OpenOptions, Result, ResultContext,
};

pub(crate) const HASHES_SIZE: usize = 0x400;
pub(crate) const SECTOR_DATA_SIZE: usize = SECTOR_SIZE - HASHES_SIZE; // 0x7C00

// ppki (Retail)
const RVL_CERT_ISSUER_PPKI_TICKET: &str = "Root-CA00000001-XS00000003";
#[rustfmt::skip]
const RETAIL_COMMON_KEYS: [KeyBytes; 3] = [
    /* RVL_KEY_RETAIL */
    [0xeb, 0xe4, 0x2a, 0x22, 0x5e, 0x85, 0x93, 0xe4, 0x48, 0xd9, 0xc5, 0x45, 0x73, 0x81, 0xaa, 0xf7],
    /* RVL_KEY_KOREAN */
    [0x63, 0xb8, 0x2b, 0xb4, 0xf4, 0x61, 0x4e, 0x2e, 0x13, 0xf2, 0xfe, 0xfb, 0xba, 0x4c, 0x9b, 0x7e],
    /* vWii_KEY_RETAIL */
    [0x30, 0xbf, 0xc7, 0x6e, 0x7c, 0x19, 0xaf, 0xbb, 0x23, 0x16, 0x33, 0x30, 0xce, 0xd7, 0xc2, 0x8d],
];

// dpki (Debug)
const RVL_CERT_ISSUER_DPKI_TICKET: &str = "Root-CA00000002-XS00000006";
#[rustfmt::skip]
const DEBUG_COMMON_KEYS: [KeyBytes; 3] = [
    /* RVL_KEY_DEBUG */
    [0xa1, 0x60, 0x4a, 0x6a, 0x71, 0x23, 0xb5, 0x29, 0xae, 0x8b, 0xec, 0x32, 0xc8, 0x16, 0xfc, 0xaa],
    /* RVL_KEY_KOREAN_DEBUG */
    [0x67, 0x45, 0x8b, 0x6b, 0xc6, 0x23, 0x7b, 0x32, 0x69, 0x98, 0x3c, 0x64, 0x73, 0x48, 0x33, 0x66],
    /* vWii_KEY_DEBUG */
    [0x2f, 0x5c, 0x1b, 0x29, 0x44, 0xe7, 0xfd, 0x6f, 0xc3, 0x97, 0x96, 0x4b, 0x05, 0x76, 0x91, 0xfa],
];

#[derive(Debug, PartialEq, FromBytes, FromZeroes, AsBytes)]
#[repr(C, align(4))]
pub(crate) struct WiiPartEntry {
    pub(crate) offset: U32,
    pub(crate) kind: U32,
}

static_assert!(size_of::<WiiPartEntry>() == 8);

impl WiiPartEntry {
    pub(crate) fn offset(&self) -> u64 { (self.offset.get() as u64) << 2 }
}

#[derive(Debug, PartialEq)]
pub(crate) struct WiiPartInfo {
    pub(crate) group_idx: u32,
    pub(crate) part_idx: u32,
    pub(crate) offset: u64,
    pub(crate) kind: PartitionKind,
    pub(crate) header: WiiPartitionHeader,
    pub(crate) junk_id: [u8; 4],
    pub(crate) junk_start: u64,
    pub(crate) title_key: KeyBytes,
}

pub(crate) const WII_PART_GROUP_OFF: u64 = 0x40000;

#[derive(Debug, PartialEq, FromBytes, FromZeroes, AsBytes)]
#[repr(C, align(4))]
pub(crate) struct WiiPartGroup {
    pub(crate) part_count: U32,
    pub(crate) part_entry_off: U32,
}

static_assert!(size_of::<WiiPartGroup>() == 8);

impl WiiPartGroup {
    pub(crate) fn part_entry_off(&self) -> u64 { (self.part_entry_off.get() as u64) << 2 }
}

#[derive(Debug, Clone, PartialEq, FromBytes, FromZeroes, AsBytes)]
#[repr(C, align(4))]
pub struct SignedHeader {
    /// Signature type, always 0x00010001 (RSA-2048)
    pub sig_type: U32,
    /// RSA-2048 signature
    pub sig: [u8; 256],
    _pad: [u8; 60],
}

static_assert!(size_of::<SignedHeader>() == 0x140);

#[derive(Debug, Clone, PartialEq, Default, FromBytes, FromZeroes, AsBytes)]
#[repr(C, align(4))]
pub struct TicketTimeLimit {
    pub enable_time_limit: U32,
    pub time_limit: U32,
}

static_assert!(size_of::<TicketTimeLimit>() == 8);

#[derive(Debug, Clone, PartialEq, FromBytes, FromZeroes, AsBytes)]
#[repr(C, align(4))]
pub struct Ticket {
    pub header: SignedHeader,
    pub sig_issuer: [u8; 64],
    pub ecdh: [u8; 60],
    pub version: u8,
    _pad1: U16,
    pub title_key: KeyBytes,
    _pad2: u8,
    pub ticket_id: [u8; 8],
    pub console_id: [u8; 4],
    pub title_id: [u8; 8],
    _pad3: U16,
    pub ticket_title_version: U16,
    pub permitted_titles_mask: U32,
    pub permit_mask: U32,
    pub title_export_allowed: u8,
    pub common_key_idx: u8,
    _pad4: [u8; 48],
    pub content_access_permissions: [u8; 64],
    _pad5: [u8; 2],
    pub time_limits: [TicketTimeLimit; 8],
}

static_assert!(size_of::<Ticket>() == 0x2A4);

impl Ticket {
    pub fn decrypt_title_key(&self) -> Result<KeyBytes> {
        let mut iv: KeyBytes = [0; 16];
        iv[..8].copy_from_slice(&self.title_id);
        let cert_issuer_ticket =
            CStr::from_bytes_until_nul(&self.sig_issuer).ok().and_then(|c| c.to_str().ok());
        let common_keys = match cert_issuer_ticket {
            Some(RVL_CERT_ISSUER_PPKI_TICKET) => &RETAIL_COMMON_KEYS,
            Some(RVL_CERT_ISSUER_DPKI_TICKET) => &DEBUG_COMMON_KEYS,
            Some(v) => {
                return Err(Error::DiscFormat(format!("unknown certificate issuer {:?}", v)));
            }
            None => {
                return Err(Error::DiscFormat("failed to parse certificate issuer".to_string()));
            }
        };
        let common_key = common_keys.get(self.common_key_idx as usize).ok_or(Error::DiscFormat(
            format!("unknown common key index {}", self.common_key_idx),
        ))?;
        let mut title_key = self.title_key;
        aes_decrypt(common_key, iv, &mut title_key);
        Ok(title_key)
    }
}

#[derive(Debug, Clone, PartialEq, FromBytes, FromZeroes, AsBytes)]
#[repr(C, align(4))]
pub struct TmdHeader {
    pub header: SignedHeader,
    pub sig_issuer: [u8; 64],
    pub version: u8,
    pub ca_crl_version: u8,
    pub signer_crl_version: u8,
    pub is_vwii: u8,
    pub ios_id: [u8; 8],
    pub title_id: [u8; 8],
    pub title_type: u32,
    pub group_id: U16,
    _pad1: [u8; 2],
    pub region: U16,
    pub ratings: KeyBytes,
    _pad2: [u8; 12],
    pub ipc_mask: [u8; 12],
    _pad3: [u8; 18],
    pub access_flags: U32,
    pub title_version: U16,
    pub num_contents: U16,
    pub boot_idx: U16,
    pub minor_version: U16,
}

static_assert!(size_of::<TmdHeader>() == 0x1E4);

pub const H3_TABLE_SIZE: usize = 0x18000;

#[derive(Debug, Clone, PartialEq, FromBytes, FromZeroes, AsBytes)]
#[repr(C, align(4))]
pub struct WiiPartitionHeader {
    pub ticket: Ticket,
    tmd_size: U32,
    tmd_off: U32,
    cert_chain_size: U32,
    cert_chain_off: U32,
    h3_table_off: U32,
    data_off: U32,
    data_size: U32,
}

static_assert!(size_of::<WiiPartitionHeader>() == 0x2C0);

impl WiiPartitionHeader {
    pub fn tmd_size(&self) -> u64 { self.tmd_size.get() as u64 }

    pub fn tmd_off(&self) -> u64 { (self.tmd_off.get() as u64) << 2 }

    pub fn cert_chain_size(&self) -> u64 { self.cert_chain_size.get() as u64 }

    pub fn cert_chain_off(&self) -> u64 { (self.cert_chain_off.get() as u64) << 2 }

    pub fn h3_table_off(&self) -> u64 { (self.h3_table_off.get() as u64) << 2 }

    pub fn h3_table_size(&self) -> u64 { H3_TABLE_SIZE as u64 }

    pub fn data_off(&self) -> u64 { (self.data_off.get() as u64) << 2 }

    pub fn data_size(&self) -> u64 { (self.data_size.get() as u64) << 2 }
}

pub struct PartitionWii {
    io: Box<dyn BlockIO>,
    partition: PartitionInfo,
    block: Option<Block>,
    block_buf: Box<[u8]>,
    block_idx: u32,
    sector_buf: Box<[u8; SECTOR_SIZE]>,
    sector: u32,
    pos: u64,
    verify: bool,
    raw_tmd: Box<[u8]>,
    raw_cert_chain: Box<[u8]>,
    raw_h3_table: Box<[u8]>,
}

impl Clone for PartitionWii {
    fn clone(&self) -> Self {
        Self {
            io: self.io.clone(),
            partition: self.partition.clone(),
            block: None,
            block_buf: <u8>::new_box_slice_zeroed(self.block_buf.len()),
            block_idx: u32::MAX,
            sector_buf: <[u8; SECTOR_SIZE]>::new_box_zeroed(),
            sector: u32::MAX,
            pos: 0,
            verify: self.verify,
            raw_tmd: self.raw_tmd.clone(),
            raw_cert_chain: self.raw_cert_chain.clone(),
            raw_h3_table: self.raw_h3_table.clone(),
        }
    }
}

impl PartitionWii {
    pub fn new(
        inner: Box<dyn BlockIO>,
        disc_header: Box<DiscHeader>,
        partition: &PartitionInfo,
        options: &OpenOptions,
    ) -> Result<Box<Self>> {
        let block_size = inner.block_size();
        let mut reader = PartitionGC::new(inner, disc_header)?;

        // Read TMD, cert chain, and H3 table
        let offset = partition.start_sector as u64 * SECTOR_SIZE as u64;
        reader
            .seek(SeekFrom::Start(offset + partition.header.tmd_off()))
            .context("Seeking to TMD offset")?;
        let raw_tmd: Box<[u8]> = read_box_slice(&mut reader, partition.header.tmd_size() as usize)
            .context("Reading TMD")?;
        reader
            .seek(SeekFrom::Start(offset + partition.header.cert_chain_off()))
            .context("Seeking to cert chain offset")?;
        let raw_cert_chain: Box<[u8]> =
            read_box_slice(&mut reader, partition.header.cert_chain_size() as usize)
                .context("Reading cert chain")?;
        reader
            .seek(SeekFrom::Start(offset + partition.header.h3_table_off()))
            .context("Seeking to H3 table offset")?;
        let raw_h3_table: Box<[u8]> =
            read_box_slice(&mut reader, H3_TABLE_SIZE).context("Reading H3 table")?;

        Ok(Box::new(Self {
            io: reader.into_inner(),
            partition: partition.clone(),
            block: None,
            block_buf: <u8>::new_box_slice_zeroed(block_size as usize),
            block_idx: u32::MAX,
            sector_buf: <[u8; SECTOR_SIZE]>::new_box_zeroed(),
            sector: u32::MAX,
            pos: 0,
            verify: options.validate_hashes,
            raw_tmd,
            raw_cert_chain,
            raw_h3_table,
        }))
    }
}

impl Read for PartitionWii {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let partition_sector = (self.pos / SECTOR_DATA_SIZE as u64) as u32;
        let sector = self.partition.data_start_sector + partition_sector;
        if sector >= self.partition.data_end_sector {
            return Ok(0);
        }
        let block_idx = (sector as u64 * SECTOR_SIZE as u64 / self.block_buf.len() as u64) as u32;

        // Read new block if necessary
        if block_idx != self.block_idx {
            self.block =
                self.io.read_block(self.block_buf.as_mut(), block_idx, Some(&self.partition))?;
            self.block_idx = block_idx;
        }

        // Decrypt sector if necessary
        if sector != self.sector {
            let Some(block) = &self.block else {
                return Ok(0);
            };
            block.decrypt(
                self.sector_buf.as_mut(),
                self.block_buf.as_ref(),
                block_idx,
                sector,
                &self.partition,
            )?;
            if self.verify {
                verify_hashes(&self.sector_buf, sector)?;
            }
            self.sector = sector;
        }

        let offset = (self.pos % SECTOR_DATA_SIZE as u64) as usize;
        let len = min(buf.len(), SECTOR_DATA_SIZE - offset);
        buf[..len]
            .copy_from_slice(&self.sector_buf[HASHES_SIZE + offset..HASHES_SIZE + offset + len]);
        self.pos += len as u64;
        Ok(len)
    }
}

impl Seek for PartitionWii {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = match pos {
            SeekFrom::Start(v) => v,
            SeekFrom::End(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "WiiPartitionReader: SeekFrom::End is not supported".to_string(),
                ));
            }
            SeekFrom::Current(v) => self.pos.saturating_add_signed(v),
        };
        Ok(self.pos)
    }
}

#[inline(always)]
pub(crate) fn as_digest(slice: &[u8; 20]) -> digest::Output<Sha1> { (*slice).into() }

fn verify_hashes(buf: &[u8; SECTOR_SIZE], sector: u32) -> io::Result<()> {
    let (mut group, sub_group) = div_rem(sector as usize, 8);
    group %= 8;

    // H0 hashes
    for i in 0..31 {
        let mut hash = Sha1::new();
        hash.update(array_ref![buf, (i + 1) * 0x400, 0x400]);
        let expected = as_digest(array_ref![buf, i * 20, 20]);
        let output = hash.finalize();
        if output != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Invalid H0 hash! (block {:?}) {:x}\n\texpected {:x}", i, output, expected),
            ));
        }
    }

    // H1 hash
    {
        let mut hash = Sha1::new();
        hash.update(array_ref![buf, 0, 0x26C]);
        let expected = as_digest(array_ref![buf, 0x280 + sub_group * 20, 20]);
        let output = hash.finalize();
        if output != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Invalid H1 hash! (subgroup {:?}) {:x}\n\texpected {:x}",
                    sub_group, output, expected
                ),
            ));
        }
    }

    // H2 hash
    {
        let mut hash = Sha1::new();
        hash.update(array_ref![buf, 0x280, 0xA0]);
        let expected = as_digest(array_ref![buf, 0x340 + group * 20, 20]);
        let output = hash.finalize();
        if output != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Invalid H2 hash! (group {:?}) {:x}\n\texpected {:x}",
                    group, output, expected
                ),
            ));
        }
    }
    // TODO H3 hash
    Ok(())
}

impl PartitionBase for PartitionWii {
    fn meta(&mut self) -> Result<Box<PartitionMeta>> {
        self.seek(SeekFrom::Start(0)).context("Seeking to partition header")?;
        let mut meta = read_part_meta(self, true)?;
        meta.raw_ticket = Some(Box::from(self.partition.header.ticket.as_bytes()));
        meta.raw_tmd = Some(self.raw_tmd.clone());
        meta.raw_cert_chain = Some(self.raw_cert_chain.clone());
        meta.raw_h3_table = Some(self.raw_h3_table.clone());
        Ok(meta)
    }

    fn open_file(&mut self, node: &Node) -> io::Result<SharedWindowedReadStream> {
        assert_eq!(node.kind(), NodeKind::File);
        self.new_window(node.offset(true), node.length(true))
    }

    fn ideal_buffer_size(&self) -> usize { SECTOR_DATA_SIZE }
}
