use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

struct VersionProfile {
    name: &'static str,
    version: u8,
    flags: &'static str,
}

const PROFILES: &[VersionProfile] = &[
    VersionProfile {
        name: "v9",
        version: 9,
        flags: "LuauBytecodeCostModel=false,LuauEmitCallFeedback=false,DebugLuauUserDefinedClasses=false",
    },
    VersionProfile {
        name: "v10",
        version: 10,
        flags: "LuauBytecodeCostModel=false,LuauEmitCallFeedback=false,DebugLuauUserDefinedClasses=true",
    },
    VersionProfile {
        name: "v11",
        version: 11,
        flags: "LuauBytecodeCostModel=false,LuauEmitCallFeedback=true,DebugLuauUserDefinedClasses=false",
    },
    VersionProfile {
        name: "v12",
        version: 12,
        flags: "LuauBytecodeCostModel=true",
    },
];

const CASES: &[&str] = &[
    "01_literals_locals",
    "06_method_chains",
    "10_short_circuit",
    "16_closure_capture",
    "21_state_machine",
    "24_wonky_integration",
];

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn compiler() -> PathBuf {
    workspace().join(".tools/luau-windows/luau-compile.exe")
}

fn source(case: &str) -> PathBuf {
    workspace()
        .join("tests/luau_corpus/cases")
        .join(format!("{case}.luau"))
}

fn version_source(case: &str) -> PathBuf {
    workspace()
        .join("tests/luau_corpus/version_cases")
        .join(format!("{case}.luau"))
}

fn compile(mode: &str, profile: &VersionProfile, source: &Path) -> std::process::Output {
    Command::new(compiler())
        .current_dir(workspace())
        .arg(format!("--{mode}"))
        .arg("-O1")
        .arg("-g1")
        .arg(format!("--fflags={}", profile.flags))
        .arg(source)
        .output()
        .unwrap()
}

fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn trivial_local_alias_lines(source: &str) -> Vec<&str> {
    source
        .lines()
        .filter(|line| {
            let Some(rest) = line.trim().strip_prefix("local ") else {
                return false;
            };
            let Some((left, right)) = rest.split_once(" = ") else {
                return false;
            };
            is_identifier(left) && is_identifier(right)
        })
        .collect()
}

#[test]
fn wonky_v12_output_has_no_trivial_local_aliases() {
    if !compiler().is_file() {
        eprintln!("skipping: bundled Luau compiler is absent");
        return;
    }

    let compiled = compile("binary", &PROFILES[3], &source("24_wonky_integration"));
    assert!(compiled.status.success());
    let decompiled = crate::try_decompile_bytecode(&compiled.stdout, 1).unwrap();
    let aliases = trivial_local_alias_lines(&decompiled);
    let module_local = decompiled
        .lines()
        .find_map(|line| {
            let rest = line.strip_prefix("local ")?;
            let (identifier, _) = rest.split_once(" = ")?;
            is_identifier(identifier).then_some(identifier)
        })
        .expect("decompiled output should declare the module local");
    let setmetatable_body = decompiled
        .split_once("return setmetatable({")
        .map(|(_, body)| body)
        .expect("decompiled output should return the setmetatable call");
    let direct_module_argument = format!("}}, {module_local})");

    assert!(
        aliases.is_empty(),
        "trivial aliases remained: {aliases:#?}\n{decompiled}"
    );
    assert!(
        setmetatable_body
            .lines()
            .any(|line| line.trim() == direct_module_argument),
        "setmetatable did not receive module local {module_local} directly\n{decompiled}"
    );
}

#[test]
fn product_controller_recovers_methods_and_keeps_callback_assignment() {
    if !compiler().is_file() {
        eprintln!("skipping: bundled Luau compiler is absent");
        return;
    }

    let profile = &PROFILES[3];
    let compiled = compile("binary", profile, &source("25_product_controller"));
    assert!(compiled.status.success());
    let decompiled = crate::try_decompile_bytecode(&compiled.stdout, 1).unwrap();

    assert!(
        decompiled
            .lines()
            .any(|line| line.starts_with("function ") && line.contains(":dispatch(")),
        "{decompiled}"
    );
    assert!(
        decompiled
            .lines()
            .any(|line| line.starts_with("function ") && line.contains(":use(")),
        "{decompiled}"
    );
    assert!(
        decompiled
            .lines()
            .any(|line| line.trim().contains(".handlers.health = function(")),
        "{decompiled}"
    );
    assert!(!decompiled.contains("handlers:health("), "{decompiled}");

    let output = workspace().join("target/compatibility-tests/product-controller.luau");
    fs::create_dir_all(output.parent().unwrap()).unwrap();
    fs::write(&output, decompiled).unwrap();
    let recompiled = compile("null", profile, &output);
    assert!(
        recompiled.status.success(),
        "{}",
        String::from_utf8_lossy(&recompiled.stderr)
    );
}

