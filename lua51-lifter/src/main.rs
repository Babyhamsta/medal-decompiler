use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    time::Instant,
};

use clap::Parser;

#[derive(Parser, Debug)]
#[clap(about, version, author)]
struct Args {
    #[clap(short, long)]
    file: PathBuf,
}

fn main() -> anyhow::Result<()> {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();

    let args = Args::parse();
    let start = Instant::now();
    let bytecode = std::fs::read(&args.file)?;
    let source = lua51_lifter::try_decompile_bytecode(&bytecode)?;

    let output = output_path(&args.file)?;
    let mut file = File::create(output)?;
    writeln!(
        file,
        "-- Decompiled by Devirt.me (took {:?})",
        start.elapsed()
    )?;
    writeln!(file, "{source}")?;
    Ok(())
}

fn output_path(input: &Path) -> anyhow::Result<PathBuf> {
    input
        .with_extension("dec.51.lua")
        .file_name()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("input path has no file name"))
}
