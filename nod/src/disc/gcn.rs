use std::{
    cmp::min,
    io,
    io::{Read, Seek, SeekFrom},
    mem::size_of,
};

use zerocopy::{FromBytes, FromZeroes};

use crate::{
    disc::{
        ApploaderHeader, DiscHeader, DolHeader, PartitionBase, PartitionHeader, PartitionMeta,
        BI2_SIZE, BOOT_SIZE, SECTOR_SIZE,
    },
    fst::{Node, NodeKind},
    io::block::{Block, BlockIO},
    streams::{ReadStream, SharedWindowedReadStream},
    util::read::{read_box, read_box_slice, read_vec},
    Result, ResultContext,
};

pub struct PartitionGC {
    io: Box<dyn BlockIO>,
    block: Block,
    block_buf: Box<[u8]>,
    block_idx: u32,
    sector_buf: Box<[u8; SECTOR_SIZE]>,
    sector: u32,
    pos: u64,
    disc_header: Box<DiscHeader>,
}

impl Clone for PartitionGC {
    fn clone(&self) -> Self {
        Self {
            io: self.io.clone(),
            block: Block::default(),
            block_buf: <u8>::new_box_slice_zeroed(self.block_buf.len()),
            block_idx: u32::MAX,
            sector_buf: <[u8; SECTOR_SIZE]>::new_box_zeroed(),
            sector: u32::MAX,
            pos: 0,
            disc_header: self.disc_header.clone(),
        }
    }
}

impl PartitionGC {
    pub fn new(inner: Box<dyn BlockIO>, disc_header: Box<DiscHeader>) -> Result<Box<Self>> {
        let block_size = inner.block_size();
        Ok(Box::new(Self {
            io: inner,
            block: Block::default(),
            block_buf: <u8>::new_box_slice_zeroed(block_size as usize),
            block_idx: u32::MAX,
            sector_buf: <[u8; SECTOR_SIZE]>::new_box_zeroed(),
            sector: u32::MAX,
            pos: 0,
            disc_header,
        }))
    }

    pub fn into_inner(self) -> Box<dyn BlockIO> { self.io }
}

impl Read for PartitionGC {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let sector = (self.pos / SECTOR_SIZE as u64) as u32;
        let block_idx = (sector as u64 * SECTOR_SIZE as u64 / self.block_buf.len() as u64) as u32;

        // Read new block if necessary
        if block_idx != self.block_idx {
            self.block = self.io.read_block(self.block_buf.as_mut(), block_idx, None)?;
            self.block_idx = block_idx;
        }

        // Copy sector if necessary
        if sector != self.sector {
            self.block.copy_raw(
                self.sector_buf.as_mut(),
                self.block_buf.as_ref(),
                sector,
                &self.disc_header,
            )?;
            self.sector = sector;
        }

        let offset = (self.pos % SECTOR_SIZE as u64) as usize;
        let len = min(buf.len(), SECTOR_SIZE - offset);
        buf[..len].copy_from_slice(&self.sector_buf[offset..offset + len]);
        self.pos += len as u64;
        Ok(len)
    }
}

impl Seek for PartitionGC {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = match pos {
            SeekFrom::Start(v) => v,
            SeekFrom::End(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "GCPartitionReader: SeekFrom::End is not supported".to_string(),
                ));
            }
            SeekFrom::Current(v) => self.pos.saturating_add_signed(v),
        };
        Ok(self.pos)
    }
}

impl PartitionBase for PartitionGC {
    fn meta(&mut self) -> Result<Box<PartitionMeta>> {
        self.seek(SeekFrom::Start(0)).context("Seeking to partition metadata")?;
        read_part_meta(self, false)
    }

    fn open_file(&mut self, node: &Node) -> io::Result<SharedWindowedReadStream> {
        assert_eq!(node.kind(), NodeKind::File);
        self.new_window(node.offset(false), node.length())
    }

    fn ideal_buffer_size(&self) -> usize { SECTOR_SIZE }
}

pub(crate) fn read_part_meta(
    reader: &mut dyn ReadStream,
    is_wii: bool,
) -> Result<Box<PartitionMeta>> {
    // boot.bin
    let raw_boot: Box<[u8; BOOT_SIZE]> = read_box(reader).context("Reading boot.bin")?;
    let partition_header = PartitionHeader::ref_from(&raw_boot[size_of::<DiscHeader>()..]).unwrap();

    // bi2.bin
    let raw_bi2: Box<[u8; BI2_SIZE]> = read_box(reader).context("Reading bi2.bin")?;

    // apploader.bin
    let mut raw_apploader: Vec<u8> =
        read_vec(reader, size_of::<ApploaderHeader>()).context("Reading apploader header")?;
    let apploader_header = ApploaderHeader::ref_from(raw_apploader.as_slice()).unwrap();
    raw_apploader.resize(
        size_of::<ApploaderHeader>()
            + apploader_header.size.get() as usize
            + apploader_header.trailer_size.get() as usize,
        0,
    );
    reader
        .read_exact(&mut raw_apploader[size_of::<ApploaderHeader>()..])
        .context("Reading apploader")?;

    // fst.bin
    reader
        .seek(SeekFrom::Start(partition_header.fst_offset(is_wii)))
        .context("Seeking to FST offset")?;
    let raw_fst: Box<[u8]> = read_box_slice(reader, partition_header.fst_size(is_wii) as usize)
        .with_context(|| {
            format!(
                "Reading partition FST (offset {}, size {})",
                partition_header.fst_offset(is_wii),
                partition_header.fst_size(is_wii)
            )
        })?;

    // main.dol
    reader
        .seek(SeekFrom::Start(partition_header.dol_offset(is_wii)))
        .context("Seeking to DOL offset")?;
    let mut raw_dol: Vec<u8> =
        read_vec(reader, size_of::<DolHeader>()).context("Reading DOL header")?;
    let dol_header = DolHeader::ref_from(raw_dol.as_slice()).unwrap();
    let dol_size = dol_header
        .text_offs
        .iter()
        .zip(&dol_header.text_sizes)
        .map(|(offs, size)| offs.get() + size.get())
        .chain(
            dol_header
                .data_offs
                .iter()
                .zip(&dol_header.data_sizes)
                .map(|(offs, size)| offs.get() + size.get()),
        )
        .max()
        .unwrap_or(size_of::<DolHeader>() as u32);
    raw_dol.resize(dol_size as usize, 0);
    reader.read_exact(&mut raw_dol[size_of::<DolHeader>()..]).context("Reading DOL")?;

    Ok(Box::new(PartitionMeta {
        raw_boot,
        raw_bi2,
        raw_apploader: raw_apploader.into_boxed_slice(),
        raw_fst,
        raw_dol: raw_dol.into_boxed_slice(),
        raw_ticket: None,
        raw_tmd: None,
        raw_cert_chain: None,
        raw_h3_table: None,
    }))
}
