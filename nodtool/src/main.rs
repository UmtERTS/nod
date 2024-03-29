mod argp_version;
mod digest;
mod redump;

use std::{
    borrow::Cow,
    cmp::min,
    env,
    error::Error,
    ffi::OsStr,
    fmt, fs,
    fs::File,
    io,
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf, MAIN_SEPARATOR},
    str::FromStr,
    sync::{mpsc::sync_channel, Arc},
    thread,
};

use argp::{FromArgValue, FromArgs};
use digest::{digest_thread, DigestResult};
use enable_ansi_support::enable_ansi_support;
use indicatif::{ProgressBar, ProgressState, ProgressStyle};
use itertools::Itertools;
use nod::{
    Compression, Disc, DiscHeader, DiscMeta, Fst, Node, OpenOptions, PartitionBase, PartitionKind,
    PartitionMeta, Result, ResultContext, SECTOR_SIZE,
};
use size::{Base, Size};
use supports_color::Stream;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;
use zerocopy::{AsBytes, FromZeroes};

#[derive(FromArgs, Debug)]
/// Tool for reading GameCube and Wii disc images.
struct TopLevel {
    #[argp(subcommand)]
    command: SubCommand,
    #[argp(option, short = 'C')]
    /// Change working directory.
    chdir: Option<PathBuf>,
    #[argp(option, short = 'L')]
    /// Minimum logging level. (Default: info)
    /// Possible values: error, warn, info, debug, trace
    log_level: Option<LogLevel>,
    #[allow(unused)]
    #[argp(switch, short = 'V')]
    /// Print version information and exit.
    version: bool,
    #[argp(switch)]
    /// Disable color output. (env: NO_COLOR)
    no_color: bool,
}

#[derive(FromArgs, Debug)]
#[argp(subcommand)]
enum SubCommand {
    Info(InfoArgs),
    Extract(ExtractArgs),
    Convert(ConvertArgs),
    Verify(VerifyArgs),
}

#[derive(FromArgs, Debug)]
/// Displays information about disc images.
#[argp(subcommand, name = "info")]
struct InfoArgs {
    #[argp(positional)]
    /// Path to disc image(s)
    file: Vec<PathBuf>,
}

#[derive(FromArgs, Debug)]
/// Extract a disc image.
#[argp(subcommand, name = "extract")]
struct ExtractArgs {
    #[argp(positional)]
    /// Path to disc image
    file: PathBuf,
    #[argp(positional)]
    /// Output directory (optional)
    out: Option<PathBuf>,
    #[argp(switch, short = 'q')]
    /// Quiet output
    quiet: bool,
    #[argp(switch, short = 'h')]
    /// Validate data hashes (Wii only)
    validate: bool,
    #[argp(option, short = 'p')]
    /// Partition to extract (default: data)
    /// Options: all, data, update, channel, or a partition index
    partition: Option<String>,
}

#[derive(FromArgs, Debug)]
/// Converts a disc image to ISO.
#[argp(subcommand, name = "convert")]
struct ConvertArgs {
    #[argp(positional)]
    /// path to disc image
    file: PathBuf,
    #[argp(positional)]
    /// output ISO file
    out: PathBuf,
    #[argp(switch)]
    /// enable MD5 hashing (slower)
    md5: bool,
}

#[derive(FromArgs, Debug)]
/// Verifies disc images.
#[argp(subcommand, name = "verify")]
struct VerifyArgs {
    #[argp(positional)]
    /// path to disc image(s)
    file: Vec<PathBuf>,
    #[argp(switch)]
    /// enable MD5 hashing (slower)
    md5: bool,
}

#[derive(Debug, Eq, PartialEq, Copy, Clone)]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl FromStr for LogLevel {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "error" => Self::Error,
            "warn" => Self::Warn,
            "info" => Self::Info,
            "debug" => Self::Debug,
            "trace" => Self::Trace,
            _ => return Err(()),
        })
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        })
    }
}

