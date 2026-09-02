//! Read-only textual dump of deserialized Lua 5.1 bytecode.

use std::{
    collections::BTreeSet,
    fmt::{self, Write as _},
};

use either::Either;
use lua51_deserializer::{
    DeserializeError, Function, Instruction, Value,
    argument::{Constant, Register, RegisterOrConstant},
};

const MAX_DISASSEMBLY_BYTES: usize = 64 * 1024 * 1024;
const MAX_RENDERED_STRING_BYTES: usize = 256;
const MAX_LIVE_LOCALS_PER_INSTRUCTION: usize = 64;

#[derive(Debug)]
pub enum DisassembleError {
    Deserialize(DeserializeError),
    PrototypeOutOfRange { index: usize, count: usize },
    OutputLimit { limit: usize },
    OutputAllocation,
}

impl fmt::Display for DisassembleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Deserialize(error) => error.fmt(formatter),
            Self::PrototypeOutOfRange { index, count } => write!(
                formatter,
                "prototype {index} is out of range; the chunk has {count}"
            ),
            Self::OutputLimit { limit } => write!(
                formatter,
                "disassembly output exceeds the {} MiB limit; select prototypes with --proto or use --list",
                limit / (1024 * 1024)
            ),
            Self::OutputAllocation => formatter.write_str("unable to allocate disassembly output"),
        }
    }
}

impl std::error::Error for DisassembleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Deserialize(error) => Some(error),
            _ => None,
        }
    }
}

impl From<DeserializeError> for DisassembleError {
    fn from(error: DeserializeError) -> Self {
        Self::Deserialize(error)
    }
}

#[derive(Clone, Copy)]
enum OutputFailure {
    Limit,
    Allocation,
}

struct BoundedOutput {
    text: String,
    failure: Option<OutputFailure>,
    limit: usize,
}

impl BoundedOutput {
    fn new() -> Self {
        Self {
            text: String::new(),
            failure: None,
            limit: MAX_DISASSEMBLY_BYTES,
        }
    }

    #[cfg(test)]
    fn with_limit(limit: usize) -> Self {
        Self {
            text: String::new(),
            failure: None,
            limit,
        }
    }

    fn error(&self) -> DisassembleError {
        match self.failure.unwrap_or(OutputFailure::Allocation) {
            OutputFailure::Limit => DisassembleError::OutputLimit { limit: self.limit },
            OutputFailure::Allocation => DisassembleError::OutputAllocation,
        }
    }

    fn finish(self) -> Result<String, DisassembleError> {
        match self.failure {
            Some(OutputFailure::Limit) => Err(DisassembleError::OutputLimit { limit: self.limit }),
            Some(OutputFailure::Allocation) => Err(DisassembleError::OutputAllocation),
            None => Ok(self.text),
        }
    }
}

impl fmt::Write for BoundedOutput {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if self.failure.is_some() {
            return Err(fmt::Error);
        }
        let Some(new_length) = self.text.len().checked_add(value.len()) else {
            self.failure = Some(OutputFailure::Limit);
            return Err(fmt::Error);
        };
        if new_length > self.limit {
            self.failure = Some(OutputFailure::Limit);
            return Err(fmt::Error);
        }
        if self.text.try_reserve(value.len()).is_err() {
            self.failure = Some(OutputFailure::Allocation);
            return Err(fmt::Error);
        }
        self.text.push_str(value);
        Ok(())
    }
}

/// Selects which prototypes a dump covers.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ProtoSelection {
    All,
    Only(Vec<usize>),
}

struct Prototype<'function, 'bytecode> {
    function: &'function Function<'bytecode>,
    parent: Option<usize>,
    sibling_index: usize,
    children: Vec<usize>,
}

fn collect_prototypes<'function, 'bytecode>(
    function: &'function Function<'bytecode>,
    parent: Option<usize>,
    sibling_index: usize,
    output: &mut Vec<Prototype<'function, 'bytecode>>,
) -> Result<usize, DisassembleError> {
    let index = output.len();
    output
        .try_reserve(1)
        .map_err(|_| DisassembleError::OutputAllocation)?;
    output.push(Prototype {
        function,
        parent,
        sibling_index,
        children: Vec::new(),
    });

    let mut children = Vec::new();
    children
        .try_reserve_exact(function.closures.len())
        .map_err(|_| DisassembleError::OutputAllocation)?;
    for (child_index, child) in function.closures.iter().enumerate() {
        children.push(collect_prototypes(child, Some(index), child_index, output)?);
    }
    output[index].children = children;
    Ok(index)
}

