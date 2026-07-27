# Luau Bytecode 4–12 Compatibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Use parallel agents for read-only review and verification; keep one implementation owner because parser, opcode, and lifter changes overlap.

**Goal:** Preserve Luau bytecode 4–6 behavior while adding format-aware parsing and useful lifting through bytecode version 12, enabling current-compiler corpus baselines.

**Architecture:** A validated `BytecodeVersion` flows through chunk, prototype, constant, and instruction parsing. Protocol layout decisions live beside version/opcode metadata, while the lifter groups equivalent opcodes by semantics. Version 12 prototypes are bounded by declared sizes; versions 4–11 consume all known fields sequentially; top-level success requires no unexplained remainder.

**Tech Stack:** Rust nightly, `nom`, `nom-leb128`, existing AST/CFG/restructure crates, bundled Luau 0.731 Windows compiler, Python corpus runner.

## Global Constraints

- Accept bytecode versions 4 through 12.
- Preserve existing version 4–6 behavior.
- Never widen only the version gate or skip unknown sequential fields.
- Version-specific parsing must depend only on validated bytecode version, flags, type version, tag, or opcode metadata.
- Instruction word length and encoding must have one exhaustive source of truth.
- Keep one decoded instruction-vector slot per serialized word so bytecode program counters and jump offsets remain stable.
- Preserve exact signed 64-bit integers and double-precision vectors.
- Parse valid debug records without panicking.
- Version 12 must never read beyond a declared prototype boundary.
- Top-level parsing must reject unexplained bytes after the main-prototype id.
- No logic may inspect corpus filenames, literal values, URLs, register numbers, or fixed instruction sequences.
- Repository has no `.git`; use verification checkpoints instead of commits.

---

### Task 1: Version Context, Prototype Framing, and Metadata

**Files:**
- Create: `luau-lifter/src/deserializer/version.rs`
- Create: `luau-lifter/src/deserializer/tests.rs`
- Modify: `luau-lifter/src/deserializer/mod.rs`
- Modify: `luau-lifter/src/deserializer/bytecode.rs`
- Modify: `luau-lifter/src/deserializer/chunk.rs`
- Modify: `luau-lifter/src/deserializer/function.rs`

**Interfaces:**
- Produces: `BytecodeVersion::new(u8) -> Result<BytecodeVersion, String>`.
- Produces: version predicates `has_feedback()`, `has_sized_prototypes()`, and `has_cost()`.
- Produces: `Function::parse(input, encode_key, version)`.
- Preserves: `deserialize(bytecode, encode_key) -> Result<Bytecode, String>`.

- [ ] **Step 1: Add synthetic version-fixture tests**

Create test helpers that serialize unsigned LEB128 values and a minimal chunk containing one prototype with one `RETURN` instruction. Generate correct layouts for every version 4 through 12:

```text
version byte
type version 1
zero strings
one prototype
v12 only: prototype byte length
max stack 1, params 0, upvalues 0, vararg 0
flags 0
zero type bytes
one instruction: RETURN R0 0
zero constants
zero child prototypes
line defined 0, function name 0
no line info
no debug info
v11+: zero feedback slots
main prototype 0
```

Tests:

```rust
#[test]
fn accepts_every_bytecode_version_from_4_through_12()

#[test]
fn rejects_versions_outside_4_through_12_without_panicking()

#[test]
fn rejects_trailing_chunk_bytes()

#[test]
fn rejects_v12_prototype_size_past_input()

#[test]
fn accepts_v12_unknown_bytes_inside_declared_prototype()
```

The final test follows official forward-compatible v12 behavior: unknown bytes inside the declared prototype boundary are skipped, but bytes after the main id are rejected.

- [ ] **Step 2: Run compatibility tests and verify red state**

Run:

```powershell
cargo +nightly test -p luau-lifter deserializer::tests -- --nocapture
```

Expected: versions 7–12 fail at the current top-level gate; malformed-input expectations are unmet.

- [ ] **Step 3: Implement validated version context**

`BytecodeVersion` must be a copyable wrapper around `u8`, accept exactly `4..=12`, and expose:

```rust
pub const fn value(self) -> u8
pub const fn has_feedback(self) -> bool
pub const fn has_sized_prototypes(self) -> bool
pub const fn has_cost(self) -> bool
```

Predicates use protocol boundaries `>= 11`, `>= 12`, and `>= 12`.

Replace the panic in `Bytecode::parse` with a normal parse error whose public `deserialize` message includes the unsupported version.

- [ ] **Step 4: Bound version-12 prototypes**