impl FromArgValue for LogLevel {
    fn from_arg_value(value: &OsStr) -> std::result::Result<Self, String> {
        String::from_arg_value(value)
            .and_then(|s| Self::from_str(&s).map_err(|_| "Invalid log level".to_string()))
    }
}

// Duplicated from supports-color so we can check early.
fn env_no_color() -> bool {
    match env::var("NO_COLOR").as_deref() {
        Ok("") | Ok("0") | Err(_) => false,
        Ok(_) => true,
    }
}

fn main() {
    let args: TopLevel = argp_version::from_env();
    let use_colors = if args.no_color || env_no_color() {
        false
    } else {
        // Try to enable ANSI support on Windows.
        let _ = enable_ansi_support();
        // Disable isatty check for supports-color. (e.g. when used with ninja)
        env::set_var("IGNORE_IS_TERMINAL", "1");
        supports_color::on(Stream::Stdout).is_some_and(|c| c.has_basic)
    };

    let format =
        tracing_subscriber::fmt::format().with_ansi(use_colors).with_target(false).without_time();
    let builder = tracing_subscriber::fmt().event_format(format);
    if let Some(level) = args.log_level {
        builder
            .with_max_level(match level {
                LogLevel::Error => LevelFilter::ERROR,
                LogLevel::Warn => LevelFilter::WARN,
                LogLevel::Info => LevelFilter::INFO,
                LogLevel::Debug => LevelFilter::DEBUG,
                LogLevel::Trace => LevelFilter::TRACE,
            })
            .init();
    } else {
        builder
            .with_env_filter(
                EnvFilter::builder()
                    .with_default_directive(LevelFilter::INFO.into())
                    .from_env_lossy(),
            )
            .init();
    }

    let mut result = Ok(());
    if let Some(dir) = &args.chdir {
        result = env::set_current_dir(dir).map_err(|e| {
            nod::Error::Io(format!("Failed to change working directory to '{}'", display(dir)), e)
        });
    }
    result = result.and_then(|_| match args.command {
        SubCommand::Info(c_args) => info(c_args),
        SubCommand::Convert(c_args) => convert(c_args),
        SubCommand::Extract(c_args) => extract(c_args),
        SubCommand::Verify(c_args) => verify(c_args),
    });
    if let Err(e) = result {
        eprintln!("Failed: {}", e);
        if let Some(source) = e.source() {
            eprintln!("Caused by: {}", source);
        }
        std::process::exit(1);
    }
}

fn print_header(header: &DiscHeader, meta: &DiscMeta) {
    println!("Format: {}", meta.format);
    if meta.compression != Compression::None {
        println!("Compression: {}", meta.compression);
    }
    if let Some(block_size) = meta.block_size {
        println!("Block size: {}", Size::from_bytes(block_size));
    }
    println!("Lossless: {}", meta.lossless);
    println!(
        "Verification data: {}",
        meta.crc32.is_some()
            || meta.md5.is_some()
            || meta.sha1.is_some()
            || meta.xxhash64.is_some()
    );
    println!();
    println!("Title: {}", header.game_title_str());
    println!("Game ID: {}", header.game_id_str());
    println!("Disc {}, Revision {}", header.disc_num + 1, header.disc_version);
    if header.no_partition_hashes != 0 {
        println!("[!] Disc has no hashes");
    }
    if header.no_partition_encryption != 0 {
        println!("[!] Disc is not encrypted");
    }
}

fn info(args: InfoArgs) -> Result<()> {
    for file in &args.file {
        info_file(file)?;
    }
    Ok(())
}