fn prototypes<'function, 'bytecode>(
    function: &'function Function<'bytecode>,
) -> Result<Vec<Prototype<'function, 'bytecode>>, DisassembleError> {
    let mut output = Vec::new();
    collect_prototypes(function, None, 0, &mut output)?;
    Ok(output)
}

fn write_prototype_path(
    output: &mut BoundedOutput,
    prototypes: &[Prototype<'_, '_>],
    index: usize,
) -> Result<(), DisassembleError> {
    let prototype = &prototypes[index];
    if let Some(parent) = prototype.parent {
        write_prototype_path(output, prototypes, parent)?;
        write!(output, ".{}", prototype.sibling_index).map_err(|_| output.error())
    } else {
        output.write_str("0").map_err(|_| output.error())
    }
}

fn escape_bytes(bytes: &[u8]) -> String {
    let shown = &bytes[..bytes.len().min(MAX_RENDERED_STRING_BYTES)];
    let mut output = String::with_capacity(shown.len().saturating_mul(4).saturating_add(48));
    output.push('"');
    for &byte in shown {
        match byte {
            b'"' => output.push_str("\\\""),
            b'\\' => output.push_str("\\\\"),
            b'\n' => output.push_str("\\n"),
            b'\r' => output.push_str("\\r"),
            b'\t' => output.push_str("\\t"),
            0x20..=0x7e => output.push(char::from(byte)),
            other => {
                let _ = write!(output, "\\x{other:02x}");
            }
        }
    }
    if shown.len() != bytes.len() {
        let _ = write!(output, "...<{} bytes omitted>", bytes.len() - shown.len());
    }
    output.push('"');
    output
}

fn function_name(function: &Function<'_>) -> String {
    let name = function.name.strip_suffix(&[0]).unwrap_or(function.name);
    if name.is_empty() {
        "<anonymous>".to_owned()
    } else {
        escape_bytes(name)
    }
}

