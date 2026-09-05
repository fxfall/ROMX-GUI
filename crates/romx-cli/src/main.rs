use clap::{Args, Parser, Subcommand};
use romx_core::{
    export_lpl, extract_to_dir, import_lpl, inspect_payload_profile,
    launch_format_id_for_extension, pack_entries_to_path_with_writer_options,
    pack_to_path_with_options, platform_id_for_name, read_metadata_cover_path, validate_path,
    ExportLplOptions, ImportLplOptions, PackEntry, PackOptions,
};
use serde_json::json;
use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "romx",
    version = romx_core::APP_VERSION,
    about = "Pack, inspect, verify, extract, import, and export ROMX files"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a ROMX container from a ROM, metadata JSON, and optional cover image.
    Pack(PackArgs),
    /// Create a ROMX container from a descriptor and one or more sidecar files.
    PackSet(PackSetArgs),
    /// Print validated ROMX footer and metadata information as JSON.
    Inspect { romx: PathBuf },
    /// Validate the ROMX structure and report component statuses as JSON.
    Validate { romx: PathBuf },
    /// Validate a ROMX container, including regions, optional metadata/cover, and body SHA-256.
    Verify { romx: PathBuf },
    /// Read platform, embedded metadata, and artwork from an original ROM payload without packing it.
    Probe { payload: PathBuf },
    /// Extract the payload, metadata, and optional cover from a ROMX container.
    Extract(ExtractArgs),
    /// Import a RetroArch LPL into sequential ROMX files.
    ImportLpl(ImportLplArgs),
    /// Export a ROMX directory to ROMs, thumbnails, and a RetroArch LPL.
    ExportLpl(ExportLplArgs),
}

#[derive(Debug, Args)]
struct PackArgs {
    /// Original ROM payload.
    rom: PathBuf,
    /// ROMX metadata JSON document.
    metadata: PathBuf,
    /// Output ROMX path.
    #[arg(short, long)]
    output: PathBuf,
    /// Optional PNG cover; common image formats are normalized by the CLI adapter.
    #[arg(long)]
    cover: Option<PathBuf>,
    /// Normalize the cover to an exact WIDTHxHEIGHT PNG.
    #[arg(long)]
    cover_size: Option<String>,
    /// Override the metadata CRC32 lookup key (8 hexadecimal characters).
    #[arg(long)]
    crc32: Option<String>,
}

#[derive(Debug, Args)]
struct PackSetArgs {
    /// Virtual path and source file, repeated as VPATH=SOURCE.
    #[arg(long = "entry", required = true)]
    entries: Vec<String>,
    /// Virtual path of the launch descriptor. Required when more than one
    /// entry is supplied.
    #[arg(long)]
    entrypoint: Option<String>,
    /// ROMX platform registry name (for example saturn, playstation, gamecube).
    #[arg(long)]
    platform: String,
    /// Launch descriptor registry name (raw, cue, gdi, m3u, ccd, mds, toc).
    #[arg(long)]
    launch_format: String,
    /// ROMX metadata JSON document.
    #[arg(long)]
    metadata: Option<PathBuf>,
    /// Output ROMX path.
    #[arg(short, long)]
    output: PathBuf,
    /// Optional PNG cover; common image formats are normalized by the CLI adapter.
    #[arg(long)]
    cover: Option<PathBuf>,
    /// Normalize the cover to an exact WIDTHxHEIGHT PNG.
    #[arg(long)]
    cover_size: Option<String>,
    /// Override the metadata CRC32 lookup key (8 hexadecimal characters).
    #[arg(long)]
    crc32: Option<String>,
    /// Include immutable body SHA-256 in the footer.
    #[arg(long)]
    body_sha256: bool,
    /// Include CRC32 values for every RIDX entry.
    #[arg(long, default_value_t = true)]
    entry_crc32: bool,
    /// Reserve a mutable region of this many bytes.
    #[arg(long, default_value_t = 0)]
    mutable_capacity: u64,
    /// Reserve this many mutable directory entries.
    #[arg(long, default_value_t = 0)]
    mutable_entry_capacity: u32,
}

#[derive(Debug, Args)]
struct ExtractArgs {
    /// ROMX file to extract.
    romx: PathBuf,
    /// Output directory.
    #[arg(short, long)]
    output: PathBuf,
}