fn info_file(path: &Path) -> Result<()> {
    log::info!("Loading {}", display(path));
    let disc = Disc::new_with_options(path, &OpenOptions {
        rebuild_encryption: false,
        validate_hashes: false,
    })?;
    let header = disc.header();
    let meta = disc.meta();
    print_header(header, &meta);

    if header.is_wii() {
        for (idx, info) in disc.partitions().iter().enumerate() {
            println!();
            println!("Partition {}", idx);
            println!("\tType: {}", info.kind);
            let offset = info.start_sector as u64 * SECTOR_SIZE as u64;
            println!("\tStart sector: {} (offset {:#X})", info.start_sector, offset);
            let data_size =
                (info.data_end_sector - info.data_start_sector) as u64 * SECTOR_SIZE as u64;
            println!(
                "\tData offset / size: {:#X} / {:#X} ({})",
                info.data_start_sector as u64 * SECTOR_SIZE as u64,
                data_size,
                Size::from_bytes(data_size)
            );
            println!(
                "\tTMD  offset / size: {:#X} / {:#X}",
                offset + info.header.tmd_off(),
                info.header.tmd_size()
            );
            println!(
                "\tCert offset / size: {:#X} / {:#X}",
                offset + info.header.cert_chain_off(),
                info.header.cert_chain_size()
            );
            println!(
                "\tH3   offset / size: {:#X} / {:#X}",
                offset + info.header.h3_table_off(),
                info.header.h3_table_size()
            );

            let mut partition = disc.open_partition(idx)?;
            let meta = partition.meta()?;
            let tmd = meta.tmd_header();
            let title_id_str = if let Some(tmd) = tmd {
                format!(
                    "{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                    tmd.title_id[0],
                    tmd.title_id[1],
                    tmd.title_id[2],
                    tmd.title_id[3],
                    tmd.title_id[4],
                    tmd.title_id[5],
                    tmd.title_id[6],
                    tmd.title_id[7]
                )
            } else {
                "N/A".to_string()
            };
            println!("\tTitle: {}", info.disc_header.game_title_str());
            println!("\tGame ID: {} ({})", info.disc_header.game_id_str(), title_id_str);
            println!(
                "\tDisc {}, Revision {}",
                info.disc_header.disc_num + 1,
                info.disc_header.disc_version
            );
        }
    } else if header.is_gamecube() {
        // TODO
    } else {
        println!(
            "Invalid GC/Wii magic: {:#010X}/{:#010X}",
            header.gcn_magic.get(),
            header.wii_magic.get()
        );
    }
    println!();
    Ok(())
}

fn convert(args: ConvertArgs) -> Result<()> {
    convert_and_verify(&args.file, Some(&args.out), args.md5)
}

fn verify(args: VerifyArgs) -> Result<()> {
    for file in &args.file {
        convert_and_verify(file, None, args.md5)?;
        println!();
    }
    Ok(())
}