fn constant_at(function: &Function<'_>, index: usize) -> String {
    let Some(constant) = function.constants.get(index) else {
        return format!(
            "<K{index} out of range, {} constants>",
            function.constants.len()
        );
    };
    match constant {
        Value::Nil => "nil".to_owned(),
        Value::Boolean(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => escape_bytes(value),
    }
}

fn register(register: &Register) -> String {
    format!("R{}", register.0)
}

fn registers(registers: &[Register]) -> String {
    registers
        .iter()
        .map(register)
        .collect::<Vec<_>>()
        .join(", ")
}

fn constant(function: &Function<'_>, constant: &Constant) -> String {
    format!(
        "K{}={}",
        constant.0,
        constant_at(function, constant.0 as usize)
    )
}

fn register_or_constant(function: &Function<'_>, value: &RegisterOrConstant) -> String {
    match &value.0 {
        Either::Left(value) => register(value),
        Either::Right(value) => constant(function, value),
    }
}

fn variable_count(encoded: u8) -> String {
    if encoded == 0 {
        "MULTRET".to_owned()
    } else {
        (encoded - 1).to_string()
    }
}

fn jump_target(offset: usize, skip: i32) -> i64 {
    offset as i64 + 1 + i64::from(skip)
}

fn floating_byte_size(encoded: u16) -> String {
    if encoded < 8 {
        return encoded.to_string();
    }
    let mantissa = u64::from((encoded & 7) + 8);
    let exponent = u32::from((encoded >> 3) - 1);
    1u64.checked_shl(exponent)
        .and_then(|scale| mantissa.checked_mul(scale))
        .map_or_else(|| "overflow".to_owned(), |size| size.to_string())
}

fn instruction_text(
    function: &Function<'_>,
    child_prototypes: &[usize],
    offset: usize,
    instruction: &Instruction,
) -> (&'static str, String) {
    match instruction {
        Instruction::Move {
            destination,
            source,
        } => (
            "MOVE",
            format!("{} <- {}", register(destination), register(source)),
        ),
        Instruction::LoadConstant {
            destination,
            source,
        } => (
            "LOADK",
            format!(
                "{} <- {}",
                register(destination),
                constant(function, source)
            ),
        ),
        Instruction::LoadBoolean {
            destination,
            value,
            skip_next,
        } => (
            "LOADBOOL",
            format!("{} <- {value} skip_next={skip_next}", register(destination)),
        ),
        Instruction::LoadNil(values) => ("LOADNIL", registers(values)),
        Instruction::GetUpvalue {
            destination,
            upvalue,
        } => (
            "GETUPVAL",
            format!("{} <- U{}", register(destination), upvalue.0),
        ),
        Instruction::GetGlobal {
            destination,
            global,
        } => (
            "GETGLOBAL",
            format!(
                "{} <- _G[{}]",
                register(destination),
                constant(function, global)
            ),
        ),
        Instruction::GetIndex {
            destination,
            object,
            key,
        } => (
            "GETTABLE",
            format!(
                "{} <- {}[{}]",
                register(destination),
                register(object),
                register_or_constant(function, key)
            ),
        ),
        Instruction::SetGlobal { destination, value } => (
            "SETGLOBAL",
            format!(
                "_G[{}] <- {}",
                constant(function, destination),
                register(value)
            ),
        ),
        Instruction::SetUpvalue {
            destination,
            source,
        } => (
            "SETUPVAL",
            format!("U{} <- {}", destination.0, register(source)),
        ),
        Instruction::SetIndex { object, key, value } => (
            "SETTABLE",
            format!(
                "{}[{}] <- {}",
                register(object),
                register_or_constant(function, key),
                register_or_constant(function, value)
            ),
        ),
        Instruction::NewTable {
            destination,
            array_size,
            hash_size,
        } => (
            "NEWTABLE",
            format!(
                "{} array_size={} hash_size={} encoded=({array_size}, {hash_size})",
                register(destination),
                floating_byte_size(*array_size),
                floating_byte_size(*hash_size)
            ),
        ),
        Instruction::PrepMethodCall {
            destination,
            self_arg,
            object,
            method,
        } => (
            "SELF",
            format!(
                "{} <- {}[{}] ; {} <- {}",
                register(destination),
                register(object),
                register_or_constant(function, method),
                register(self_arg),
                register(object)
            ),
        ),
        Instruction::Add {
            destination,
            lhs,
            rhs,
        }
        | Instruction::Sub {
            destination,
            lhs,
            rhs,
        }
        | Instruction::Mul {
            destination,
            lhs,
            rhs,
        }
        | Instruction::Div {
            destination,
            lhs,
            rhs,
        }
        | Instruction::Mod {
            destination,
            lhs,
            rhs,
        }
        | Instruction::Pow {
            destination,
            lhs,
            rhs,
        } => {
            let mnemonic = match instruction {
                Instruction::Add { .. } => "ADD",
                Instruction::Sub { .. } => "SUB",
                Instruction::Mul { .. } => "MUL",
                Instruction::Div { .. } => "DIV",
                Instruction::Mod { .. } => "MOD",
                Instruction::Pow { .. } => "POW",
                _ => unreachable!(),
            };
            (
                mnemonic,
                format!(
                    "{} <- {}, {}",
                    register(destination),
                    register_or_constant(function, lhs),
                    register_or_constant(function, rhs)
                ),
            )
        }
        Instruction::Minus {
            destination,
            operand,
        }
        | Instruction::Not {
            destination,
            operand,
        }
        | Instruction::Length {
            destination,
            operand,
        } => {
            let mnemonic = match instruction {
                Instruction::Minus { .. } => "UNM",
                Instruction::Not { .. } => "NOT",
                Instruction::Length { .. } => "LEN",
                _ => unreachable!(),
            };
            (
                mnemonic,
                format!("{} <- {}", register(destination), register(operand)),
            )
        }
        Instruction::Concatenate {
            destination,
            operands,
        } => (
            "CONCAT",
            format!("{} <- {}", register(destination), registers(operands)),
        ),
        Instruction::Jump(skip) => (
            "JMP",
            format!("skip={skip} target={}", jump_target(offset, *skip)),
        ),
        Instruction::Equal { lhs, rhs, invert }
        | Instruction::LessThan { lhs, rhs, invert }
        | Instruction::LessThanOrEqual { lhs, rhs, invert } => {
            let mnemonic = match instruction {
                Instruction::Equal { .. } => "EQ",
                Instruction::LessThan { .. } => "LT",
                Instruction::LessThanOrEqual { .. } => "LE",
                _ => unreachable!(),
            };
            (
                mnemonic,
                format!(
                    "{}, {} invert={invert}",
                    register_or_constant(function, lhs),
                    register_or_constant(function, rhs)
                ),
            )
        }
        Instruction::Test { value, invert } => {
            ("TEST", format!("{} invert={invert}", register(value)))
        }
        Instruction::TestSet {
            destination,
            value,
            invert,
        } => (
            "TESTSET",
            format!(
                "{} <- {} invert={invert}",
                register(destination),
                register(value)
            ),
        ),
        Instruction::Call {
            function,
            arguments,
            return_values,
        } => (
            "CALL",
            format!(
                "func={} nargs={} nresults={}",
                register(function),
                variable_count(*arguments),
                variable_count(*return_values)
            ),
        ),
        Instruction::TailCall {
            function,
            arguments,
        } => (
            "TAILCALL",
            format!(
                "func={} nargs={}",
                register(function),
                variable_count(*arguments)
            ),
        ),
        Instruction::Return(start, count) => (
            "RETURN",
            format!("start={} count={}", register(start), variable_count(*count)),
        ),
        Instruction::IterateNumericForLoop { control, skip } => (
            "FORLOOP",
            format!(
                "control=[{}] skip={skip} target={}",
                registers(control),
                jump_target(offset, *skip)
            ),
        ),
        Instruction::InitNumericForLoop { control, skip } => (
            "FORPREP",
            format!(
                "control=[{}] skip={skip} target={}",
                registers(control),
                jump_target(offset, *skip)
            ),
        ),
        Instruction::IterateGenericForLoop {
            generator,
            state,
            internal_control,
            vars,
        } => (
            "TFORLOOP",
            format!(
                "generator={} state={} control={} vars=[{}]",
                register(generator),
                register(state),
                register(internal_control),
                registers(vars)
            ),
        ),
        Instruction::SetList {
            table,
            number_of_elements,
            block_number,
        } => (
            "SETLIST",
            format!(
                "table={} count={} block={block_number}",
                register(table),
                if *number_of_elements == 0 {
                    "MULTRET".to_owned()
                } else {
                    number_of_elements.to_string()
                }
            ),
        ),
        Instruction::ExtraWord(word) => ("EXTRAWORD", format!("0x{word:08x}")),
        Instruction::Close(register_value) => ("CLOSE", format!("{}..", register(register_value))),
        Instruction::Closure {
            destination,
            function: child,
        } => {
            let child_index = child.0 as usize;
            let target = child_prototypes
                .get(child_index)
                .map(|index| format!("proto {index}"))
                .unwrap_or_else(|| {
                    format!(
                        "child[{child_index}] OUT OF RANGE ({} children)",
                        child_prototypes.len()
                    )
                });
            ("CLOSURE", format!("{} <- {target}", register(destination)))
        }
        Instruction::VarArg(start, count) => (
            "VARARG",
            format!("start={} count={}", register(start), variable_count(*count)),
        ),
    }
}

fn source_line(function: &Function<'_>, offset: usize) -> Option<u32> {
    function
        .positions
        .get(offset)
        .filter(|position| position.instruction == offset)
        .map(|position| position.source)
}

struct LocalTimeline<'function, 'bytecode> {
    function: &'function Function<'bytecode>,
    starts: Vec<(u32, usize)>,
    ends: Vec<(u32, usize)>,
    next_start: usize,
    next_end: usize,
    active: BTreeSet<usize>,
    omitted_active: usize,
}

impl<'function, 'bytecode> LocalTimeline<'function, 'bytecode> {
    fn new(function: &'function Function<'bytecode>) -> Result<Self, DisassembleError> {
        let mut starts = Vec::new();
        let mut ends = Vec::new();
        starts
            .try_reserve_exact(function.locals.len())
            .map_err(|_| DisassembleError::OutputAllocation)?;
        ends.try_reserve_exact(function.locals.len())
            .map_err(|_| DisassembleError::OutputAllocation)?;
        for (index, local) in function
            .locals
            .iter()
            .enumerate()
            .filter(|(_, local)| local.range.start < local.range.end)
        {
            starts.push((local.range.start, index));
            ends.push((local.range.end, index));
        }
        starts.sort_unstable();
        ends.sort_unstable();
        Ok(Self {
            function,
            starts,
            ends,
            next_start: 0,
            next_end: 0,
            active: BTreeSet::new(),
            omitted_active: 0,
        })
    }

    fn at(&mut self, offset: usize) -> String {
        let offset = u32::try_from(offset).unwrap_or(u32::MAX);
        while let Some(&(end, index)) = self.ends.get(self.next_end)
            && end <= offset
        {
            if !self.active.remove(&index) {
                self.omitted_active = self.omitted_active.saturating_sub(1);
            }
            self.next_end += 1;
        }
        while let Some(&(start, index)) = self.starts.get(self.next_start)
            && start <= offset
        {
            if self.function.locals[index].range.contains(&offset) {
                if self.active.len() < MAX_LIVE_LOCALS_PER_INSTRUCTION {
                    self.active.insert(index);
                } else {
                    self.omitted_active = self.omitted_active.saturating_add(1);
                }
            }
            self.next_start += 1;
        }
        if self.active.is_empty() && self.omitted_active == 0 {
            return String::new();
        }
        let live = self
            .active
            .iter()
            .take(MAX_LIVE_LOCALS_PER_INSTRUCTION)
            .map(|&index| escape_bytes(self.function.locals[index].name))
            .collect::<Vec<_>>();
        let suffix = if self.omitted_active == 0 {
            String::new()
        } else {
            format!(", ... ({} more)", self.omitted_active)
        };
        format!("  ; live locals {}{suffix}", live.join(", "))
    }
}

fn debug_upvalue_names(function: &Function<'_>) -> String {
    const MAX_DEBUG_UPVALUE_NAMES: usize = 256;
    let names = function
        .upvalues
        .iter()
        .take(MAX_DEBUG_UPVALUE_NAMES)
        .enumerate()
        .map(|(index, name)| format!("U{index}={}", escape_bytes(name)))
        .collect::<Vec<_>>();
    let omitted = function.upvalues.len().saturating_sub(names.len());
    if omitted == 0 {
        names.join(", ")
    } else {
        format!("{}, ... ({omitted} more)", names.join(", "))
    }
}

fn write_prototype(
    output: &mut BoundedOutput,
    prototypes: &[Prototype<'_, '_>],
    index: usize,
    show_locals: bool,
) -> Result<(), DisassembleError> {
    let prototype = &prototypes[index];
    let function = prototype.function;
    let name = function_name(function);
    write!(
        output,
        "\n-- proto {index}{} path=",
        if index == 0 { " (MAIN)" } else { "" },
    )
    .map_err(|_| output.error())?;
    write_prototype_path(output, prototypes, index)?;
    writeln!(
        output,
        " parent={} name={name}",
        prototype
            .parent
            .map_or_else(|| "none".to_owned(), |parent| parent.to_string())
    )
    .map_err(|_| output.error())?;
    writeln!(
        output,
        "   params={} upvalues={} vararg=0x{:02x} maxstacksize={} instructions={} constants={} children={} lines={}..{}",
        function.number_of_parameters,
        function.number_of_upvalues,
        function.vararg_flag,
        function.maximum_stack_size,
        function.code.len(),
        function.constants.len(),
        function.closures.len(),
        function.line_defined,
        function.last_line_defined
    )
    .map_err(|_| output.error())?;
    if !function.upvalues.is_empty() {
        writeln!(
            output,
            "   debug upvalue names: {}",
            debug_upvalue_names(function)
        )
        .map_err(|_| output.error())?;
    }
    if !prototype.children.is_empty() {
        writeln!(output, "   child protos: {:?}", prototype.children)
            .map_err(|_| output.error())?;
    }

    let mut local_timeline = if show_locals {
        Some(LocalTimeline::new(function)?)
    } else {
        None
    };
    for (offset, instruction) in function.code.iter().enumerate() {
        let (mnemonic, operands) =
            instruction_text(function, &prototype.children, offset, instruction);
        let line = source_line(function, offset)
            .map_or_else(|| "[     ]".to_owned(), |line| format!("[{line:>5}]"));
        let locals = local_timeline
            .as_mut()
            .map_or_else(String::new, |timeline| timeline.at(offset));
        writeln!(
            output,
            "  {offset:5} {line} {mnemonic:<12} {operands}{locals}"
        )
        .map_err(|_| output.error())?;
    }
    Ok(())
}

fn write_prototype_selection(
    output: &mut BoundedOutput,
    prototypes: &[Prototype<'_, '_>],
    index: usize,
    show_locals: bool,
) -> Result<(), DisassembleError> {
    if index >= prototypes.len() {
        return Err(DisassembleError::PrototypeOutOfRange {
            index,
            count: prototypes.len(),
        });
    }
    write_prototype(output, prototypes, index, show_locals)
}

/// Dumps a whole chunk, or selected depth-first prototype indices, as text.
pub fn disassemble(
    bytecode: &[u8],
    selection: &ProtoSelection,
    show_locals: bool,
) -> Result<String, DisassembleError> {
    let chunk = lua51_deserializer::deserialize(bytecode)?;
    let prototypes = prototypes(&chunk.function)?;
    let mut output = BoundedOutput::new();
    writeln!(
        output,
        "== Lua 5.1 bytecode, {} prototypes, main = proto 0",
        prototypes.len()
    )
    .map_err(|_| output.error())?;
    match selection {
        ProtoSelection::All => {
            for index in 0..prototypes.len() {
                write_prototype_selection(&mut output, &prototypes, index, show_locals)?;
            }
        }
        ProtoSelection::Only(indices) => {
            for &index in indices {
                write_prototype_selection(&mut output, &prototypes, index, show_locals)?;
            }
        }
    }
    output.finish()
}

/// Renders one summary line per depth-first prototype index.
pub fn list_prototypes(bytecode: &[u8]) -> Result<String, DisassembleError> {
    let chunk = lua51_deserializer::deserialize(bytecode)?;
    let prototypes = prototypes(&chunk.function)?;
    let mut output = BoundedOutput::new();
    writeln!(output, "== {} prototypes, main = proto 0", prototypes.len())
        .map_err(|_| output.error())?;
    for (index, prototype) in prototypes.iter().enumerate() {
        let function = prototype.function;
        let name = function_name(function);
        write!(output, "proto {index:5}  path=",).map_err(|_| output.error())?;
        write_prototype_path(&mut output, &prototypes, index)?;
        writeln!(
            output,
            " parent={:<5} insns={:6} params={:3} upvalues={:3} vararg=0x{:02x} maxstack={:3} line={:6} name={name}",
            prototype
                .parent
                .map_or_else(|| "-".to_owned(), |parent| parent.to_string()),
            function.code.len(),
            function.number_of_parameters,
            function.number_of_upvalues,
            function.vararg_flag,
            function.maximum_stack_size,
            function.line_defined
        )
        .map_err(|_| output.error())?;
    }
    output.finish()
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use lua51_deserializer::{Function, local::Local};

    use super::{
        BoundedOutput, DisassembleError, LocalTimeline, ProtoSelection, disassemble, escape_bytes,
        floating_byte_size, list_prototypes,
    };

    fn push_u32(output: &mut Vec<u8>, value: u32) {
        output.extend_from_slice(&value.to_le_bytes());
    }

    fn instruction_abx(opcode: u32, a: u32, bx: u32) -> u32 {
        opcode | (a << 6) | (bx << 14)
    }

    fn instruction_abc(opcode: u32, a: u32, b: u32, c: u32) -> u32 {
        opcode | (a << 6) | (c << 14) | (b << 23)
    }

    fn test_chunk() -> Vec<u8> {
        let mut output = vec![0x1b, b'L', b'u', b'a', 0x51, 0, 1, 4, 4, 4, 8, 0];
        push_u32(&mut output, 0);
        push_u32(&mut output, 0);
        push_u32(&mut output, 0);
        output.extend_from_slice(&[0, 0, 2, 2]);
        push_u32(&mut output, 2);
        push_u32(&mut output, instruction_abx(1, 0, 0));
        push_u32(&mut output, instruction_abc(30, 0, 2, 0));
        push_u32(&mut output, 1);
        output.push(4);
        push_u32(&mut output, 4);
        output.extend_from_slice(b"key\0");
        push_u32(&mut output, 0);
        push_u32(&mut output, 2);
        push_u32(&mut output, 10);
        push_u32(&mut output, 11);
        push_u32(&mut output, 1);
        push_u32(&mut output, 2);
        output.extend_from_slice(b"x\0");
        push_u32(&mut output, 0);
        push_u32(&mut output, 2);
        push_u32(&mut output, 0);
        output
    }

    fn nested_chunk() -> Vec<u8> {
        let mut output = vec![0x1b, b'L', b'u', b'a', 0x51, 0, 1, 4, 4, 4, 8, 0];
        push_u32(&mut output, 0);
        push_u32(&mut output, 0);
        push_u32(&mut output, 0);
        output.extend_from_slice(&[0, 0, 2, 2]);
        push_u32(&mut output, 2);
        push_u32(&mut output, instruction_abx(36, 0, 0));
        push_u32(&mut output, instruction_abc(30, 0, 1, 0));
        push_u32(&mut output, 0);
        push_u32(&mut output, 1);

        push_u32(&mut output, 0);
        push_u32(&mut output, 0);
        push_u32(&mut output, 0);
        output.extend_from_slice(&[0, 0, 0, 2]);
        push_u32(&mut output, 1);
        push_u32(&mut output, instruction_abc(30, 0, 1, 0));
        push_u32(&mut output, 0);
        push_u32(&mut output, 0);
        push_u32(&mut output, 0);
        push_u32(&mut output, 0);
        push_u32(&mut output, 0);

        push_u32(&mut output, 0);
        push_u32(&mut output, 0);
        push_u32(&mut output, 0);
        output
    }

    #[test]
    fn disassembly_resolves_constants_lines_and_live_locals() {
        let output = disassemble(&test_chunk(), &ProtoSelection::All, true).unwrap();

        assert!(output.contains("LOADK        R0 <- K0=\"key\""));
        assert!(output.contains("[   10]"));
        assert!(output.contains("live locals \"x\""));
    }

    #[test]
    fn prototype_listing_and_selection_validate_indices() {
        assert!(
            list_prototypes(&test_chunk())
                .unwrap()
                .contains("proto     0")
        );
        let error = disassemble(&test_chunk(), &ProtoSelection::Only(vec![1]), false).unwrap_err();
        assert!(matches!(
            error,
            DisassembleError::PrototypeOutOfRange { index: 1, count: 1 }
        ));
    }

    #[test]
    fn nested_prototypes_use_depth_first_indices_and_resolve_closures() {
        let output = disassemble(&nested_chunk(), &ProtoSelection::All, false).unwrap();

        assert!(output.contains("child protos: [1]"));
        assert!(output.contains("CLOSURE      R0 <- proto 1"));
        assert!(output.contains("-- proto 1 path=0.0 parent=0"));
    }

    #[test]
    fn table_size_operands_decode_lua_floating_bytes() {
        assert_eq!(floating_byte_size(7), "7");
        assert_eq!(floating_byte_size(8), "8");
        assert_eq!(floating_byte_size(16), "16");
        assert_eq!(floating_byte_size(255), "16106127360");
        assert_eq!(floating_byte_size(511), "overflow");
    }

    #[test]
    fn long_strings_are_previewed_instead_of_repeated_in_full() {
        let rendered = escape_bytes(&vec![b'a'; 300]);

        assert!(rendered.contains("44 bytes omitted"));
        assert!(rendered.len() < 320);
    }

    #[test]
    fn bounded_output_returns_a_typed_limit_error() {
        let mut output = BoundedOutput::with_limit(4);

        assert!(write!(output, "12345").is_err());
        assert!(matches!(
            output.finish(),
            Err(DisassembleError::OutputLimit { limit: 4 })
        ));
    }

    #[test]
    fn local_timeline_bounds_tracked_overlaps() {
        let mut locals = (0..65)
            .map(|_| Local {
                name: b"x".as_slice(),
                range: 0..2,
            })
            .collect::<Vec<_>>();
        locals.push(Local {
            name: b"invalid",
            range: 3..1,
        });
        let function = Function {
            name: b"",
            line_defined: 0,
            last_line_defined: 0,
            number_of_upvalues: 0,
            vararg_flag: 0,
            maximum_stack_size: 2,
            code: Vec::new(),
            constants: Vec::new(),
            closures: Vec::new(),
            positions: Vec::new(),
            locals,
            upvalues: Vec::new(),
            number_of_parameters: 0,
        };
        let mut timeline = LocalTimeline::new(&function).unwrap();

        assert!(timeline.at(0).contains("(1 more)"));
        assert_eq!(timeline.active.len(), 64);
        assert_eq!(timeline.omitted_active, 1);
        assert!(timeline.at(1).contains("(1 more)"));
        assert!(timeline.at(2).is_empty());
    }
}