#[derive(Debug, Args)]
struct ImportLplArgs {
    /// RetroArch playlist to import.
    lpl: PathBuf,
    /// Directory for sequential ROMX files and manifest.json.
    #[arg(short, long)]
    output: PathBuf,
    /// Root used to map virtual paths such as /roms/00-GB/1.gb.
    #[arg(long)]
    rom_root: Option<PathBuf>,
    /// RetroArch thumbnails root (PNG/JPEG/WebP/GIF/BMP are accepted).
    #[arg(long)]
    cover_root: Option<PathBuf>,
    /// Ignore the LPL directory and find ROMs by basename in this directory.
    #[arg(long)]
    rom_dir: Option<PathBuf>,
    /// Ignore the thumbnail tree and find PNGs by basename in this directory.
    #[arg(long)]
    cover_dir: Option<PathBuf>,
    /// Thumbnail set directory beneath each playlist.
    #[arg(long, default_value = "Named_Snaps")]
    cover_set: String,
    /// Skip entries whose ROM file is missing.
    #[arg(long)]
    skip_missing: bool,
    /// Override the metadata CRC32 lookup key for every imported ROM.
    #[arg(long)]
    crc32: Option<String>,
    /// Normalize imported covers to an exact WIDTHxHEIGHT PNG.
    #[arg(long)]
    cover_size: Option<String>,
}

#[derive(Debug, Args)]
struct ExportLplArgs {
    /// Directory containing ROMX files.
    romx_dir: PathBuf,
    /// Root for the generated playlists, ROMs, and thumbnails directories.
    #[arg(short, long)]
    output: PathBuf,
    /// Override the playlist name.
    #[arg(long)]
    playlist_name: Option<String>,
    /// Write the playlist to this exact path.
    #[arg(long)]
    lpl_path: Option<PathBuf>,
    /// Write extracted ROMs to this exact directory.
    #[arg(long)]
    rom_dir: Option<PathBuf>,
    /// Write extracted PNG covers to this exact directory.
    #[arg(long)]
    cover_dir: Option<PathBuf>,
    /// Write extracted logical SAVE objects to this exact directory.
    #[arg(long)]
    save_dir: Option<PathBuf>,
    /// Virtual ROM path prefix stored in exported LPL entries.
    #[arg(long)]
    lpl_rom_prefix: Option<String>,
    /// Thumbnail set directory beneath the playlist.
    #[arg(long, default_value = "Named_Snaps")]
    cover_set: String,
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn parse_cover_size(value: Option<&str>) -> Result<Option<(u32, u32)>, Box<dyn Error>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let (width, height) = value
        .split_once(['x', 'X'])
        .ok_or("cover size must use WIDTHxHEIGHT")?;
    let width = width.parse::<u32>()?;
    let height = height.parse::<u32>()?;
    if width == 0 || height == 0 || width > 8192 || height > 8192 {
        return Err("cover size must be between 1x1 and 8192x8192".into());
    }
    Ok(Some((width, height)))
}

fn parse_entry_assignment(value: &str) -> Result<PackEntry, Box<dyn Error>> {
    let (path, source) = value
        .split_once('=')
        .ok_or("--entry must use VPATH=SOURCE")?;
    if path.is_empty() || source.is_empty() {
        return Err("--entry must use non-empty VPATH=SOURCE".into());
    }
    Ok(PackEntry {
        path: path.to_owned(),
        source: PathBuf::from(source),
        format_id: 0,
        entrypoint: false,
    })
}