In `Chunk::parse`, read prototype count explicitly instead of using the generic list parser. For every prototype:

- versions 4–11: call `Function::parse` on remaining chunk input;
- version 12: read a ULEB128 byte length, take exactly that slice, parse known fields inside it, then advance outer input by the declared length;
- reject truncated declared lengths;
- allow unknown trailing bytes only inside a valid v12 prototype slice.

Thread `BytecodeVersion` into every `Function::parse` call.

- [ ] **Step 5: Parse function flags, debug data, feedback, and cost**

Retain `flags: u8` on `Function`.

Add focused data types:

```rust
pub struct DebugLocal {
    pub name: usize,
    pub start_pc: usize,
    pub end_pc: usize,
    pub register: u8,
}

pub struct FeedbackSlot {
    pub kind: u8,
    pub pc: usize,
}
```

Store debug locals, debug-upvalue string indices, feedback slots, and optional `u64` cost. Use `nom_leb128::leb128_u64` for cost.

Parse absolute line deltas with `le_i32`, not `le_u32`.

Version rules:

- v4–v10: stop after debug records;
- v11+: parse feedback count, then one-byte kind and ULEB128 PC for every slot;
- v12 with flags bit `0x08`: parse ULEB128 `u64` cost;
- v12 without that flag: no cost field.

Remove the debug-info panic.

- [ ] **Step 6: Require complete top-level consumption**

`deserialize` must return an error if valid chunk parsing leaves bytes after the main-prototype id. Version-12 prototype-internal extension bytes are already consumed by their declared boundary and do not count as top-level remainder.

- [ ] **Step 7: Verify Task 1**

Run:

```powershell
cargo +nightly test -p luau-lifter deserializer::tests -- --nocapture
cargo +nightly build -p luau-lifter
```

Expected: synthetic version/framing/debug tests pass; build exits `0`.

---

### Task 2: Exhaustive Constant and Instruction Metadata

**Files:**
- Modify: `luau-lifter/src/deserializer/constant.rs`
- Modify: `luau-lifter/src/op_code.rs`
- Modify: `luau-lifter/src/instruction.rs`
- Modify: `luau-lifter/src/deserializer/function.rs`
- Modify: `luau-lifter/src/deserializer/tests.rs`

**Interfaces:**
- Produces: exact constant variants for table templates, integer, class shape, and double vector.
- Produces: `InstructionEncoding::{Abc, Ad, E}`.
- Produces on `OpCode`: `encoding()`, `has_aux()`, `minimum_version()`.
- Consumes: `BytecodeVersion` for tag/opcode validity.

- [ ] **Step 1: Add failing constant-payload tests**

Hand-serialize and assert:

```text
tag 8: two entries [(key 3, value 4), (key 5, sentinel -1)]
tag 9: positive 9007199254740993 and negative -9223372036854775808
tag 10: class-name index 1, properties [2,3], methods [4]
tag 11: four f64 values including a value not exactly representable as f32
```

Also assert tag 8 is rejected before version 7, tag 9 before version 8, tag 10 before version 10, and tag 11 before version 12.

- [ ] **Step 2: Add failing opcode-metadata tests**

Assert exact ordinals and metadata:

| Opcode | Ordinal | Encoding | AUX | Minimum version |
| --- | ---: | --- | --- | ---: |
| `GETUDATAKS` | 83 | ABC | yes | 9 |
| `SETUDATAKS` | 84 | ABC | yes | 9 |
| `NAMECALLUDATA` | 85 | ABC | yes | 9 |
| `NEWCLASSMEMBER` | 86 | ABC | yes | 10 |
| `CALLFB` | 87 | ABC | yes | 11 |
| `CMPPROTO` | 88 | AD | yes | 11 |

Assert every enum value from `NOP` through `CMPPROTO` has an encoding and word length, and new opcodes are rejected under earlier versions.

- [ ] **Step 3: Run tests and verify red state**

Run:

```powershell
cargo +nightly test -p luau-lifter deserializer::tests -- --nocapture
```

Expected: new tag and opcode tests fail because current schema ends at tag 7 and opcode 82.

- [ ] **Step 4: Implement later constant payloads**

Use variants that preserve protocol information:

```rust
Table {
    entries: Vec<(usize, Option<usize>)>,
}
Integer(i64)
ClassShape {
    class_name: usize,
    properties: Vec<usize>,
    methods: Vec<usize>,
}
VectorF(f32, f32, f32, f32)
VectorD(f64, f64, f64, f64)
```

