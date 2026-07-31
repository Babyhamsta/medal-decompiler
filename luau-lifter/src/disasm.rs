//! Read-only textual dump of deserialized Luau bytecode.
//!
//! This module never participates in lifting or decompilation; it exists so a
//! chunk's raw instruction stream can be inspected directly, with register
//! numbers, AUX words and resolved constants spelled out.

use std::fmt::Write as _;

use crate::deserializer::{
    self,
    bytecode::Bytecode,
    chunk::Chunk,
    constant::Constant,
    function::{DebugLocal, Function},
};
use crate::instruction::Instruction;
use crate::op_code::OpCode;

/// Selects which prototypes a dump covers.
#[derive(Debug, Clone)]
pub enum ProtoSelection {
    All,
    Only(Vec<usize>),
}

fn escape_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 2);
    out.push('"');
    for &byte in bytes {
        match byte {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(byte as char),
            other => {
                let _ = write!(out, "\\x{other:02x}");
            }
        }
    }
    out.push('"');
    out
}

fn string_at(chunk: &Chunk, index: usize) -> String {
    match index.checked_sub(1).and_then(|i| chunk.string_table.get(i)) {
        Some(bytes) => escape_bytes(bytes),
        None => format!("<string#{index}?>"),
    }
}

fn constant_at(chunk: &Chunk, function: &Function, index: usize) -> String {
    let Some(constant) = function.constants.get(index) else {
        return format!("<K{index} out of range, {} constants>", function.constants.len());
    };
    match constant {
        Constant::Nil => "nil".to_owned(),
        Constant::Boolean(value) => value.to_string(),
        Constant::Number(value) => format!("{value}"),
        Constant::Integer(value) => format!("{value}"),
        Constant::String(string_index) => string_at(chunk, *string_index),
        Constant::Import(value) => format!("import(0x{value:08x})"),
        Constant::Table { entries } => format!("table({} entries)", entries.len()),
        Constant::Closure(proto) => format!("closure(proto {proto})"),
        Constant::VectorF(x, y, z, w) => format!("vectorf({x}, {y}, {z}, {w})"),
        Constant::VectorD(x, y, z, w) => format!("vectord({x}, {y}, {z}, {w})"),
        Constant::ClassShape { class_name, .. } => {
            format!("classshape({})", string_at(chunk, *class_name))
        }
    }
}

/// Renders the import path packed into a GETIMPORT AUX word.
fn import_path(chunk: &Chunk, function: &Function, aux: u32) -> String {
    let length = (aux >> 30) & 3;
    let parts: Vec<usize> = vec![
        ((aux >> 20) & 1023) as usize,
        ((aux >> 10) & 1023) as usize,
        (aux & 1023) as usize,
    ];
    let mut rendered = Vec::new();
    for part in parts.into_iter().take(length as usize) {
        rendered.push(format!("K{part}={}", constant_at(chunk, function, part)));
    }
    format!("len={length} [{}]", rendered.join(" . "))
}

