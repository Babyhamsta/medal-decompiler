use luau_lifter::ProtoSelection;

const USAGE: &str = "\
usage: luau-lifter <file.luac> [options]

  --key <n>        instruction encode key (default 1; -e selects 203)
  -e               shorthand for --key 203
  --disasm         print the disassembly instead of decompiled source
  --proto <n>      with --disasm, dump only this prototype; may be repeated
  --list           with --disasm, one summary line per prototype
  --locals         with --disasm, annotate instructions with live debug locals
";

fn main() {
    let mut arguments = std::env::args().skip(1);
    let mut file_name: Option<String> = None;
    let mut key: u8 = 1;
    let mut disasm = false;
    let mut protos: Vec<usize> = Vec::new();
    let mut list = false;
    let mut locals = false;

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return;
            }
            "-e" => key = 203,
            "--key" => {
                key = arguments
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_else(|| fail("--key expects a byte"));
            }
            "--disasm" => disasm = true,
            "--proto" => {
                let value = arguments
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_else(|| fail("--proto expects an index"));
                protos.push(value);
            }
            "--list" => list = true,
            "--locals" => locals = true,
            other if file_name.is_none() => file_name = Some(other.to_owned()),
            other => fail(&format!("unexpected argument {other}")),
        }
    }

    let Some(file_name) = file_name else {
        fail(USAGE);
    };
    if !disasm && (list || locals || !protos.is_empty()) {
        fail("--proto, --list, and --locals require --disasm");
    }
    let bytecode = std::fs::read(&file_name)
        .unwrap_or_else(|error| fail(&format!("unable to read {file_name}: {error}")));

    if disasm {
        let result = if list {
            luau_lifter::list_prototypes(&bytecode, key)
        } else {
            let selection = if protos.is_empty() {
                ProtoSelection::All
            } else {
                ProtoSelection::Only(protos)
            };
            luau_lifter::disassemble(&bytecode, key, &selection, locals)
        };
        match result {
            Ok(text) => print!("{text}"),
            Err(error) => {
                eprintln!("disassembler error: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    let result = luau_lifter::try_decompile_bytecode(&bytecode, key);
    luau_lifter::report_profile();
    match result {
        Ok(output) => println!("{output}"),
        Err(error) => {
            eprintln!("decompiler error: {error}");
            std::process::exit(1);
        }
    }
}

fn fail(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(2);
}