Convert legacy tag-5 table keys to entries with `None` values. For tag 8, signed `-1` becomes `None`; nonnegative signed indices become `Some(index)`. Decode integer sign plus ULEB128 magnitude without converting through `f64`.

- [ ] **Step 5: Centralize opcode layout**

Append ordinals 83–88 to `OpCode`. Move encoding, AUX, and minimum-version classification into exhaustive methods on `OpCode`.

Rewrite `Instruction::parse` to:

1. decode opcode using existing encode key;
2. convert decoded byte to `OpCode`;
3. validate `minimum_version`;
4. choose ABC, AD, or E from `encoding`;
5. return a structured error instead of `unreachable!`.

Rewrite function instruction walking to consume `1 + has_aux()` words and attach raw AUX to the decoded instruction. Insert one `NOP` placeholder for every AUX word so jump PCs retain serialized-word indices.

- [ ] **Step 6: Verify Task 2**

Run:

```powershell
cargo +nightly test -p luau-lifter deserializer::tests -- --nocapture
cargo +nightly build -p luau-lifter
```

Expected: constant/opcode metadata tests pass; build exits `0`.

---

### Task 3: Semantic Lifting for New Formats

**Files:**
- Modify: `ast/src/literal.rs`
- Modify: `luau-lifter/src/lifter.rs`
- Create: `luau-lifter/src/compatibility_tests.rs`
- Modify: `luau-lifter/src/lib.rs`

**Interfaces:**
- Produces: exact `Literal::Integer(i64)` and `Literal::VectorD(f64, f64, f64)`.
- Produces: semantic lowering for v9 userdata aliases, v10 class members, v11 feedback calls, and prototype-guard fallback.
- Produces: populated `DUPTABLE` AST values.

- [ ] **Step 1: Add failing literal-format tests**

Assert:

```rust
assert_eq!(Literal::Integer(9_007_199_254_740_993).to_string(), "9007199254740993");
assert_eq!(Literal::Integer(i64::MIN).to_string(), "-9223372036854775808");
assert_eq!(
    Literal::VectorD(1.25, 2.5, 3.75).to_string(),
    "Vector3.new(1.25, 2.5, 3.75)"
);
```

Integer and double-vector truthiness/type inference follow numeric and vector behavior respectively.

- [ ] **Step 2: Add failing real-compiler compatibility tests**

Compile `01_literals_locals`, `06_method_chains`, `16_closure_capture`, and `24_wonky_integration` using bundled compiler under real v9, v10, v11, and v12 flag profiles. For every artifact:

- first byte equals requested version;
- deserialization consumes the entire chunk;
- decompilation does not panic;
- output recompiles with bundled compiler.

Version 11 cases must contain `CALLFB` in compiler text output. The test may skip only when bundled Windows tools are absent; it may not convert a decompiler failure into a skip.

- [ ] **Step 3: Run tests and verify red state**

Run:

```powershell
cargo +nightly test -p luau-lifter compatibility_tests -- --nocapture
```

Expected: new opcodes/constants have no lifter semantics, so at least v11/v12 cases fail.

- [ ] **Step 4: Preserve exact literal values**

Implement AST formatting, truthiness, traversal, side-effect behavior, and type inference for exact integers and double vectors. Map bytecode integer and double-vector constants directly to these variants.

- [ ] **Step 5: Reconstruct table templates**

For `DUPTABLE`, read its table-template constant:

- convert key constant indices to AST literals;
- use the referenced value constant when present;
- use numeric zero for a missing template value, matching VM initialization before later writes;
- omit entries whose referenced value constant is nil;
- emit one `ast::Table` assignment.

This transformation depends only on constant-table relationships.

- [ ] **Step 6: Lower equivalent new opcodes by semantic family**

- `GETUDATAKS`: lower like `GETTABLEKS`, masking AUX to its low 16-bit constant index.
- `SETUDATAKS`: lower like `SETTABLEKS`, masking AUX likewise.
- `NAMECALLUDATA`: lower like `NAMECALL`, masking AUX likewise.
- `NEWCLASSMEMBER`: lower as indexed assignment from class register A and member-name constant AUX to value register C.
- `CALLFB`: lower exactly like `CALL` after its AUX feedback-slot word has been consumed.
- `CMPPROTO`: lower as an unconditional jump to its D target. This selects the compiler’s generic fallback path instead of its profile-specialized fast path; both paths implement the same source behavior, while the fallback has source-level semantics.

- [ ] **Step 7: Localize unsupported and malformed failures**

Add:

```rust
pub fn try_decompile_bytecode(
    bytecode: &[u8],
    encode_key: u8,
) -> Result<String, String>
```

Keep existing `decompile_bytecode` for API compatibility, returning a valid Luau comment containing the error. Change CLI `main` to call the fallible API, write the error to stderr, and exit nonzero.

- [ ] **Step 8: Verify Task 3**

Run:

```powershell
cargo +nightly test -p luau-lifter compatibility_tests -- --nocapture
cargo +nightly test -p luau-lifter
cargo +nightly build -p luau-lifter
```

Expected: real v9–v12 artifacts decompile and recompile; exact-literal tests pass.

---

### Task 4: Corpus Version Profiles

**Files:**
- Modify: `tools/luau_corpus/model.py`
- Modify: `tools/luau_corpus/process.py`
- Modify: `tools/run_luau_corpus.py`
- Modify: `tests/python/test_luau_corpus.py`

**Interfaces:**
- Extends: `CompileProfile` with `fast_flags: tuple[str, ...] = ()`.
- Produces: `COMPATIBILITY_PROFILES` for bytecode 9, 10, 11, and 12.
- Extends CLI: `--profiles compatibility`.

- [ ] **Step 1: Add failing profile-command tests**

Assert compatibility profiles emit one `--fflags=` argument and produce these headers on a focused real compile:

```text
V9 -> 9
V10 -> 10
V11 -> 11
V12 -> 12
```

Use flags:

```text
V9: LuauBytecodeCostModel=false,LuauEmitCallFeedback=false,DebugLuauUserDefinedClasses=false
V10: LuauBytecodeCostModel=false,LuauEmitCallFeedback=false,DebugLuauUserDefinedClasses=true
V11: LuauBytecodeCostModel=false,LuauEmitCallFeedback=true,DebugLuauUserDefinedClasses=false
V12: LuauBytecodeCostModel=true
```

- [ ] **Step 2: Run Python tests and verify red state**

Run:

```powershell
python -m unittest tests.python.test_luau_corpus -v
```

Expected: `CompileProfile` lacks fast flags and compatibility profiles.

- [ ] **Step 3: Implement profile flags and CLI selection**

Place `--fflags=<comma-separated flags>` before source path only when flags are nonempty. Add `compatibility` CLI selection without changing primary/secondary profile behavior.

- [ ] **Step 4: Record observed bytecode version**

Add `bytecode_version: int | None` to `CaseResult`. When compilation succeeds and stdout is nonempty, record `stdout[0]`. Include it in JSON and Markdown summaries.

- [ ] **Step 5: Verify Task 4**

Run:

```powershell
python -m unittest tests.python.test_luau_corpus -v
python tools\run_luau_corpus.py --profiles compatibility --case 01_literals_locals --output tests\luau_corpus\results\compatibility-smoke
```

Expected: tests pass; summary reports versions 9, 10, 11, and 12; all four outputs recompile.

---

### Task 5: Full Baseline and Compatibility Audit

**Files:**
- Generate: `tests/luau_corpus/results/baseline/**`
- Modify: `docs/decompiler-baseline-findings.md`

- [ ] **Step 1: Generate full primary and compatibility baselines**

Run:

```powershell
python tools\run_luau_corpus.py --profiles primary --output tests\luau_corpus\results\baseline-primary
python tools\run_luau_corpus.py --profiles compatibility --output tests\luau_corpus\results\baseline-versions
```

Expected: 72 primary attempts plus 96 version-compatibility attempts; summaries retain every failure.

- [ ] **Step 2: Run full Rust and Python checks**

Run:

```powershell
python -m unittest tests.python.test_luau_corpus -v
cargo +nightly test --workspace
cargo +nightly build --workspace
```

Expected: exit `0`.

- [ ] **Step 3: Run decorrelated read-only review**

Dispatch three agents:

1. v4–v12 format and malformed-input review;
2. opcode/AUX/PC and lifter-semantics review;
3. source/output quality and hard-coding review.

Require exact evidence paths, confidence, regression risks, and a hard-coding verdict.

- [ ] **Step 4: Update baseline report**

Add:

- exact case/profile pass counts;
- representative simple, closure, control-flow, and wonky source/output pairs;
- remaining unsupported features;
- ranked general CFG/SSA/readability improvements;
- compatibility evidence and limitations for versions 4–8.

- [ ] **Step 5: Continue into quality remediation**

Write the next evidence-specific plan from full baseline output. Select at most three HIGH-confidence general readability root causes, then execute them with focused red-green tests and full-corpus regression comparison.