fn operands(chunk: &Chunk, function: &Function, instruction: Instruction) -> String {
    match instruction {
        Instruction::BC {
            op_code,
            a,
            b,
            c,
            aux,
        } => match op_code {
            OpCode::LOP_NOP | OpCode::LOP_BREAK => String::new(),
            OpCode::LOP_LOADNIL => format!("R{a}"),
            OpCode::LOP_LOADB => format!("R{a} {b} jump+{c}"),
            OpCode::LOP_MOVE => format!("R{a} <- R{b}"),
            OpCode::LOP_GETGLOBAL => format!(
                "R{a} <- _G[K{aux}={}] slot={c}",
                constant_at(chunk, function, aux as usize)
            ),
            OpCode::LOP_SETGLOBAL => format!(
                "_G[K{aux}={}] <- R{a} slot={c}",
                constant_at(chunk, function, aux as usize)
            ),
            OpCode::LOP_GETUPVAL => format!("R{a} <- U{b}"),
            OpCode::LOP_SETUPVAL => format!("U{b} <- R{a}"),
            OpCode::LOP_CLOSEUPVALS => format!("R{a}.."),
            OpCode::LOP_GETTABLE => format!("R{a} <- R{b}[R{c}]"),
            OpCode::LOP_SETTABLE => format!("R{b}[R{c}] <- R{a}"),
            OpCode::LOP_GETTABLEKS | OpCode::LOP_GETUDATAKS => {
                let key = if op_code == OpCode::LOP_GETUDATAKS {
                    aux & 0xffff
                } else {
                    aux
                };
                format!(
                    "R{a} <- R{b}[K{key}={}] slot={c}",
                    constant_at(chunk, function, key as usize)
                )
            }
            OpCode::LOP_SETTABLEKS | OpCode::LOP_SETUDATAKS => {
                let key = if op_code == OpCode::LOP_SETUDATAKS {
                    aux & 0xffff
                } else {
                    aux
                };
                format!(
                    "R{b}[K{key}={}] <- R{a} slot={c}",
                    constant_at(chunk, function, key as usize)
                )
            }
            OpCode::LOP_GETTABLEN => format!("R{a} <- R{b}[{}]", c as u16 + 1),
            OpCode::LOP_SETTABLEN => format!("R{b}[{}] <- R{a}", c as u16 + 1),
            OpCode::LOP_NAMECALL | OpCode::LOP_NAMECALLUDATA => {
                let key = if op_code == OpCode::LOP_NAMECALLUDATA {
                    aux & 0xffff
                } else {
                    aux
                };
                format!(
                    "R{a} <- R{b}[K{key}={}] ; R{} <- R{b} slot={c}",
                    constant_at(chunk, function, key as usize),
                    a + 1
                )
            }
            OpCode::LOP_CALL => format!(
                "func=R{a} nargs={} nresults={}",
                if b == 0 {
                    "MULTRET".to_owned()
                } else {
                    format!("{}", b - 1)
                },
                if c == 0 {
                    "MULTRET".to_owned()
                } else {
                    format!("{}", c - 1)
                }
            ),
            OpCode::LOP_CALLFB => format!(
                "func=R{a} nargs={} nresults={} aux=0x{aux:08x}",
                if b == 0 {
                    "MULTRET".to_owned()
                } else {
                    format!("{}", b - 1)
                },
                if c == 0 {
                    "MULTRET".to_owned()
                } else {
                    format!("{}", c - 1)
                }
            ),
            OpCode::LOP_RETURN => format!(
                "start=R{a} count={}",
                if b == 0 {
                    "MULTRET".to_owned()
                } else {
                    format!("{}", b - 1)
                }
            ),
            OpCode::LOP_ADD
            | OpCode::LOP_SUB
            | OpCode::LOP_MUL
            | OpCode::LOP_DIV
            | OpCode::LOP_MOD
            | OpCode::LOP_POW
            | OpCode::LOP_IDIV
            | OpCode::LOP_AND
            | OpCode::LOP_OR => format!("R{a} <- R{b}, R{c}"),
            OpCode::LOP_ADDK
            | OpCode::LOP_SUBK
            | OpCode::LOP_MULK
            | OpCode::LOP_DIVK
            | OpCode::LOP_MODK
            | OpCode::LOP_POWK
            | OpCode::LOP_IDIVK
            | OpCode::LOP_ANDK
            | OpCode::LOP_ORK => format!(
                "R{a} <- R{b}, K{c}={}",
                constant_at(chunk, function, c as usize)
            ),
            OpCode::LOP_SUBRK | OpCode::LOP_DIVRK => format!(
                "R{a} <- K{b}={}, R{c}",
                constant_at(chunk, function, b as usize)
            ),
            OpCode::LOP_CONCAT => format!("R{a} <- R{b}..R{c}"),
            OpCode::LOP_NOT | OpCode::LOP_MINUS | OpCode::LOP_LENGTH => format!("R{a} <- R{b}"),
            OpCode::LOP_NEWTABLE => format!(
                "R{a} hashsize={} arraysize={aux}",
                if b == 0 { 0 } else { 1 << (b - 1) }
            ),
            OpCode::LOP_SETLIST => format!(
                "R{b}.. -> R{a}[{aux}..] count={}",
                if c == 0 {
                    "MULTRET".to_owned()
                } else {
                    format!("{}", c - 1)
                }
            ),
            OpCode::LOP_GETVARARGS => format!(
                "R{a}.. count={}",
                if b == 0 {
                    "MULTRET".to_owned()
                } else {
                    format!("{}", b - 1)
                }
            ),
            OpCode::LOP_PREPVARARGS => format!("numparams={a}"),
            OpCode::LOP_FASTCALL => format!("builtin={a} jump+{c}"),
            OpCode::LOP_FASTCALL1 => format!("builtin={a} arg=R{b} jump+{c}"),
            OpCode::LOP_FASTCALL2 => format!("builtin={a} arg=R{b} arg2=R{aux} jump+{c}"),
            OpCode::LOP_FASTCALL2K => format!(
                "builtin={a} arg=R{b} arg2=K{aux}={} jump+{c}",
                constant_at(chunk, function, aux as usize)
            ),
            OpCode::LOP_FASTCALL3 => format!(
                "builtin={a} arg=R{b} arg2=R{} arg3=R{} jump+{c}",
                aux & 0xff,
                (aux >> 8) & 0xff
            ),
            OpCode::LOP_CAPTURE => {
                let kind = match a {
                    0 => "VAL",
                    1 => "REF",
                    2 => "UPVAL",
                    _ => "?",
                };
                if a == 2 {
                    format!("{kind} U{b}")
                } else {
                    format!("{kind} R{b}")
                }
            }
            OpCode::LOP_NEWCLASSMEMBER => format!(
                "R{a}[K{aux}={}] <- R{c}",
                constant_at(chunk, function, aux as usize)
            ),
            _ => format!("A={a} B={b} C={c} AUX=0x{aux:08x}"),
        },
        Instruction::AD {
            op_code,
            a,
            d,
            aux,
        } => match op_code {
            OpCode::LOP_LOADN => format!("R{a} <- {d}"),
            OpCode::LOP_LOADK => format!(
                "R{a} <- K{d}={}",
                constant_at(chunk, function, d as usize)
            ),
            OpCode::LOP_LOADKX => format!(
                "R{a} <- K{aux}={}",
                constant_at(chunk, function, aux as usize)
            ),
            OpCode::LOP_GETIMPORT => format!(
                "R{a} <- K{d}={} ; aux=0x{aux:08x} {}",
                constant_at(chunk, function, d as usize),
                import_path(chunk, function, aux)
            ),
            OpCode::LOP_NEWCLOSURE => match function.functions.get(d as usize) {
                Some(child) => format!("R{a} <- child[{d}] = proto {child}"),
                None => format!(
                    "R{a} <- child[{d}] OUT OF RANGE ({} children)",
                    function.functions.len()
                ),
            },
            OpCode::LOP_DUPCLOSURE => format!(
                "R{a} <- K{d}={}",
                constant_at(chunk, function, d as usize)
            ),
            OpCode::LOP_DUPTABLE => format!(
                "R{a} <- K{d}={}",
                constant_at(chunk, function, d as usize)
            ),
            OpCode::LOP_JUMP | OpCode::LOP_JUMPBACK => format!("d={d}"),
            OpCode::LOP_JUMPIF | OpCode::LOP_JUMPIFNOT => format!("R{a} d={d}"),
            OpCode::LOP_JUMPIFEQ
            | OpCode::LOP_JUMPIFLE
            | OpCode::LOP_JUMPIFLT
            | OpCode::LOP_JUMPIFNOTEQ
            | OpCode::LOP_JUMPIFNOTLE
            | OpCode::LOP_JUMPIFNOTLT => format!("R{a}, R{aux} d={d}"),
            OpCode::LOP_JUMPXEQKNIL | OpCode::LOP_JUMPXEQKB => {
                format!("R{a} d={d} aux=0x{aux:08x}")
            }
            OpCode::LOP_JUMPXEQKN | OpCode::LOP_JUMPXEQKS => format!(
                "R{a} d={d} K{}={} not={}",
                aux & 0x00ff_ffff,
                constant_at(chunk, function, (aux & 0x00ff_ffff) as usize),
                aux >> 31
            ),
            OpCode::LOP_FORNPREP | OpCode::LOP_FORNLOOP => format!("base=R{a} d={d}"),
            OpCode::LOP_FORGPREP | OpCode::LOP_FORGPREP_INEXT | OpCode::LOP_FORGPREP_NEXT => {
                format!("base=R{a} d={d}")
            }
            OpCode::LOP_FORGLOOP => format!(
                "base=R{a} d={d} vars={} ipairs={}",
                aux & 0xff,
                aux >> 31
            ),
            _ => format!("A={a} D={d} AUX=0x{aux:08x}"),
        },
        Instruction::E { op_code, e } => match op_code {
            OpCode::LOP_JUMPX => format!("e={e}"),
            OpCode::LOP_COVERAGE => format!("hits={e}"),
            _ => format!("E={e}"),
        },
    }
}