fn parse_launch_format(value: &str) -> Result<u16, Box<dyn Error>> {
    let normalized = value.trim_start_matches('.').to_ascii_lowercase();
    let id = match normalized.as_str() {
        "raw" | "single" | "raw_single_file" => 1,
        extension => launch_format_id_for_extension(extension),
    };
    if id == 1 && !matches!(normalized.as_str(), "raw" | "single" | "raw_single_file") {
        return Err(format!("unsupported launch format: {value}").into());
    }
    Ok(id)
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    match cli.command {
        Command::Pack(args) => {
            let cover_size = parse_cover_size(args.cover_size.as_deref())?;
            pack_to_path_with_options(
                &args.rom,
                Some(&args.metadata),
                args.cover.as_deref(),
                &args.output,
                args.crc32.as_deref(),
                cover_size,
            )?;
            println!("packed ROMX: {}", args.output.display());
        }
        Command::PackSet(args) => {
            let mut entries = args
                .entries
                .iter()
                .map(|value| parse_entry_assignment(value))
                .collect::<Result<Vec<_>, _>>()?;
            let entrypoint = args
                .entrypoint
                .or_else(|| (entries.len() == 1).then(|| entries[0].path.clone()));
            let Some(entrypoint) = entrypoint else {
                return Err("--entrypoint is required when packing multiple entries".into());
            };
            for entry in &mut entries {
                entry.entrypoint = entry.path == entrypoint;
            }
            if !entries.iter().any(|entry| entry.entrypoint) {
                return Err(
                    format!("entrypoint is not present in --entry list: {entrypoint}").into(),
                );
            }
            let platform_id = platform_id_for_name(&args.platform);
            if platform_id == 0 {
                return Err(format!("unknown ROMX platform: {}", args.platform).into());
            }
            let launch_format_id = parse_launch_format(&args.launch_format)?;
            let cover_target = parse_cover_size(args.cover_size.as_deref())?;
            let options = PackOptions {
                body_sha256: args.body_sha256,
                crc32_override: args.crc32,
                cover_target,
                platform_id,
                launch_format_id,
                entry_format_id: 0,
                include_entry_crc32: args.entry_crc32,
                mutable_capacity: args.mutable_capacity,
                mutable_entry_capacity: args.mutable_entry_capacity,
                ..Default::default()
            };
            pack_entries_to_path_with_writer_options(
                &entries,
                Some(&entrypoint),
                args.metadata.as_deref(),
                args.cover.as_deref(),
                &args.output,
                &options,
            )?;
            println!("packed ROMX set: {}", args.output.display());
        }
        Command::Inspect { romx } => {
            let preview = read_metadata_cover_path(&romx)?;
            let footer = &preview.footer;
            let value = json!({
                "path": romx,
                "spec_version": romx_core::SPEC_VERSION,
                "version": footer.version,
                "rom_offset": footer.rom.offset,
                "rom_size": footer.rom.size,
                "metadata_offset": footer.metadata.offset,
                "metadata_size": footer.metadata.size,
                "cover_offset": footer.cover.offset,
                "cover_size": footer.cover.size,
                "mutable_capacity": footer.mutable_capacity,
                "flags": footer.flags,
                "reserved": hex(&footer.reserved),
                "body_sha256": hex(&footer.body_sha256),
                "metadata": preview.metadata,
                "has_cover": preview.cover.is_some(),
                "entries": preview.entries.iter().map(|entry| json!({
                    "path": entry.path,
                    "format_id": entry.format_id,
                    "data_offset": entry.data_offset,
                    "data_size": entry.data_size,
                    "crc32": entry.crc32,
                    "entrypoint": entry.entrypoint,
                })).collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        Command::Validate { romx } => {
            let report = validate_path(&romx)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "path": romx,
                    "structure": format!("{:?}", report.structure),
                    "payload_hashes": format!("{:?}", report.payload_hashes),
                    "body_sha256": format!("{:?}", report.body_sha256),
                    "metadata": format!("{:?}", report.metadata),
                    "cover": format!("{:?}", report.cover),
                    "metadata_crc32": format!("{:?}", report.metadata_crc32),
                    "computed_payload_crc32": report.computed_payload_crc32,
                    "metadata_result": report.metadata_result,
                    "cover_result": report.cover_result,
                }))?
            );
        }
        Command::Verify { romx } => {
            validate_path(&romx)?;
            println!("valid ROMX: {}", romx.display());
        }
        Command::Probe { payload } => {
            let profile = inspect_payload_profile(&payload)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "path": payload,
                    "payload_format": profile.payload_format,
                    "platform": profile.platform,
                    "metadata": profile.metadata,
                    "embedded_cover_count": profile.covers.len(),
                }))?
            );
        }
        Command::Extract(args) => {
            let payload = extract_to_dir(&args.romx, &args.output)?;
            println!("extracted payload: {}", payload.display());
        }
        Command::ImportLpl(args) => {
            let options = ImportLplOptions {
                rom_root: args.rom_root,
                cover_root: args.cover_root,
                force_rom_dir: args.rom_dir,
                force_cover_dir: args.cover_dir,
                cover_set: args.cover_set,
                skip_missing: args.skip_missing,
                crc32_override: args.crc32,
                temporary_output: false,
                include_identity: true,
                write_manifest: true,
                cover_target: parse_cover_size(args.cover_size.as_deref())?,
                mutable_capacity: 0,
                mutable_entry_capacity: 0,
                mutable_save_bundles: Vec::new(),
                mutable_region: None,
                mutable_region_source: None,
            };
            let report = import_lpl(&args.lpl, &args.output, &options)?;
            println!(
                "imported {} of {} LPL items into {} (skipped {})",
                report.imported,
                report.total_items,
                args.output.display(),
                report.skipped
            );
        }
        Command::ExportLpl(args) => {
            let options = ExportLplOptions {
                playlist_name: args.playlist_name,
                lpl_path: args.lpl_path,
                rom_dir: args.rom_dir,
                cover_dir: args.cover_dir,
                save_dir: args.save_dir,
                lpl_rom_prefix: args.lpl_rom_prefix,
                lpl_cover_prefix: None,
                cover_set: args.cover_set,
                temporary_output: false,
            };
            let report = export_lpl(&args.romx_dir, &args.output, &options)?;
            println!(
                "exported {} items to {}",
                report.exported,
                report.lpl_path.display()
            );
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
