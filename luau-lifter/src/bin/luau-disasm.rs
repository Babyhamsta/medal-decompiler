//! Prints deserialized Luau bytecode as text. Inspection only: this binary
//! shares the crate's deserializer with the decompiler but never lifts.

use luau_lifter::ProtoSelection;

const USAGE: &str = "\
usage: luau-disasm <file.luac> [options]

  --key <n>        instruction encode key (default 1; -e selects 203)
  -e               shorthand for --key 203
  --proto <n>      dump only this prototype; may be repeated
  --list           one summary line per prototype, no instructions
  --audit-calls    pair every CALL with the preceding value-producing
                   instruction and flag register mismatches
  --audit-all      like --audit-calls but also prints matching pairs
  --locals         annotate instructions with live debug locals
";

fn main() {
    let mut arguments = std::env::args().skip(1);
    let mut file_name: Option<String> = None;
    let mut key: u8 = 1;
    let mut protos: Vec<usize> = Vec::new();
    let mut list = false;
    let mut audit = false;
    let mut audit_all = false;
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
            "--proto" => {
                let value = arguments
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_else(|| fail("--proto expects an index"));
                protos.push(value);
            }
            "--list" => list = true,
            "--audit-calls" => audit = true,
            "--audit-all" => {
                audit = true;
                audit_all = true;
            }
            "--locals" => locals = true,
            other if file_name.is_none() => file_name = Some(other.to_owned()),
            other => fail(&format!("unexpected argument {other}")),
        }
    }

    let Some(file_name) = file_name else {
        fail(USAGE);
    };
    let bytecode = std::fs::read(&file_name)
        .unwrap_or_else(|error| fail(&format!("unable to read {file_name}: {error}")));

    let result = if list {
        luau_lifter::list_prototypes(&bytecode, key)
    } else if audit {
        luau_lifter::audit_calls(&bytecode, key, !audit_all)
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
}

fn fail(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(2);
}
