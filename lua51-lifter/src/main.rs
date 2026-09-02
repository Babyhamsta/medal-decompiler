use std::{
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use clap::Parser;

#[derive(Parser, Debug)]
#[clap(about, version, author)]
struct Args {
    #[clap(short, long)]
    file: PathBuf,
    /// Print disassembly instead of decompiled source.
    #[clap(long)]
    disasm: bool,
    /// With --disasm, dump only this prototype; may be repeated.
    #[clap(long, requires = "disasm")]
    proto: Vec<usize>,
    /// With --disasm, print one summary line per prototype.
    #[clap(
        long,
        requires = "disasm",
        conflicts_with_all = ["proto", "locals"]
    )]
    list: bool,
    /// With --disasm, annotate instructions with live debug locals.
    #[clap(long, requires = "disasm")]
    locals: bool,
}

fn main() -> anyhow::Result<()> {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();

    let args = Args::parse();
    let start = Instant::now();
    let bytecode = std::fs::read(&args.file)?;

    if args.disasm {
        let output = if args.list {
            lua51_lifter::list_prototypes(&bytecode)
        } else {
            let selection = if args.proto.is_empty() {
                lua51_lifter::ProtoSelection::All
            } else {
                lua51_lifter::ProtoSelection::Only(args.proto)
            };
            lua51_lifter::disassemble(&bytecode, &selection, args.locals)
        }
        .map_err(anyhow::Error::new)?;
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        stdout.write_all(output.as_bytes())?;
        stdout.flush()?;
        return Ok(());
    }

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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::Args;

    #[test]
    fn disassembly_only_options_require_disassembly_mode() {
        assert!(
            Args::try_parse_from(["lua51-lifter", "--file", "sample.luac", "--proto", "1"])
                .is_err()
        );
        assert!(
            Args::try_parse_from(["lua51-lifter", "--file", "sample.luac", "--locals"]).is_err()
        );
        assert!(
            Args::try_parse_from([
                "lua51-lifter",
                "--file",
                "sample.luac",
                "--disasm",
                "--proto",
                "1",
                "--locals",
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "lua51-lifter",
                "--file",
                "sample.luac",
                "--disasm",
                "--list",
                "--proto",
                "1",
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from([
                "lua51-lifter",
                "--file",
                "sample.luac",
                "--disasm",
                "--list",
                "--locals",
            ])
            .is_err()
        );
    }
}