#[test]
fn bundled_compiler_versions_decompile_and_recompile() {
    if !compiler().is_file() {
        eprintln!("skipping: bundled Luau compiler is absent");
        return;
    }

    let output_root = workspace()
        .join("target/compatibility-tests")
        .join(std::process::id().to_string());
    fs::create_dir_all(&output_root).unwrap();

    for profile in PROFILES {
        for case in CASES {
            let compiled = compile("binary", profile, &source(case));
            assert!(
                compiled.status.success(),
                "{} {case} compilation failed: {}",
                profile.name,
                String::from_utf8_lossy(&compiled.stderr)
            );
            assert_eq!(compiled.stdout.first().copied(), Some(profile.version));

            let decompiled = crate::try_decompile_bytecode(&compiled.stdout, 1)
                .unwrap_or_else(|error| panic!("{} {case}: {error}", profile.name));
            assert!(
                !decompiled.contains("goto ") && !decompiled.contains("::"),
                "{} {case} produced unsupported Luau jumps",
                profile.name
            );
            let decompiled_path = output_root.join(format!("{}-{case}.luau", profile.name));
            fs::write(&decompiled_path, decompiled).unwrap();

            let recompiled = compile("null", profile, &decompiled_path);
            assert!(
                recompiled.status.success(),
                "{} {case} recompile failed: {}",
                profile.name,
                String::from_utf8_lossy(&recompiled.stderr)
            );
        }
    }
}

#[test]
fn version_11_method_case_contains_call_feedback() {
    if !compiler().is_file() {
        eprintln!("skipping: bundled Luau compiler is absent");
        return;
    }

    let profile = &PROFILES[2];
    let text = compile("text", profile, &source("06_method_chains"));
    assert!(text.status.success());
    assert!(
        String::from_utf8_lossy(&text.stdout).contains("CALLFB"),
        "v11 test input did not exercise CALLFB"
    );
}

#[test]
fn version_9_integer_literals_preserve_integer_syntax() {
    if !compiler().is_file() {
        eprintln!("skipping: bundled Luau compiler is absent");
        return;
    }

    let profile = VersionProfile {
        name: "v9-integer",
        version: 9,
        flags: "LuauBytecodeCostModel=false,LuauEmitCallFeedback=false,DebugLuauUserDefinedClasses=false,LuauIntegerType2=true",
    };
    let source = version_source("v8_integer");
    let compiled = compile("binary", &profile, &source);
    assert!(compiled.status.success());
    assert_eq!(compiled.stdout.first().copied(), Some(9));

    let decompiled = crate::try_decompile_bytecode(&compiled.stdout, 1).unwrap();
    assert!(decompiled.contains("9007199254740993i"), "{decompiled}");
    assert!(
        decompiled.contains("(-9223372036854775807i - 1i)"),
        "{decompiled}"
    );

    let output = workspace().join("target/compatibility-tests/v9-integer.luau");
    fs::create_dir_all(output.parent().unwrap()).unwrap();
    fs::write(&output, decompiled).unwrap();
    let recompiled = compile("null", &profile, &output);
    assert!(
        recompiled.status.success(),
        "{}",
        String::from_utf8_lossy(&recompiled.stderr)
    );
}

#[test]
fn version_10_class_shape_reconstructs_class_declaration() {
    if !compiler().is_file() {
        eprintln!("skipping: bundled Luau compiler is absent");
        return;
    }

    let profile = &PROFILES[1];
    let source = version_source("v10_class");
    let text = compile("text", profile, &source);
    assert!(text.status.success());
    let instructions = String::from_utf8_lossy(&text.stdout);
    assert!(instructions.contains("LOADKX"));
    assert!(instructions.contains("NEWCLASSMEMBER"));

    let compiled = compile("binary", profile, &source);
    assert!(compiled.status.success());
    assert_eq!(compiled.stdout.first().copied(), Some(10));
    let decompiled = crate::try_decompile_bytecode(&compiled.stdout, 1).unwrap();
    assert!(decompiled.contains("class Counter"), "{decompiled}");
    assert!(decompiled.contains("public value"), "{decompiled}");
    assert!(decompiled.contains("function add("), "{decompiled}");
    assert!(decompiled.contains(".value +"), "{decompiled}");

    let output = workspace().join("target/compatibility-tests/v10-class.luau");
    fs::create_dir_all(output.parent().unwrap()).unwrap();
    fs::write(&output, decompiled).unwrap();
    let recompiled = compile("null", profile, &output);
    assert!(
        recompiled.status.success(),
        "{}",
        String::from_utf8_lossy(&recompiled.stderr)
    );
}
