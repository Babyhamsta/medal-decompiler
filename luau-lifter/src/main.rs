fn main() {
    let file_name = std::env::args().nth(1).expect("expected exactly one file");
    let key = std::env::args()
        .nth(2)
        .or_else(|| None)
        .map(|s| if s == "-e" { 203 } else { panic!() })
        .unwrap_or(1);
    let bytecode = std::fs::read(file_name).expect("failed to read file");
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