fn source_line(function: &Function, index: usize) -> Option<usize> {
    let gap = function.line_gap_log2?;
    let deltas = function.line_info_delta.as_ref()?;
    let absolute_deltas = function.abs_line_info_delta.as_ref()?;
    let relative = deltas
        .iter()
        .take(index + 1)
        .fold(0u8, |line, delta| line.wrapping_add(*delta));
    let interval = index >> gap;
    let absolute = absolute_deltas
        .iter()
        .take(interval + 1)
        .copied()
        .sum::<i32>();
    usize::try_from(absolute + i32::from(relative)).ok()
}

fn locals_at(chunk: &Chunk, locals: &[DebugLocal], index: usize) -> String {
    let live: Vec<String> = locals
        .iter()
        .filter(|local| local.start_pc <= index && index < local.end_pc)
        .map(|local| format!("R{}={}", local.register, string_at(chunk, local.name)))
        .collect();
    if live.is_empty() {
        String::new()
    } else {
        format!("  ; live {}", live.join(", "))
    }
}

fn write_prototype(out: &mut String, chunk: &Chunk, index: usize, show_locals: bool) {
    let function = &chunk.functions[index];
    let name = if function.function_name == 0 {
        "<anonymous>".to_owned()
    } else {
        string_at(chunk, function.function_name)
    };
    let _ = writeln!(
        out,
        "\n-- proto {index}{}  name={name}",
        if index == chunk.main { " (MAIN)" } else { "" }
    );
    let _ = writeln!(
        out,
        "   params={} upvalues={} vararg={} maxstacksize={} instructions={} constants={} children={} line_defined={} flags=0x{:02x}",
        function.num_parameters,
        function.num_upvalues,
        function.is_vararg,
        function.max_stack_size,
        function.instructions.len(),
        function.constants.len(),
        function.functions.len(),
        function.line_defined,
        function.flags,
    );
    if !function.debug_upvalues.is_empty() {
        let names: Vec<String> = function
            .debug_upvalues
            .iter()
            .enumerate()
            .map(|(i, name)| format!("U{i}={}", string_at(chunk, *name)))
            .collect();
        let _ = writeln!(out, "   debug upvalue names: {}", names.join(", "));
    }
    if !function.functions.is_empty() {
        let _ = writeln!(out, "   child protos: {:?}", function.functions);
    }

    let mut pending_aux = false;
    for (offset, instruction) in function.instructions.iter().enumerate() {
        if pending_aux {
            pending_aux = false;
            let _ = writeln!(out, "  {offset:5}      (AUX word of previous instruction)");
            continue;
        }
        let op_code = instruction.op_code();
        pending_aux = op_code.has_aux();
        let mnemonic = format!("{op_code:?}");
        let mnemonic = mnemonic.strip_prefix("LOP_").unwrap_or(&mnemonic).to_owned();
        let line = match source_line(function, offset) {
            Some(line) => format!("[{line:>5}]"),
            None => "[     ]".to_owned(),
        };
        let live = if show_locals {
            locals_at(chunk, &function.debug_locals, offset)
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "  {offset:5} {line} {mnemonic:<16} {}{live}",
            operands(chunk, function, *instruction)
        );
    }
}

