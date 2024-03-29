#![warn(missing_docs)]
//! Library for traversing & reading Nintendo Optical Disc (GameCube and Wii) images.
//!
//! Originally based on the C++ library [nod](https://github.com/AxioDL/nod),
//! but does not currently support authoring.
//!
//! Currently supported file formats:
//! - ISO (GCM)
//! - WIA / RVZ
//! - WBFS (+ NKit 2 lossless)
//! - CISO (+ NKit 2 lossless)
//! - NFS (Wii U VC)
//! - GCZ
//!
//! # Examples
//!
//! Opening a disc image and reading a file:
//!
//! ```no_run
//! use std::io::Read;
//!
//! // Open a disc image and the first data partition.
//! let disc = nod::Disc::new("path/to/file.iso")
//!     .expect("Failed to open disc");
//! let mut partition = disc.open_partition_kind(nod::PartitionKind::Data)
//!     .expect("Failed to open data partition");
//!
//! // Read partition metadata and the file system table.
//! let meta = partition.meta()
//!     .expect("Failed to read partition metadata");
//! let fst = meta.fst()
//!     .expect("File system table is invalid");
//!
//! // Find a file by path and read it into a string.
//! if let Some((_, node)) = fst.find("/MP3/Worlds.txt") {
//!     let mut s = String::new();
//!     partition
//!         .open_file(node)
//!         .expect("Failed to open file stream")
//!         .read_to_string(&mut s)
//!         .expect("Failed to read file");
//!     println!("{}", s);
//! }
//! ```
//!
//! Converting a disc image to raw ISO:
//!
//! ```no_run
//! // Enable `rebuild_encryption` to ensure the output is a valid ISO.
//! let options = nod::OpenOptions { rebuild_encryption: true, ..Default::default() };
//! let mut disc = nod::Disc::new_with_options("path/to/file.rvz", &options)
//!     .expect("Failed to open disc");
//!
//! // Read directly from the open disc and write to the output file.
//! let mut out = std::fs::File::create("output.iso")
//!     .expect("Failed to create output file");
//! std::io::copy(&mut disc, &mut out)
//!     .expect("Failed to write data");
//! ```

use std::{
    io::{Read, Seek},
    path::Path,
};

pub use disc::{
    ApploaderHeader, DiscHeader, DolHeader, PartitionBase, PartitionHeader, PartitionKind,
    PartitionMeta, BI2_SIZE, BOOT_SIZE, SECTOR_SIZE,
};
pub use fst::{Fst, Node, NodeKind};
pub use io::{block::PartitionInfo, Compression, DiscMeta, Format};
pub use streams::ReadStream;

mod disc;
mod fst;
mod io;
mod streams;
mod util;

/// Error types for nod.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// An error for disc format related issues.
    #[error("disc format error: {0}")]
    DiscFormat(String),
    /// A general I/O error.
    #[error("I/O error: {0}")]
    Io(String, #[source] std::io::Error),
    /// An unknown error.
    #[error("error: {0}")]
    Other(String),
}

impl From<&str> for Error {
    fn from(s: &str) -> Error { Error::Other(s.to_string()) }
}

impl From<String> for Error {
    fn from(s: String) -> Error { Error::Other(s) }
}

/// Helper result type for [`Error`].
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Helper trait for adding context to errors.
pub trait ErrorContext {
    /// Adds context to an error.
    fn context(self, context: impl Into<String>) -> Error;
}

impl ErrorContext for std::io::Error {
    fn context(self, context: impl Into<String>) -> Error { Error::Io(context.into(), self) }
}

/// Helper trait for adding context to result errors.
pub trait ResultContext<T> {
    /// Adds context to a result error.
    fn context(self, context: impl Into<String>) -> Result<T>;

    /// Adds context to a result error using a closure.
    fn with_context<F>(self, f: F) -> Result<T>
    where F: FnOnce() -> String;
}

impl<T, E> ResultContext<T> for Result<T, E>
where E: ErrorContext
{
    fn context(self, context: impl Into<String>) -> Result<T> {
        self.map_err(|e| e.context(context))
    }

    fn with_context<F>(self, f: F) -> Result<T>
    where F: FnOnce() -> String {
        self.map_err(|e| e.context(f()))
    }
}

/// Options for opening a disc image.
#[derive(Default, Debug, Clone)]
pub struct OpenOptions {
    /// Wii: Rebuild partition data encryption and hashes if the underlying format stores data
    /// decrypted or with hashes removed. (e.g. WIA/RVZ, NFS)
    pub rebuild_encryption: bool,
    /// Wii: Validate partition data hashes while reading the disc image.
    pub validate_hashes: bool,
}

/// An open disc image and read stream.
///
/// This is the primary entry point for reading disc images.
pub struct Disc {
    reader: disc::reader::DiscReader,
    options: OpenOptions,
}

impl Disc {
    /// Opens a disc image from a file path.
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Disc> {
        Disc::new_with_options(path, &OpenOptions::default())
    }

    /// Opens a disc image from a file path with custom options.
    pub fn new_with_options<P: AsRef<Path>>(path: P, options: &OpenOptions) -> Result<Disc> {
        let io = io::block::open(path.as_ref())?;
        let reader = disc::reader::DiscReader::new(io, options)?;
        Ok(Disc { reader, options: options.clone() })
    }

    /// The disc's primary header.
    pub fn header(&self) -> &DiscHeader { self.reader.header() }

    /// Returns extra metadata included in the disc file format, if any.
    pub fn meta(&self) -> DiscMeta { self.reader.meta() }

    /// The disc's size in bytes, or an estimate if not stored by the format.
    pub fn disc_size(&self) -> u64 { self.reader.disc_size() }

    /// A list of Wii partitions on the disc.
    ///
    /// **GameCube**: This will return an empty slice.
    pub fn partitions(&self) -> &[PartitionInfo] { self.reader.partitions() }

    /// Opens a decrypted partition read stream for the specified partition index.
    ///
    /// **GameCube**: `index` must always be 0.
    pub fn open_partition(&self, index: usize) -> Result<Box<dyn PartitionBase>> {
        self.reader.open_partition(index, &self.options)
    }

    /// Opens a decrypted partition read stream for the first partition matching
    /// the specified kind.
    ///
    /// **GameCube**: `kind` must always be [`PartitionKind::Data`].
    pub fn open_partition_kind(&self, kind: PartitionKind) -> Result<Box<dyn PartitionBase>> {
        self.reader.open_partition_kind(kind, &self.options)
    }
}

impl Read for Disc {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> { self.reader.read(buf) }
}

impl Seek for Disc {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> { self.reader.seek(pos) }
}