fn convert_and_verify(in_file: &Path, out_file: Option<&Path>, md5: bool) -> Result<()> {
    println!("Loading {}", display(in_file));
    let mut disc = Disc::new_with_options(in_file, &OpenOptions {
        rebuild_encryption: true,
        validate_hashes: false,
    })?;
    let header = disc.header();
    let meta = disc.meta();
    print_header(header, &meta);

    let disc_size = disc.disc_size();

    let mut file = if let Some(out_file) = out_file {
        Some(
            File::create(out_file)
                .with_context(|| format!("Creating file {}", display(out_file)))?,
        )
    } else {
        None
    };

    if out_file.is_some() {
        println!("\nConverting...");
    } else {
        println!("\nVerifying...");
    }
    let pb = ProgressBar::new(disc_size);
    pb.set_style(ProgressStyle::with_template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})")
        .unwrap()
        .with_key("eta", |state: &ProgressState, w: &mut dyn fmt::Write| {
            write!(w, "{:.1}s", state.eta().as_secs_f64()).unwrap()
        })
        .progress_chars("#>-"));

    const BUFFER_SIZE: usize = 1015808; // LCM(0x8000, 0x7C00)
    let digest_threads = if md5 {
        vec![
            digest_thread::<crc32fast::Hasher>(),
            digest_thread::<md5::Md5>(),
            digest_thread::<sha1::Sha1>(),
            digest_thread::<xxhash_rust::xxh64::Xxh64>(),
        ]
    } else {
        vec![
            digest_thread::<crc32fast::Hasher>(),
            digest_thread::<sha1::Sha1>(),
            digest_thread::<xxhash_rust::xxh64::Xxh64>(),
        ]
    };

    let (w_tx, w_rx) = sync_channel::<Arc<[u8]>>(1);
    let w_thread = thread::spawn(move || {
        let mut total_written = 0u64;
        while let Ok(data) = w_rx.recv() {
            if let Some(file) = &mut file {
                file.write_all(data.as_ref())
                    .with_context(|| {
                        format!("Writing {} bytes at offset {}", data.len(), total_written)
                    })
                    .unwrap();
            }
            total_written += data.len() as u64;
            pb.set_position(total_written);
        }
        if let Some(mut file) = file {
            file.flush().context("Flushing output file").unwrap();
        }
        pb.finish();
    });

    let mut total_read = 0u64;
    let mut buf = <u8>::new_box_slice_zeroed(BUFFER_SIZE);
    while total_read < disc_size {
        let read = min(BUFFER_SIZE as u64, disc_size - total_read) as usize;
        disc.read_exact(&mut buf[..read]).with_context(|| {
            format!("Reading {} bytes at disc offset {}", BUFFER_SIZE, total_read)
        })?;

        let arc = Arc::<[u8]>::from(&buf[..read]);
        for (tx, _) in &digest_threads {
            tx.send(arc.clone()).map_err(|_| "Sending data to hash thread")?;
        }
        w_tx.send(arc).map_err(|_| "Sending data to write thread")?;
        total_read += read as u64;
    }
    drop(w_tx); // Close channel
    w_thread.join().unwrap();

    println!();
    if let Some(path) = out_file {
        println!("Wrote {} to {}", Size::from_bytes(total_read), display(path));
    }

    println!();
    let mut crc32 = None;
    let mut md5 = None;
    let mut sha1 = None;
    let mut xxh64 = None;
    for (tx, handle) in digest_threads {
        drop(tx); // Close channel
        match handle.join().unwrap() {
            DigestResult::Crc32(v) => crc32 = Some(v),
            DigestResult::Md5(v) => md5 = Some(v),
            DigestResult::Sha1(v) => sha1 = Some(v),
            DigestResult::Xxh64(v) => xxh64 = Some(v),
        }
    }

    let redump_entry = crc32.and_then(redump::find_by_crc32);
    let expected_crc32 = meta.crc32.or(redump_entry.as_ref().map(|e| e.crc32));
    let expected_md5 = meta.md5.or(redump_entry.as_ref().map(|e| e.md5));
    let expected_sha1 = meta.sha1.or(redump_entry.as_ref().map(|e| e.sha1));
    let expected_xxh64 = meta.xxhash64;

    fn print_digest(value: DigestResult, expected: Option<DigestResult>) {
        print!("{:<6}: ", value.name());
        if let Some(expected) = expected {
            if expected != value {
                print!("{} ❌ (expected: {})", value, expected);
            } else {
                print!("{} ✅", value);
            }
        } else {
            print!("{}", value);
        }
        println!();
    }

    if let Some(entry) = &redump_entry {
        let mut full_match = true;
        if let Some(md5) = md5 {
            if entry.md5 != md5 {
                full_match = false;
            }
        }
        if let Some(sha1) = sha1 {
            if entry.sha1 != sha1 {
                full_match = false;
            }
        }
        if full_match {
            println!("Redump: {} ✅", entry.name);
        } else {
            println!("Redump: {} ❓ (partial match)", entry.name);
        }
    } else {
        println!("Redump: Not found ❌");
    }
    if let Some(crc32) = crc32 {
        print_digest(DigestResult::Crc32(crc32), expected_crc32.map(DigestResult::Crc32));
    }
    if let Some(md5) = md5 {
        print_digest(DigestResult::Md5(md5), expected_md5.map(DigestResult::Md5));
    }
    if let Some(sha1) = sha1 {
        print_digest(DigestResult::Sha1(sha1), expected_sha1.map(DigestResult::Sha1));
    }
    if let Some(xxh64) = xxh64 {
        print_digest(DigestResult::Xxh64(xxh64), expected_xxh64.map(DigestResult::Xxh64));
    }
    Ok(())
}