/// Dumps a whole chunk (or the selected prototypes) as text.
pub fn disassemble(
    bytecode: &[u8],
    encode_key: u8,
    selection: &ProtoSelection,
    show_locals: bool,
) -> Result<String, String> {
    let chunk = match deserializer::deserialize(bytecode, encode_key)? {
        Bytecode::Error(message) => return Err(format!("bytecode carries an error: {message}")),
        Bytecode::Chunk(chunk) => chunk,
    };
    let version = bytecode.first().copied().unwrap_or(0);

    let mut out = String::new();
    let _ = writeln!(
        out,
        "== bytecode version {version}, {} prototypes, {} strings, main = proto {}",
        chunk.functions.len(),
        chunk.string_table.len(),
        chunk.main
    );

    let indices: Vec<usize> = match selection {
        ProtoSelection::All => (0..chunk.functions.len()).collect(),
        ProtoSelection::Only(list) => list.clone(),
    };
    for index in indices {
        if index >= chunk.functions.len() {
            return Err(format!(
                "prototype {index} is out of range; the chunk has {}",
                chunk.functions.len()
            ));
        }
        write_prototype(&mut out, &chunk, index, show_locals);
    }
    Ok(out)
}

/// One line per prototype, for locating a small prototype in a large chunk.
pub fn list_prototypes(bytecode: &[u8], encode_key: u8) -> Result<String, String> {
    let chunk = match deserializer::deserialize(bytecode, encode_key)? {
        Bytecode::Error(message) => return Err(format!("bytecode carries an error: {message}")),
        Bytecode::Chunk(chunk) => chunk,
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "== {} prototypes, main = proto {}",
        chunk.functions.len(),
        chunk.main
    );
    for (index, function) in chunk.functions.iter().enumerate() {
        let _ = writeln!(
            out,
            "proto {index:5}  insns={:6}  params={:3}  upvalues={:3}  vararg={:5}  maxstack={:3}  line={:6}  name={}",
            function.instructions.len(),
            function.num_parameters,
            function.num_upvalues,
            function.is_vararg,
            function.max_stack_size,
            function.line_defined,
            if function.function_name == 0 {
                "<anonymous>".to_owned()
            } else {
                string_at(&chunk, function.function_name)
            }
        );
    }
    Ok(out)
}

