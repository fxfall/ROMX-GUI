use clap::{Args, Parser, Subcommand};
use romx_core::{
    export_lpl, extract_to_dir, import_lpl, pack_to_path_with_crc32, read_path, ExportLplOptions,
    ImportLplOptions,
};
use serde_json::json;
use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "romx",
    version,
    about = "Pack, inspect, verify, extract, import, and export ROMX files"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a ROMX container from a ROM, metadata JSON, and optional PNG cover.
    Pack(PackArgs),
    /// Print validated ROMX footer and metadata information as JSON.
    Inspect { romx: PathBuf },
    /// Validate a ROMX container, including regions and SHA-256 hashes.
    Verify { romx: PathBuf },
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
    /// Optional PNG cover.
    #[arg(long)]
    cover: Option<PathBuf>,
    /// Override the metadata CRC32 lookup key (8 hexadecimal characters).
    #[arg(long)]
    crc32: Option<String>,
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
    /// RetroArch thumbnails root.
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

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    match cli.command {
        Command::Pack(args) => {
            pack_to_path_with_crc32(
                &args.rom,
                Some(&args.metadata),
                args.cover.as_deref(),
                &args.output,
                args.crc32.as_deref(),
            )?;
            println!("packed ROMX: {}", args.output.display());
        }
        Command::Inspect { romx } => {
            let document = read_path(&romx)?;
            let footer = &document.footer;
            let value = json!({
                "path": romx,
                "version": footer.version,
                "rom_offset": footer.rom.offset,
                "rom_size": footer.rom.size,
                "metadata_offset": footer.metadata.offset,
                "metadata_size": footer.metadata.size,
                "cover_offset": footer.cover.offset,
                "cover_size": footer.cover.size,
                "flags": footer.flags,
                "rom_sha256": hex(&footer.rom_sha256),
                "body_sha256": hex(&footer.body_sha256),
                "metadata": document.metadata,
                "has_cover": document.cover.is_some(),
            });
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        Command::Verify { romx } => {
            read_path(&romx)?;
            println!("valid ROMX: {}", romx.display());
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
                lpl_rom_prefix: args.lpl_rom_prefix,
                cover_set: args.cover_set,
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
