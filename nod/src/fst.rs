//! Disc file system types

use std::{borrow::Cow, ffi::CStr, mem::size_of};

use encoding_rs::SHIFT_JIS;
use zerocopy::{big_endian::*, AsBytes, FromBytes, FromZeroes};

use crate::{static_assert, Result};

/// File system node kind.
#[derive(Clone, Debug, PartialEq)]
pub enum NodeKind {
    /// Node is a file.
    File,
    /// Node is a directory.
    Directory,
    /// Invalid node kind. (Should not normally occur)
    Invalid,
}

/// An individual file system node.
#[derive(Clone, Debug, PartialEq, FromBytes, FromZeroes, AsBytes)]
#[repr(C, align(4))]
pub struct Node {
    kind: u8,
    // u24 big-endian
    name_offset: [u8; 3],
    offset: U32,
    length: U32,
}

static_assert!(size_of::<Node>() == 12);

impl Node {
    /// File system node kind.
    pub fn kind(&self) -> NodeKind {
        match self.kind {
            0 => NodeKind::File,
            1 => NodeKind::Directory,
            _ => NodeKind::Invalid,
        }
    }

    /// Whether the node is a file.
    pub fn is_file(&self) -> bool { self.kind == 0 }

    /// Whether the node is a directory.
    pub fn is_dir(&self) -> bool { self.kind == 1 }

    /// Offset in the string table to the filename.
    pub fn name_offset(&self) -> u32 {
        u32::from_be_bytes([0, self.name_offset[0], self.name_offset[1], self.name_offset[2]])
    }

    /// For files, this is the partition offset of the file data. (Wii: >> 2)
    ///
    /// For directories, this is the parent node index in the FST.
    pub fn offset(&self, is_wii: bool) -> u64 {
        if is_wii && self.kind == 0 {
            self.offset.get() as u64 * 4
        } else {
            self.offset.get() as u64
        }
    }

    /// For files, this is the byte size of the file.
    ///
    /// For directories, this is the child end index in the FST.
    ///
    /// Number of child files and directories recursively is `length - offset`.
    pub fn length(&self) -> u64 { self.length.get() as u64 }
}

/// A view into the file system table (FST).
pub struct Fst<'a> {
    /// The nodes in the FST.
    pub nodes: &'a [Node],
    /// The string table containing all file and directory names.
    pub string_table: &'a [u8],
}

impl<'a> Fst<'a> {
    /// Create a new FST view from a buffer.
    pub fn new(buf: &'a [u8]) -> Result<Self, &'static str> {
        let Some(root_node) = Node::ref_from_prefix(buf) else {
            return Err("FST root node not found");
        };
        // String table starts after the last node
        let string_base = root_node.length() * size_of::<Node>() as u64;
        if string_base >= buf.len() as u64 {
            return Err("FST string table out of bounds");
        }
        let (node_buf, string_table) = buf.split_at(string_base as usize);
        let nodes = Node::slice_from(node_buf).unwrap();
        Ok(Self { nodes, string_table })
    }

    /// Iterate over the nodes in the FST.
    pub fn iter(&self) -> FstIter { FstIter { fst: self, idx: 1 } }

    /// Get the name of a node.
    pub fn get_name(&self, node: &Node) -> Result<Cow<str>, String> {
        let name_buf = self.string_table.get(node.name_offset() as usize..).ok_or_else(|| {
            format!(
                "FST: name offset {} out of bounds (string table size: {})",
                node.name_offset(),
                self.string_table.len()
            )
        })?;
        let c_string = CStr::from_bytes_until_nul(name_buf).map_err(|_| {
            format!("FST: name at offset {} not null-terminated", node.name_offset())
        })?;
        let (decoded, _, errors) = SHIFT_JIS.decode(c_string.to_bytes());
        if errors {
            return Err(format!("FST: Failed to decode name at offset {}", node.name_offset()));
        }
        Ok(decoded)
    }

    /// Finds a particular file or directory by path.
    pub fn find(&self, path: &str) -> Option<(usize, &Node)> {
        let mut split = path.trim_matches('/').split('/');
        let mut current = split.next()?;
        let mut idx = 1;
        let mut stop_at = None;
        while let Some(node) = self.nodes.get(idx) {
            if self.get_name(node).as_ref().map_or(false, |name| name.eq_ignore_ascii_case(current))
            {
                if let Some(next) = split.next() {
                    current = next;
                } else {
                    return Some((idx, node));
                }
                // Descend into directory
                idx += 1;
                stop_at = Some(node.length() as usize + idx);
            } else if node.is_dir() {
                // Skip directory
                idx = node.length() as usize;
            } else {
                // Skip file
                idx += 1;
            }
            if let Some(stop) = stop_at {
                if idx >= stop {
                    break;
                }
            }
        }
        None
    }
}

/// Iterator over the nodes in an FST.
pub struct FstIter<'a> {
    fst: &'a Fst<'a>,
    idx: usize,
}

impl<'a> Iterator for FstIter<'a> {
    type Item = (usize, &'a Node, Result<Cow<'a, str>, String>);

    fn next(&mut self) -> Option<Self::Item> {
        let idx = self.idx;
        let node = self.fst.nodes.get(idx)?;
        let name = self.fst.get_name(node);
        self.idx += 1;
        Some((idx, node, name))
    }
}