/// The register range an instruction defines, as `(first, last_inclusive)`.
/// `None` means the instruction defines nothing. `u8::MAX` as the upper bound
/// stands for "up to the stack top", which is not statically known.
fn defined_registers(instruction: Instruction) -> Option<(u8, u8)> {
    match instruction {
        Instruction::BC {
            op_code, a, b, c, ..
        } => match op_code {
            OpCode::LOP_LOADNIL
            | OpCode::LOP_LOADB
            | OpCode::LOP_MOVE
            | OpCode::LOP_GETGLOBAL
            | OpCode::LOP_GETUPVAL
            | OpCode::LOP_GETTABLE
            | OpCode::LOP_GETTABLEKS
            | OpCode::LOP_GETTABLEN
            | OpCode::LOP_GETUDATAKS
            | OpCode::LOP_ADD
            | OpCode::LOP_SUB
            | OpCode::LOP_MUL
            | OpCode::LOP_DIV
            | OpCode::LOP_MOD
            | OpCode::LOP_POW
            | OpCode::LOP_IDIV
            | OpCode::LOP_ADDK
            | OpCode::LOP_SUBK
            | OpCode::LOP_MULK
            | OpCode::LOP_DIVK
            | OpCode::LOP_MODK
            | OpCode::LOP_POWK
            | OpCode::LOP_IDIVK
            | OpCode::LOP_SUBRK
            | OpCode::LOP_DIVRK
            | OpCode::LOP_AND
            | OpCode::LOP_OR
            | OpCode::LOP_ANDK
            | OpCode::LOP_ORK
            | OpCode::LOP_CONCAT
            | OpCode::LOP_NOT
            | OpCode::LOP_MINUS
            | OpCode::LOP_LENGTH
            | OpCode::LOP_NEWTABLE => Some((a, a)),
            OpCode::LOP_NAMECALL | OpCode::LOP_NAMECALLUDATA => Some((a, a.saturating_add(1))),
            OpCode::LOP_CALL | OpCode::LOP_CALLFB => {
                if c == 0 {
                    Some((a, u8::MAX))
                } else if c == 1 {
                    None
                } else {
                    Some((a, a.saturating_add(c - 2)))
                }
            }
            OpCode::LOP_GETVARARGS => {
                if b == 0 {
                    Some((a, u8::MAX))
                } else if b == 1 {
                    None
                } else {
                    Some((a, a.saturating_add(b - 2)))
                }
            }
            _ => None,
        },
        Instruction::AD { op_code, a, .. } => match op_code {
            OpCode::LOP_LOADN
            | OpCode::LOP_LOADK
            | OpCode::LOP_LOADKX
            | OpCode::LOP_GETIMPORT
            | OpCode::LOP_NEWCLOSURE
            | OpCode::LOP_DUPCLOSURE
            | OpCode::LOP_DUPTABLE => Some((a, a)),
            _ => None,
        },
        Instruction::E { .. } => None,
    }
}