pub fn has_extension(filename: &Path, extension: &str) -> bool {
    match filename.extension() {
        Some(ext) => ext.eq_ignore_ascii_case(extension),
        None => false,
    }
}

fn extract(args: ExtractArgs) -> Result<()> {
    let output_dir: PathBuf;
    if let Some(dir) = args.out {
        output_dir = dir;
    } else if has_extension(&args.file, "nfs") {
        // Special logic to extract from content/hif_*.nfs to extracted/..
        if let Some(parent) = args.file.parent() {
            output_dir = parent.with_file_name("extracted");
        } else {
            output_dir = args.file.with_extension("");
        }
    } else {
        output_dir = args.file.with_extension("");
    }
    let disc = Disc::new_with_options(&args.file, &OpenOptions {
        rebuild_encryption: false,
        validate_hashes: args.validate,
    })?;
    let header = disc.header();
    let is_wii = header.is_wii();
    if let Some(partition) = args.partition {
        if partition.eq_ignore_ascii_case("all") {
            for info in disc.partitions() {
                let mut out_dir = output_dir.clone();
                out_dir.push(info.kind.dir_name().as_ref());
                let mut partition = disc.open_partition(info.index)?;
                extract_partition(header, partition.as_mut(), &out_dir, is_wii, args.quiet)?;
            }
        } else if partition.eq_ignore_ascii_case("data") {
            let mut partition = disc.open_partition_kind(PartitionKind::Data)?;
            extract_partition(header, partition.as_mut(), &output_dir, is_wii, args.quiet)?;
        } else if partition.eq_ignore_ascii_case("update") {
            let mut partition = disc.open_partition_kind(PartitionKind::Update)?;
            extract_partition(header, partition.as_mut(), &output_dir, is_wii, args.quiet)?;
        } else if partition.eq_ignore_ascii_case("channel") {
            let mut partition = disc.open_partition_kind(PartitionKind::Channel)?;
            extract_partition(header, partition.as_mut(), &output_dir, is_wii, args.quiet)?;
        } else {
            let idx = partition.parse::<usize>().map_err(|_| "Invalid partition index")?;
            let mut partition = disc.open_partition(idx)?;
            extract_partition(header, partition.as_mut(), &output_dir, is_wii, args.quiet)?;
        }
    } else {
        let mut partition = disc.open_partition_kind(PartitionKind::Data)?;
        extract_partition(header, partition.as_mut(), &output_dir, is_wii, args.quiet)?;
    }
    Ok(())
}

fn extract_partition(
    header: &DiscHeader,
    partition: &mut dyn PartitionBase,
    out_dir: &Path,
    is_wii: bool,
    quiet: bool,
) -> Result<()> {
    let meta = partition.meta()?;
    extract_sys_files(header, meta.as_ref(), out_dir, quiet)?;

    // Extract FST
    let files_dir = out_dir.join("files");
    fs::create_dir_all(&files_dir)
        .with_context(|| format!("Creating directory {}", display(&files_dir)))?;

    let fst = Fst::new(&meta.raw_fst)?;
    let mut path_segments = Vec::<(Cow<str>, usize)>::new();
    for (idx, node, name) in fst.iter() {
        // Remove ended path segments
        let mut new_size = 0;
        for (_, end) in path_segments.iter() {
            if *end == idx {
                break;
            }
            new_size += 1;
        }
        path_segments.truncate(new_size);

        // Add the new path segment
        let end = if node.is_dir() { node.length() as usize } else { idx + 1 };
        path_segments.push((name?, end));

        let path = path_segments.iter().map(|(name, _)| name.as_ref()).join("/");
        if node.is_dir() {
            fs::create_dir_all(files_dir.join(&path))
                .with_context(|| format!("Creating directory {}", path))?;
        } else {
            extract_node(node, partition, &files_dir, &path, is_wii, quiet)?;
        }
    }
    Ok(())
}