/// Walks every `CALL` and searches backwards for the nearest instruction that
/// defines the register the `CALL` names as its function. Reports the distance
/// and the defining opcode, or that nothing in the prototype defines it.
pub fn audit_calls(
    bytecode: &[u8],
    encode_key: u8,
    only_suspicious: bool,
) -> Result<String, String> {
    let chunk = match deserializer::deserialize(bytecode, encode_key)? {
        Bytecode::Error(message) => return Err(format!("bytecode carries an error: {message}")),
        Bytecode::Chunk(chunk) => chunk,
    };

    let mut out = String::new();
    let mut total = 0usize;
    let mut reported = 0usize;

    for (proto_index, function) in chunk.functions.iter().enumerate() {
        // Real instructions only, with their word offsets.
        let mut real: Vec<(usize, Instruction)> = Vec::new();
        let mut pending_aux = false;
        for (offset, instruction) in function.instructions.iter().enumerate() {
            if pending_aux {
                pending_aux = false;
                continue;
            }
            pending_aux = instruction.op_code().has_aux();
            real.push((offset, *instruction));
        }

        for position in 0..real.len() {
            let (offset, instruction) = real[position];
            let Instruction::BC {
                op_code: op_code @ (OpCode::LOP_CALL | OpCode::LOP_CALLFB),
                a: call_register,
                ..
            } = instruction
            else {
                continue;
            };
            // A NAMECALL directly before a CALL supplies the function itself.
            if position > 0
                && matches!(
                    real[position - 1].1.op_code(),
                    OpCode::LOP_NAMECALL | OpCode::LOP_NAMECALLUDATA
                )
            {
                continue;
            }
            total += 1;

            let mut definer: Option<(usize, OpCode, u8)> = None;
            for back in (0..position).rev() {
                let (definer_offset, definer_instruction) = real[back];
                if let Some((low, high)) = defined_registers(definer_instruction)
                    && call_register >= low
                    && call_register <= high
                {
                    definer = Some((
                        definer_offset,
                        definer_instruction.op_code(),
                        (position - back) as u8,
                    ));
                    break;
                }
            }

            let call_mnemonic = format!("{op_code:?}");
            let call_mnemonic = call_mnemonic.strip_prefix("LOP_").unwrap_or(&call_mnemonic);
            match definer {
                Some((definer_offset, definer_op, distance)) => {
                    if !only_suspicious {
                        reported += 1;
                        let mnemonic = format!("{definer_op:?}");
                        let _ = writeln!(
                            out,
                            "proto {proto_index} @{offset} {call_mnemonic} func=R{call_register}  <- defined @{definer_offset} by {} (distance {distance})",
                            mnemonic.strip_prefix("LOP_").unwrap_or(&mnemonic)
                        );
                    }
                }
                None => {
                    reported += 1;
                    let _ = writeln!(
                        out,
                        "proto {proto_index} @{offset} {call_mnemonic} func=R{call_register}  <- UNDEFINED: no earlier instruction in this prototype writes R{call_register}{}",
                        if (call_register as usize) < function.num_parameters as usize {
                            " (but it is a parameter)"
                        } else {
                            ""
                        }
                    );
                }
            }
        }
    }
    let _ = writeln!(out, "-- {reported} reported of {total} non-NAMECALL CALL sites");
    Ok(out)
}