fn extract_sys_files(
    header: &DiscHeader,
    data: &PartitionMeta,
    out_dir: &Path,
    quiet: bool,
) -> Result<()> {
    let sys_dir = out_dir.join("sys");
    fs::create_dir_all(&sys_dir)
        .with_context(|| format!("Creating directory {}", display(&sys_dir)))?;
    extract_file(data.raw_boot.as_ref(), &sys_dir.join("boot.bin"), quiet)?;
    extract_file(data.raw_bi2.as_ref(), &sys_dir.join("bi2.bin"), quiet)?;
    extract_file(data.raw_apploader.as_ref(), &sys_dir.join("apploader.img"), quiet)?;
    extract_file(data.raw_fst.as_ref(), &sys_dir.join("fst.bin"), quiet)?;
    extract_file(data.raw_dol.as_ref(), &sys_dir.join("main.dol"), quiet)?;

    // Wii files
    if header.is_wii() {
        let disc_dir = out_dir.join("disc");
        fs::create_dir_all(&disc_dir)
            .with_context(|| format!("Creating directory {}", display(&disc_dir)))?;
        extract_file(&header.as_bytes()[..0x100], &disc_dir.join("header.bin"), quiet)?;
    }
    if let Some(ticket) = data.raw_ticket.as_deref() {
        extract_file(ticket, &out_dir.join("ticket.bin"), quiet)?;
    }
    if let Some(tmd) = data.raw_tmd.as_deref() {
        extract_file(tmd, &out_dir.join("tmd.bin"), quiet)?;
    }
    if let Some(cert_chain) = data.raw_cert_chain.as_deref() {
        extract_file(cert_chain, &out_dir.join("cert.bin"), quiet)?;
    }
    if let Some(h3_table) = data.raw_h3_table.as_deref() {
        extract_file(h3_table, &out_dir.join("h3.bin"), quiet)?;
    }
    Ok(())
}

fn extract_file(bytes: &[u8], out_path: &Path, quiet: bool) -> Result<()> {
    if !quiet {
        println!(
            "Extracting {} (size: {})",
            display(out_path),
            Size::from_bytes(bytes.len()).format().with_base(Base::Base10)
        );
    }
    fs::write(out_path, bytes).with_context(|| format!("Writing file {}", display(out_path)))?;
    Ok(())
}

fn extract_node(
    node: &Node,
    partition: &mut dyn PartitionBase,
    base_path: &Path,
    name: &str,
    is_wii: bool,
    quiet: bool,
) -> Result<()> {
    let file_path = base_path.join(name);
    if !quiet {
        println!(
            "Extracting {} (size: {})",
            display(&file_path),
            Size::from_bytes(node.length()).format().with_base(Base::Base10)
        );
    }
    let file = File::create(&file_path)
        .with_context(|| format!("Creating file {}", display(&file_path)))?;
    let mut w = BufWriter::with_capacity(partition.ideal_buffer_size(), file);
    let mut r = partition.open_file(node).with_context(|| {
        format!(
            "Opening file {} on disc for reading (offset {}, size {})",
            name,
            node.offset(is_wii),
            node.length()
        )
    })?;
    io::copy(&mut r, &mut w).with_context(|| format!("Extracting file {}", display(&file_path)))?;
    w.flush().with_context(|| format!("Flushing file {}", display(&file_path)))?;
    Ok(())
}

fn display(path: &Path) -> PathDisplay { PathDisplay { path } }

struct PathDisplay<'a> {
    path: &'a Path,
}

impl<'a> fmt::Display for PathDisplay<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use fmt::Write;
        let mut first = true;
        for segment in self.path.iter() {
            let segment_str = segment.to_string_lossy();
            if segment_str == "." {
                continue;
            }
            if first {
                first = false;
            } else {
                f.write_char(MAIN_SEPARATOR)?;
            }
            f.write_str(&segment_str)?;
        }
        Ok(())
    }
}
