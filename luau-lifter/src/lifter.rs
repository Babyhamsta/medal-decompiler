use by_address::ByAddress;

use itertools::Itertools;
use parking_lot::Mutex;
use petgraph::stable_graph::NodeIndex;

use rustc_hash::FxHashMap;
use triomphe::Arc;

use super::{
    deserializer::{
        constant::Constant as BytecodeConstant,
        function::{DebugLocal, Function as BytecodeFunction},
    },
    error::{DecompileError, DecompilePhase},
    instruction::Instruction,
    op_code::OpCode,
};
use ast::{self};
use cfg::{
    block::{BlockEdge, BranchType},
    function::Function,
};

fn whole_function_debug_local(
    locals: &[DebugLocal],
    register: usize,
    instruction_count: usize,
) -> Option<&DebugLocal> {
    let mut candidates = locals.iter().filter(|local| {
        local.register as usize == register
            && local.start_pc == 0
            && local.end_pc == instruction_count
            && local.start_pc < local.end_pc
    });
    let local = candidates.next()?;
    candidates.next().is_none().then_some(local)
}

pub struct Lifter<'a> {
    function_list: &'a Vec<BytecodeFunction>,
    string_table: &'a Vec<Vec<u8>>,
    blocks: FxHashMap<usize, NodeIndex>,
    function: Function,
    child_functions: FxHashMap<ByAddress<Arc<Mutex<ast::Function>>>, usize>,
    register_map: FxHashMap<usize, ast::RcLocal>,
    constant_map: FxHashMap<usize, ast::Literal>,
    current_node: Option<NodeIndex>,
    upvalues: Vec<ast::RcLocal>,
}

impl<'a> Lifter<'a> {
    pub fn lift(
        f_list: &'a Vec<BytecodeFunction>,
        str_list: &'a Vec<Vec<u8>>,
        function_id: usize,
    ) -> Result<
        (
            Function,
            Vec<ast::RcLocal>,
            FxHashMap<ByAddress<Arc<Mutex<ast::Function>>>, usize>,
        ),
        DecompileError,
    > {
        let mut context = Self {
            function_list: f_list,
            string_table: str_list,
            blocks: FxHashMap::default(),
            function: Function::new(function_id),
            child_functions: FxHashMap::default(),
            register_map: FxHashMap::default(),
            constant_map: FxHashMap::default(),
            current_node: None,
            upvalues: Vec::new(),
        };

        context.lift_function()?;
        Ok((context.function, context.upvalues, context.child_functions))
    }

    /// Builds a [`DecompileError`] for the [`DecompilePhase::Lift`] phase, tagged with the
    /// prototype currently being lifted and, when known, the instruction that violated the
    /// invariant.
    fn lift_error(
        &self,
        instruction: Option<usize>,
        invariant: &'static str,
        detail: impl Into<String>,
    ) -> DecompileError {
        DecompileError::new(
            DecompilePhase::Lift,
            Some(self.function.id),
            instruction,
            invariant,
            detail,
        )
    }

    /// Resolves an upvalue operand against the prototype's declared upvalue list, returning an
    /// error instead of indexing out of bounds when the operand references an upvalue the
    /// prototype never declared.
    fn upvalue(&self, index: u8, instruction: usize) -> Result<ast::RcLocal, DecompileError> {
        self.upvalues.get(index as usize).cloned().ok_or_else(|| {
            self.lift_error(
                Some(instruction),
                "upvalue index within declared upvalues",
                format!(
                    "instruction references upvalue index {index} but the prototype declares {} upvalue(s)",
                    self.upvalues.len()
                ),
            )
        })
    }

    fn lift_function(&mut self) -> Result<(), DecompileError> {
        self.discover_blocks()?;

        let mut blocks = self.blocks.keys().cloned().collect::<Vec<_>>();

        blocks.sort_unstable();

        // TODO: code_ranges in lua51-lifter
        let instruction_count = self.function_list[self.function.id].instructions.len();
        if instruction_count == 0 {
            return Err(self.lift_error(
                None,
                "prototype has instructions",
                "the prototype's instruction stream is empty, so it has no entry block",
            ));
        }
        // Ranges are half-open. A boundary at exactly `instruction_count` is the implicit
        // end-of-function boundary that a branch or a trailing conditional's fallthrough can
        // name; it materialises as an empty terminal block. `jump_target` accepts the same
        // upper bound, so the two agree on what a reachable boundary is.
        let mut block_end = instruction_count;
        let mut block_ranges = Vec::with_capacity(blocks.len());
        for &block_start in blocks.iter().rev() {
            if block_start > instruction_count {
                return Err(self.lift_error(
                    None,
                    "block boundary within instruction stream",
                    format!(
                        "block starts at instruction {block_start} but the prototype only has \
                         {instruction_count} instruction(s)"
                    ),
                ));
            }
            block_ranges.push((block_start, block_end));
            block_end = if block_start != 0 { block_start } else { block_end };
        }

        for index in 0..self.function_list[self.function.id].num_upvalues as usize {
            let name = self.function_list[self.function.id]
                .debug_upvalues
                .get(index)
                .and_then(|name| self.debug_name(*name));
            let upvalue = ast::RcLocal::new(ast::Local::new(name));
            self.function.set_binding(
                upvalue.clone(),
                cfg::provenance::BindingIdentity::Upvalue {
                    function_id: self.function.id,
                    index,
                },
            );
            self.upvalues.push(upvalue);
        }

        for i in 0..self.function_list[self.function.id].num_parameters {
            let parameter = self.register(i as usize);
            self.function.parameters.push(parameter.clone());
        }

        self.function.is_variadic = self.function_list[self.function.id].is_vararg;

        for (start_pc, end_pc) in block_ranges {
            let node = self.block_to_node(start_pc)?;
            self.current_node = Some(node);
            // `end_pc` is exclusive: `start_pc == end_pc` is the empty terminal block.
            let (statements, origins, edges) = self.lift_block(start_pc, end_pc)?;
            let block = self.function.block_mut(node).unwrap();
            block.0.extend(statements);
            self.function.set_statement_origins(node, origins);
            self.function.set_edges(node, edges);
        }

        let entry_node = self.function.new_block();
        self.function.set_edges(
            entry_node,
            vec![(
                self.block_to_node(0)?,
                BlockEdge::new(BranchType::Unconditional),
            )],
        );
        self.function.set_entry(entry_node);
        Ok(())
    }

    /// Resolves a branch operand into an absolute instruction index, returning an error
    /// instead of over/underflowing when the offset would land before the first instruction
    /// or past the end of the prototype's instruction stream.
    ///
    /// A target of exactly `instruction_count` names the implicit end-of-function boundary.
    /// [`Lifter::lift_function`] accepts the same upper bound and materialises that boundary
    /// as an empty terminal block, so a target this returns is always a block a later
    /// [`Lifter::block_to_node`] can resolve.
    fn jump_target(
        &self,
        base: usize,
        offset: isize,
        instruction: usize,
    ) -> Result<usize, DecompileError> {
        let instruction_count = self.function_list[self.function.id].instructions.len();
        base.checked_add_signed(offset)
            .filter(|&target| target <= instruction_count)
            .ok_or_else(|| {
                self.lift_error(
                    Some(instruction),
                    "jump target within instruction stream",
                    format!(
                        "branch offset {offset} from instruction {base} does not land within \
                         0..={instruction_count}"
                    ),
                )
            })
    }

    fn discover_blocks(&mut self) -> Result<(), DecompileError> {
        self.blocks.insert(0, self.function.new_block());
        let instruction_count = self.function_list[self.function.id].instructions.len();
        for insn_index in 0..instruction_count {
            let insn = self.function_list[self.function.id].instructions[insn_index];
            match insn {
                Instruction::BC { op_code, c, .. } => match op_code {
                    OpCode::LOP_LOADB if c != 0 => {
                        let dest_index = self.jump_target(insn_index + 1, c.into(), insn_index)?;
                        self.blocks
                            .entry(dest_index)
                            .or_insert_with(|| self.function.new_block());
                    }
                    _ => {}
                },

                Instruction::AD {
                    op_code,
                    a: _,
                    d,
                    aux: _,
                } => match op_code {
                    OpCode::LOP_JUMP
                    | OpCode::LOP_JUMPBACK
                    | OpCode::LOP_JUMPIF
                    | OpCode::LOP_JUMPIFNOT
                    | OpCode::LOP_CMPPROTO => {
                        let dest_index = self.jump_target(insn_index + 1, d.into(), insn_index)?;
                        self.blocks
                            .entry(insn_index + 1)
                            .or_insert_with(|| self.function.new_block());
                        self.blocks
                            .entry(dest_index)
                            .or_insert_with(|| self.function.new_block());
                    }
                    OpCode::LOP_JUMPIFEQ
                    | OpCode::LOP_JUMPIFLE
                    | OpCode::LOP_JUMPIFLT
                    | OpCode::LOP_JUMPIFNOTEQ
                    | OpCode::LOP_JUMPIFNOTLE
                    | OpCode::LOP_JUMPIFNOTLT
                    | OpCode::LOP_JUMPXEQKNIL
                    | OpCode::LOP_JUMPXEQKB
                    | OpCode::LOP_JUMPXEQKN
                    | OpCode::LOP_JUMPXEQKS => {
                        let dest_index = self.jump_target(insn_index + 1, d.into(), insn_index)?;
                        self.blocks
                            .entry(insn_index + 2)
                            .or_insert_with(|| self.function.new_block());
                        self.blocks
                            .entry(dest_index)
                            .or_insert_with(|| self.function.new_block());
                    }
                    OpCode::LOP_FORNPREP => {
                        let dest_index = self.jump_target(insn_index + 1, d.into(), insn_index)?;
                        self.blocks
                            .entry(insn_index + 1)
                            .or_insert_with(|| self.function.new_block());
                        self.blocks
                            .entry(dest_index)
                            .or_insert_with(|| self.function.new_block());
                    }
                    OpCode::LOP_FORGPREP
                    | OpCode::LOP_FORGPREP_NEXT
                    | OpCode::LOP_FORGPREP_INEXT => {
                        let dest_index = self.jump_target(insn_index + 1, d.into(), insn_index)?;
                        self.blocks
                            .entry(insn_index + 1)
                            .or_insert_with(|| self.function.new_block());
                        self.blocks
                            .entry(dest_index)
                            .or_insert_with(|| self.function.new_block());
                    }
                    OpCode::LOP_FORNLOOP => {
                        let dest_index = self.jump_target(insn_index + 1, d.into(), insn_index)?;
                        self.blocks
                            .entry(insn_index)
                            .or_insert_with(|| self.function.new_block());
                        self.blocks
                            .entry(insn_index + 1)
                            .or_insert_with(|| self.function.new_block());
                        self.blocks
                            .entry(dest_index)
                            .or_insert_with(|| self.function.new_block());
                    }
                    OpCode::LOP_FORGLOOP => {
                        let dest_index = self.jump_target(insn_index + 1, d.into(), insn_index)?;
                        self.blocks
                            .entry(insn_index + 1)
                            .or_insert_with(|| self.function.new_block());
                        self.blocks
                            .entry(dest_index)
                            .or_insert_with(|| self.function.new_block());
                    }
                    _ => {}
                },

                Instruction::E { op_code, e } => {
                    if op_code == OpCode::LOP_JUMPX {
                        let dest_index =
                            self.jump_target(insn_index + 1, e as isize, insn_index)?;
                        self.blocks
                            .entry(insn_index + 1)
                            .or_insert_with(|| self.function.new_block());
                        self.blocks
                            .entry(dest_index)
                            .or_insert_with(|| self.function.new_block());
                    }
                }
            }
        }

        Ok(())
    }

    /// Lifts the half-open instruction range `block_start..block_end`. An empty range is
    /// legal and yields a block with no statements and no edges.
    fn lift_block(
        &mut self,
        block_start: usize,
        block_end: usize,
    ) -> Result<
        (
            Vec<ast::Statement>,
            Vec<cfg::provenance::OriginSet>,
            Vec<(NodeIndex, BlockEdge)>,
        ),
        DecompileError,
    > {
        let mut statements: Vec<ast::Statement> =
            Vec::with_capacity(block_end.saturating_sub(block_start));
        let mut origins: Vec<cfg::provenance::OriginSet> =
            Vec::with_capacity(statements.capacity());
        let mut edges = Vec::new();

        let mut top: Option<(ast::RValue, u8)> = None;

        let mut iter = self.function_list[self.function.id].instructions[block_start..block_end]
            .iter()
            .enumerate();

        while let Some((index, instruction)) = iter.next() {
            let instruction = *instruction;
            let mut emitted_origins = cfg::provenance::OriginSet::from([
                self.source_origin(block_start + index, instruction)
            ]);
            let mut origins_attached = false;
            let mut terminate = false;
            match instruction {
                Instruction::BC {
                    op_code,
                    a,
                    b,
                    c,
                    aux,
                } => match op_code {
                    // TODO: do we want to nil initialize all registers here?
                    OpCode::LOP_PREPVARARGS => {}
                    OpCode::LOP_MOVE => {
                        let a = self.register(a as _);
                        let b = self.register(b as _);
                        statements.push(ast::Assign::new(vec![a.into()], vec![b.into()]).into());
                    }
                    OpCode::LOP_GETUPVAL => {
                        let a = self.register(a as _);
                        let up = self.upvalue(b, block_start + index)?;
                        statements.push(ast::Assign::new(vec![a.into()], vec![up.into()]).into());
                    }
                    OpCode::LOP_SETUPVAL => {
                        let a = self.register(a as _);
                        let up = self.upvalue(b, block_start + index)?;
                        statements.push(ast::Assign::new(vec![up.into()], vec![a.into()]).into());
                    }
                    OpCode::LOP_LOADNIL => {
                        let target = self.register(a as _);
                        statements.push(
                            ast::Assign::new(vec![target.into()], vec![ast::Literal::Nil.into()])
                                .into(),
                        )
                    }
                    OpCode::LOP_LOADB => {
                        let target = self.register(a as _);
                        statements.push(
                            ast::Assign::new(
                                vec![target.into()],
                                vec![ast::Literal::Boolean(b != 0).into()],
                            )
                            .into(),
                        );
                        if c != 0 {
                            edges.push((
                                self.block_to_node(block_start + index + 2)?,
                                BlockEdge::new(BranchType::Unconditional),
                            ));
                        }
                    }
                    OpCode::LOP_NEWTABLE => {
                        statements.push(
                            ast::Assign::new(
                                vec![self.register(a as _).into()],
                                vec![ast::Table::default().into()],
                            )
                            .into(),
                        );
                    }
                    OpCode::LOP_GETGLOBAL => {
                        let value = self.register(a as _);
                        let global_name = self.constant_bytes(aux as _, block_start + index)?;
                        statements.push(
                            ast::Assign::new(
                                vec![value.into()],
                                vec![ast::Global::new(global_name).into()],
                            )
                            .into(),
                        );
                    }
                    OpCode::LOP_SETGLOBAL => {
                        let value = self.register(a as _);
                        let global_name = self.constant_bytes(aux as _, block_start + index)?;
                        statements.push(
                            ast::Assign::new(
                                vec![ast::Global::new(global_name).into()],
                                vec![value.into()],
                            )
                            .into(),
                        );
                    }
                    OpCode::LOP_GETTABLE => {
                        let target = self.register(a as _);
                        let table = self.register(b as _);
                        let key = self.register(c as _);
                        statements.push(
                            ast::Assign::new(
                                vec![target.into()],
                                vec![ast::Index::new(table.into(), key.into()).into()],
                            )
                            .into(),
                        );
                    }
                    OpCode::LOP_GETTABLEKS | OpCode::LOP_GETUDATAKS => {
                        let target = self.register(a as _);
                        let table = self.register(b as _);
                        let key_index = if op_code == OpCode::LOP_GETUDATAKS {
                            aux & 0xffff
                        } else {
                            aux
                        };
                        let key = self.constant(key_index as _, block_start + index)?;
                        statements.push(
                            ast::Assign::new(
                                vec![target.into()],
                                vec![ast::Index::new(table.into(), key.into()).into()],
                            )
                            .into(),
                        );
                    }
                    OpCode::LOP_GETTABLEN => {
                        let value = self.register(a as _);
                        let table = self.register(b as _);
                        let key = ast::Literal::Number((c as usize + 1) as f64);
                        statements.push(
                            ast::Assign::new(
                                vec![value.into()],
                                vec![ast::Index::new(table.into(), key.into()).into()],
                            )
                            .into(),
                        );
                    }
                    OpCode::LOP_SETTABLE => {
                        let value = self.register(a as _);
                        let table = self.register(b as _);
                        let key = self.register(c as _);
                        statements.push(
                            ast::Assign::new(
                                vec![ast::Index::new(table.into(), key.into()).into()],
                                vec![value.into()],
                            )
                            .into(),
                        );
                    }
                    OpCode::LOP_SETTABLEKS | OpCode::LOP_SETUDATAKS => {
                        let value = self.register(a as _);
                        let table = self.register(b as _);
                        let key_index = if op_code == OpCode::LOP_SETUDATAKS {
                            aux & 0xffff
                        } else {
                            aux
                        };
                        let key = self.constant(key_index as _, block_start + index)?;
                        statements.push(
                            ast::Assign::new(
                                vec![ast::Index::new(table.into(), key.into()).into()],
                                vec![value.into()],
                            )
                            .into(),
                        );
                    }
                    OpCode::LOP_SETTABLEN => {
                        let value = self.register(a as _);
                        let table = self.register(b as _);
                        let key = ast::Literal::Number((c as usize + 1) as f64);
                        statements.push(
                            ast::Assign::new(
                                vec![ast::Index::new(table.into(), key.into()).into()],
                                vec![value.into()],
                            )
                            .into(),
                        );
                    }
                    OpCode::LOP_ADD
                    | OpCode::LOP_SUB
                    | OpCode::LOP_MUL
                    | OpCode::LOP_DIV
                    | OpCode::LOP_MOD
                    | OpCode::LOP_POW
                    | OpCode::LOP_IDIV => {
                        let op = match op_code {
                            OpCode::LOP_ADD => ast::BinaryOperation::Add,
                            OpCode::LOP_SUB => ast::BinaryOperation::Sub,
                            OpCode::LOP_MUL => ast::BinaryOperation::Mul,
                            OpCode::LOP_DIV => ast::BinaryOperation::Div,
                            OpCode::LOP_MOD => ast::BinaryOperation::Mod,
                            OpCode::LOP_POW => ast::BinaryOperation::Pow,
                            OpCode::LOP_IDIV => ast::BinaryOperation::IDiv,
                            _ => unreachable!(),
                        };
                        let target = self.register(a as _);
                        let left = self.register(b as _);
                        let right = self.register(c as _);
                        statements.push(
                            ast::Assign::new(
                                vec![target.into()],
                                vec![ast::Binary::new(left.into(), right.into(), op).into()],
                            )
                            .into(),
                        );
                    }
                    OpCode::LOP_ADDK
                    | OpCode::LOP_SUBK
                    | OpCode::LOP_MULK
                    | OpCode::LOP_DIVK
                    | OpCode::LOP_MODK
                    | OpCode::LOP_POWK
                    | OpCode::LOP_IDIVK => {
                        let op = match op_code {
                            OpCode::LOP_ADDK => ast::BinaryOperation::Add,
                            OpCode::LOP_SUBK => ast::BinaryOperation::Sub,
                            OpCode::LOP_MULK => ast::BinaryOperation::Mul,
                            OpCode::LOP_DIVK => ast::BinaryOperation::Div,
                            OpCode::LOP_MODK => ast::BinaryOperation::Mod,
                            OpCode::LOP_POWK => ast::BinaryOperation::Pow,
                            OpCode::LOP_IDIVK => ast::BinaryOperation::IDiv,
                            _ => unreachable!(),
                        };
                        let target = self.register(a as _);
                        let left = self.register(b as _);
                        let right = self.constant(c as _, block_start + index)?;
                        statements.push(
                            ast::Assign::new(
                                vec![target.into()],
                                vec![ast::Binary::new(left.into(), right.into(), op).into()],
                            )
                            .into(),
                        );
                    }
                    OpCode::LOP_NOT | OpCode::LOP_MINUS | OpCode::LOP_LENGTH => {
                        let op = match op_code {
                            OpCode::LOP_NOT => ast::UnaryOperation::Not,
                            OpCode::LOP_MINUS => ast::UnaryOperation::Negate,
                            OpCode::LOP_LENGTH => ast::UnaryOperation::Length,
                            _ => unreachable!(),
                        };
                        let target = self.register(a as _);
                        let value = self.register(b as _);
                        statements.push(
                            ast::Assign::new(
                                vec![target.into()],
                                vec![ast::Unary::new(value.into(), op).into()],
                            )
                            .into(),
                        );
                    }
                    OpCode::LOP_RETURN => {
                        let values = if b != 0 {
                            (a as usize..a as usize + (b as usize - 1))
                                .map(|r| self.register(r).into())
                                .collect()
                        } else {
                            let (tail, end) = self.take_open_result(&mut top, block_start + index)?;
                            (a as usize..end as usize)
                                .map(|r| self.register(r).into())
                                .chain(std::iter::once(tail))
                                .collect()
                        };
                        statements.push(ast::Return::new(values).into());
                        terminate = true;
                    }
                    OpCode::LOP_FASTCALL
                    | OpCode::LOP_FASTCALL1
                    | OpCode::LOP_FASTCALL2
                    | OpCode::LOP_FASTCALL2K
                    | OpCode::LOP_FASTCALL3 => {}
                    OpCode::LOP_NAMECALL | OpCode::LOP_NAMECALLUDATA => {
                        let namecall_base = a;
                        let namecall_object = self.register(b as _);
                        let key_index = if op_code == OpCode::LOP_NAMECALLUDATA {
                            aux & 0xffff
                        } else {
                            aux
                        };
                        let namecall_method_bytes =
                            self.constant_bytes(key_index as usize, block_start + index)?;
                        let namecall_method =
                            String::from_utf8(namecall_method_bytes).map_err(|error| {
                                self.lift_error(
                                    Some(block_start + index),
                                    "utf8 constant bytes",
                                    format!(
                                        "method name constant index {key_index} is not valid \
                                         UTF-8: {error}"
                                    ),
                                )
                            })?;
                        let (nop_index, nop_instruction) = iter.next().ok_or_else(|| {
                            self.lift_error(
                                Some(block_start + index),
                                "namecall followed by a nop placeholder",
                                "NAMECALL/NAMECALLUDATA is the last instruction in its block, \
                                 but the compiler always follows it with a NOP placeholder and \
                                 a CALL",
                            )
                        })?;
                        if !matches!(
                            nop_instruction,
                            Instruction::BC {
                                op_code: OpCode::LOP_NOP,
                                ..
                            }
                        ) {
                            return Err(self.lift_error(
                                Some(block_start + nop_index),
                                "namecall followed by a nop placeholder",
                                format!(
                                    "expected LOP_NOP after NAMECALL, found {nop_instruction:?}"
                                ),
                            ));
                        }
                        let (call_index, call_instruction) = iter.next().ok_or_else(|| {
                            self.lift_error(
                                Some(block_start + index),
                                "namecall followed by a call instruction",
                                "NAMECALL/NAMECALLUDATA's NOP placeholder is the last \
                                 instruction in its block, but a CALL or CALLFB must follow",
                            )
                        })?;
                        emitted_origins.insert(
                            self.source_origin(block_start + call_index, *call_instruction),
                        );
                        match call_instruction {
                            &Instruction::BC {
                                op_code: OpCode::LOP_CALL | OpCode::LOP_CALLFB,
                                a,
                                b,
                                c,
                                ..
                            } => {
                                if a != namecall_base {
                                    return Err(self.lift_error(
                                        Some(block_start + call_index),
                                        "namecall call base matches method receiver",
                                        format!(
                                            "CALL/CALLFB base register {a} does not match the \
                                             NAMECALL base register {namecall_base}"
                                        ),
                                    ));
                                }
                                // TODO: repeated code :(
                                let arguments = if b != 0 {
                                    (a as usize + 2..a as usize + b as usize)
                                        .map(|r| self.register(r).into())
                                        .collect()
                                } else {
                                    let top =
                                        self.take_open_result(&mut top, block_start + call_index)?;
                                    (a as usize + 2..top.1 as usize)
                                        .map(|r| self.register(r).into())
                                        .chain(std::iter::once(top.0))
                                        .collect()
                                };

                                // TODO: make sure `a:method with space()` doesnt happen
                                let call = ast::MethodCall::new(
                                    namecall_object.into(),
                                    namecall_method,
                                    arguments,
                                );

                                if c != 0 {
                                    if c == 1 {
                                        statements.push(call.into());
                                    } else {
                                        statements.push(
                                            ast::Assign::new(
                                                (a as usize..a as usize + c as usize - 1)
                                                    .map(|r| self.register(r).into())
                                                    .collect(),
                                                vec![ast::Select::MethodCall(call).into_rvalue(
                                                    ast::ResultDemand::Exact((c - 1) as usize),
                                                )],
                                            )
                                            .into(),
                                        );
                                    }
                                } else {
                                    top = Some((
                                        ast::Select::MethodCall(call)
                                            .into_rvalue(ast::ResultDemand::Open),
                                        a,
                                    ));
                                }
                            }
                            other => {
                                return Err(self.lift_error(
                                    Some(block_start + call_index),
                                    "namecall followed by a call instruction",
                                    format!(
                                        "expected LOP_CALL or LOP_CALLFB after NAMECALL's NOP, \
                                         found {other:?}"
                                    ),
                                ));
                            }
                        }
                    }
                    OpCode::LOP_CALL | OpCode::LOP_CALLFB => {
                        let arguments = if b != 0 {
                            (a as usize + 1..a as usize + b as usize)
                                .map(|r| self.register(r).into())
                                .collect()
                        } else {
                            let top = self.take_open_result(&mut top, block_start + index)?;
                            (a as usize + 1..top.1 as usize)
                                .map(|r| self.register(r).into())
                                .chain(std::iter::once(top.0))
                                .collect()
                        };

                        let call = ast::Call::new(self.register(a as _).into(), arguments);

                        if c != 0 {
                            if c == 1 {
                                statements.push(call.into());
                            } else {
                                statements.push(
                                    ast::Assign::new(
                                        (a as usize..a as usize + c as usize - 1)
                                            .map(|r| self.register(r).into())
                                            .collect(),
                                        vec![ast::Select::Call(call).into_rvalue(
                                            ast::ResultDemand::Exact((c - 1) as usize),
                                        )],
                                    )
                                    .into(),
                                );
                            }
                        } else {
                            top = Some((
                                ast::Select::Call(call).into_rvalue(ast::ResultDemand::Open),
                                a,
                            ));
                        }
                    }
                    OpCode::LOP_CLOSEUPVALS => {
                        let locals = (a..self.function_list[self.function.id].max_stack_size)
                            .map(|i| self.register(i as _))
                            .collect();
                        statements.push(ast::Close { locals }.into());
                    }
                    OpCode::LOP_NEWCLASSMEMBER => {
                        let class = self.register(a as _);
                        let value = self.register(c as _);
                        let member_name = self.constant_string(aux as _, block_start + index)?;
                        let inline_method = statements.last().and_then(|statement| {
                            let assign = statement.as_assign()?;
                            if assign.left.len() == 1
                                && assign.right.len() == 1
                                && matches!(
                                    &assign.left[0],
                                    ast::LValue::Local(local) if local == &value
                                )
                                && matches!(&assign.right[0], ast::RValue::Closure(_))
                            {
                                Some(assign.right[0].clone())
                            } else {
                                None
                            }
                        });
                        let method = if let Some(method) = inline_method {
                            statements.pop();
                            emitted_origins.extend(origins.pop().unwrap());
                            method
                        } else {
                            value.into()
                        };
                        if let Some(class_index) = statements.iter().rposition(|statement| {
                            statement
                                .as_class()
                                .is_some_and(|statement| statement.target == class)
                        }) {
                            let class_statement = statements[class_index].as_class_mut().unwrap();
                            class_statement.methods.push((member_name, method));
                            origins[class_index].extend(emitted_origins.clone());
                            origins_attached = true;
                        } else {
                            statements.push(
                                ast::Assign::new(
                                    vec![
                                        ast::Index::new(
                                            class.into(),
                                            ast::Literal::String(member_name.as_bytes().to_vec())
                                                .into(),
                                        )
                                        .into(),
                                    ],
                                    vec![method],
                                )
                                .into(),
                            );
                        }
                    }
                    OpCode::LOP_SETLIST => {
                        let setlist = if c != 0 {
                            ast::SetList::new(
                                self.register(a as _),
                                aux as usize,
                                (b as usize..b as usize + c as usize - 1)
                                    .map(|r| self.register(r).into())
                                    .collect(),
                                None,
                            )
                        } else {
                            let top = self.take_open_result(&mut top, block_start + index)?;
                            ast::SetList::new(
                                self.register(a as _).clone(),
                                aux as usize,
                                (b as usize..top.1 as usize)
                                    .map(|r| self.register(r).into())
                                    .collect(),
                                Some(top.0),
                            )
                        };
                        statements.push(setlist.into());
                    }
                    OpCode::LOP_CONCAT => {
                        let operands = (b..=c)
                            .map(|r| self.register(r as _))
                            .rev()
                            .collect::<Vec<_>>();
                        if operands.len() < 2 {
                            return Err(self.lift_error(
                                Some(block_start + index),
                                "concat operand range has at least two registers",
                                format!(
                                    "LOP_CONCAT operand range b..=c has {} register(s), \
                                     expected at least 2",
                                    operands.len()
                                ),
                            ));
                        }
                        let mut operands = operands.into_iter();
                        let right = operands.next().unwrap();
                        let left = operands.next().unwrap();
                        let mut concat = ast::Binary::new(
                            left.into(),
                            right.into(),
                            ast::BinaryOperation::Concat,
                        );
                        for r in operands {
                            concat = ast::Binary::new(
                                r.into(),
                                concat.into(),
                                ast::BinaryOperation::Concat,
                            );
                        }
                        statements.push(
                            ast::Assign::new(
                                vec![self.register(a as _).into()],
                                vec![concat.into()],
                            )
                            .into(),
                        );
                    }
                    OpCode::LOP_AND => statements.push(
                        ast::Assign::new(
                            vec![self.register(a as _).into()],
                            vec![
                                ast::Binary::new(
                                    self.register(b as _).into(),
                                    self.register(c as _).into(),
                                    ast::BinaryOperation::And,
                                )
                                .into(),
                            ],
                        )
                        .into(),
                    ),
                    OpCode::LOP_ANDK => statements.push(
                        ast::Assign::new(
                            vec![self.register(a as _).into()],
                            vec![
                                ast::Binary::new(
                                    self.register(b as _).into(),
                                    self.constant(c as _, block_start + index)?.into(),
                                    ast::BinaryOperation::And,
                                )
                                .into(),
                            ],
                        )
                        .into(),
                    ),
                    OpCode::LOP_OR => statements.push(
                        ast::Assign::new(
                            vec![self.register(a as _).into()],
                            vec![
                                ast::Binary::new(
                                    self.register(b as _).into(),
                                    self.register(c as _).into(),
                                    ast::BinaryOperation::Or,
                                )
                                .into(),
                            ],
                        )
                        .into(),
                    ),
                    OpCode::LOP_ORK => statements.push(
                        ast::Assign::new(
                            vec![self.register(a as _).into()],
                            vec![
                                ast::Binary::new(
                                    self.register(b as _).into(),
                                    self.constant(c as _, block_start + index)?.into(),
                                    ast::BinaryOperation::Or,
                                )
                                .into(),
                            ],
                        )
                        .into(),
                    ),
                    OpCode::LOP_GETVARARGS => {
                        let vararg = ast::VarArg {};
                        if b > 1 {
                            statements.push(
                                ast::Assign::new(
                                    (a as usize..a as usize + b as usize - 1)
                                        .map(|r| self.register(r).into())
                                        .collect(),
                                    vec![
                                        ast::Select::VarArg(vararg).into_rvalue(
                                            ast::ResultDemand::Exact((b - 1) as usize),
                                        ),
                                    ],
                                )
                                .into(),
                            );
                        } else if b == 0 {
                            top = Some((
                                ast::Select::VarArg(vararg).into_rvalue(ast::ResultDemand::Open),
                                a,
                            ));
                        }
                    }
                    OpCode::LOP_NOP => {}
                    OpCode::LOP_SUBRK | OpCode::LOP_DIVRK => {
                        let op = match op_code {
                            OpCode::LOP_SUBRK => ast::BinaryOperation::Sub,
                            OpCode::LOP_DIVRK => ast::BinaryOperation::Div,
                            _ => unreachable!(),
                        };
                        let target = self.register(a as _);
                        let left = self.constant(b as _, block_start + index)?;
                        let right = self.register(c as _);
                        statements.push(
                            ast::Assign::new(
                                vec![target.into()],
                                vec![ast::Binary::new(left.into(), right.into(), op).into()],
                            )
                            .into(),
                        );
                    }
                    _ => {
                        return Err(self.lift_error(
                            Some(block_start + index),
                            "supported opcode",
                            format!(
                                "{:?} is not a supported instruction in the ABC encoding",
                                instruction.op_code()
                            ),
                        ));
                    }
                },
                Instruction::AD { op_code, a, d, aux } => match op_code {
                    OpCode::LOP_LOADK => {
                        let constant = self.constant(d as _, block_start + index)?;
                        let target = self.register(a as _);
                        let statement =
                            ast::Assign::new(vec![target.into()], vec![constant.into()]);
                        statements.push(statement.into());
                    }
                    OpCode::LOP_LOADKX => {
                        let target = self.register(a as _);
                        let loadkx_constant = self.function_list[self.function.id]
                            .constants
                            .get(aux as usize)
                            .cloned()
                            .ok_or_else(|| {
                                self.lift_error(
                                    Some(block_start + index),
                                    "constant index within declared constants",
                                    format!(
                                        "LOADKX references constant index {aux} but the \
                                         prototype declares {} constant(s)",
                                        self.function_list[self.function.id].constants.len()
                                    ),
                                )
                            })?;
                        match loadkx_constant {
                            BytecodeConstant::ClassShape {
                                class_name,
                                properties,
                                ..
                            } => {
                                let source_name =
                                    self.constant_string(class_name, block_start + index)?;
                                let properties = properties
                                    .iter()
                                    .map(|property| {
                                        self.constant_string(*property, block_start + index)
                                    })
                                    .collect::<Result<_, _>>()?;
                                statements
                                    .push(ast::Class::new(target, source_name, properties).into());
                            }
                            _ => {
                                let constant = self.constant(aux as usize, block_start + index)?;
                                statements.push(
                                    ast::Assign::new(vec![target.into()], vec![constant.into()])
                                        .into(),
                                );
                            }
                        }
                    }
                    OpCode::LOP_LOADN => {
                        let target = self.register(a as _);
                        let statement = ast::Assign::new(
                            vec![target.into()],
                            vec![ast::Literal::Number(d as _).into()],
                        );
                        statements.push(statement.into());
                    }
                    OpCode::LOP_GETIMPORT => {
                        let target = self.register(a as _);
                        let import_len = (aux >> 30) & 3;
                        assert!(import_len <= 3);
                        let mut import_expression: ast::RValue = ast::Global::compiler_import(
                            self.constant_bytes(((aux >> 20) & 1023) as usize, block_start + index)?,
                        )
                        .into();
                        if import_len > 1 {
                            import_expression = ast::Index::new(
                                import_expression,
                                self.constant(((aux >> 10) & 1023) as usize, block_start + index)?
                                    .into(),
                            )
                            .into();
                        }
                        if import_len > 2 {
                            import_expression = ast::Index::new(
                                import_expression,
                                self.constant((aux & 1023) as usize, block_start + index)?.into(),
                            )
                            .into();
                        }
                        let assign = ast::Assign::new(vec![target.into()], vec![import_expression]);
                        statements.push(assign.into());
                    }
                    OpCode::LOP_JUMPIFNOT => {
                        let condition = self.register(a as _);
                        let statement = ast::If::new(
                            condition.into(),
                            ast::Block::default(),
                            ast::Block::default(),
                        );
                        edges.push((
                            self.block_to_node(block_start + index + 1)?,
                            BlockEdge::new(BranchType::Then),
                        ));
                        edges.push((
                            self.block_to_node(
                                ((block_start + index + 1) as isize + d as isize) as usize,
                            )?,
                            BlockEdge::new(BranchType::Else),
                        ));
                        statements.push(statement.into());
                    }
                    OpCode::LOP_JUMPIF => {
                        let condition = self.register(a as _);
                        let statement = ast::If::new(
                            condition.into(),
                            ast::Block::default(),
                            ast::Block::default(),
                        );
                        edges.push((
                            self.block_to_node(
                                ((block_start + index + 1) as isize + d as isize) as usize,
                            )?,
                            BlockEdge::new(BranchType::Then),
                        ));
                        edges.push((
                            self.block_to_node(block_start + index + 1)?,
                            BlockEdge::new(BranchType::Else),
                        ));
                        statements.push(statement.into());
                    }
                    OpCode::LOP_JUMPIFNOTEQ => {
                        let a = self.register(a as _);
                        let aux = self.register(aux as _);
                        statements.push(
                            ast::If::new(
                                ast::Binary::new(a.into(), aux.into(), ast::BinaryOperation::Equal)
                                    .into(),
                                ast::Block::default(),
                                ast::Block::default(),
                            )
                            .into(),
                        );
                        edges.push((
                            self.block_to_node(block_start + index + 2)?,
                            BlockEdge::new(BranchType::Then),
                        ));
                        edges.push((
                            self.block_to_node(
                                ((block_start + index + 1) as isize + d as isize) as usize,
                            )?,
                            BlockEdge::new(BranchType::Else),
                        ));
                    }
                    OpCode::LOP_JUMPIFNOTLE => {
                        let a = self.register(a as _);
                        let aux = self.register(aux as _);
                        statements.push(
                            ast::If::new(
                                ast::Binary::new(
                                    a.into(),
                                    aux.into(),
                                    ast::BinaryOperation::LessThanOrEqual,
                                )
                                .into(),
                                ast::Block::default(),
                                ast::Block::default(),
                            )
                            .into(),
                        );
                        edges.push((
                            self.block_to_node(block_start + index + 2)?,
                            BlockEdge::new(BranchType::Then),
                        ));
                        edges.push((
                            self.block_to_node(
                                ((block_start + index + 1) as isize + d as isize) as usize,
                            )?,
                            BlockEdge::new(BranchType::Else),
                        ));
                    }
                    OpCode::LOP_JUMPIFNOTLT => {
                        let a = self.register(a as _);
                        let aux = self.register(aux as _);
                        statements.push(
                            ast::If::new(
                                ast::Binary::new(
                                    a.into(),
                                    aux.into(),
                                    ast::BinaryOperation::LessThan,
                                )
                                .into(),
                                ast::Block::default(),
                                ast::Block::default(),
                            )
                            .into(),
                        );
                        edges.push((
                            self.block_to_node(block_start + index + 2)?,
                            BlockEdge::new(BranchType::Then),
                        ));
                        edges.push((
                            self.block_to_node(
                                ((block_start + index + 1) as isize + d as isize) as usize,
                            )?,
                            BlockEdge::new(BranchType::Else),
                        ));
                    }
                    OpCode::LOP_JUMPIFEQ => {
                        let a = self.register(a as _);
                        let aux = self.register(aux as _);
                        statements.push(
                            ast::If::new(
                                ast::Binary::new(a.into(), aux.into(), ast::BinaryOperation::Equal)
                                    .into(),
                                ast::Block::default(),
                                ast::Block::default(),
                            )
                            .into(),
                        );
                        edges.push((
                            self.block_to_node(
                                ((block_start + index + 1) as isize + d as isize) as usize,
                            )?,
                            BlockEdge::new(BranchType::Then),
                        ));
                        edges.push((
                            self.block_to_node(block_start + index + 2)?,
                            BlockEdge::new(BranchType::Else),
                        ));
                    }
                    OpCode::LOP_JUMPIFLE => {
                        let a = self.register(a as _);
                        let aux = self.register(aux as _);
                        statements.push(
                            ast::If::new(
                                ast::Binary::new(
                                    a.into(),
                                    aux.into(),
                                    ast::BinaryOperation::LessThanOrEqual,
                                )
                                .into(),
                                ast::Block::default(),
                                ast::Block::default(),
                            )
                            .into(),
                        );
                        edges.push((
                            self.block_to_node(
                                ((block_start + index + 1) as isize + d as isize) as usize,
                            )?,
                            BlockEdge::new(BranchType::Then),
                        ));
                        edges.push((
                            self.block_to_node(block_start + index + 2)?,
                            BlockEdge::new(BranchType::Else),
                        ));
                    }
                    OpCode::LOP_JUMPIFLT => {
                        let a = self.register(a as _);
                        let aux = self.register(aux as _);
                        statements.push(
                            ast::If::new(
                                ast::Binary::new(
                                    a.into(),
                                    aux.into(),
                                    ast::BinaryOperation::LessThan,
                                )
                                .into(),
                                ast::Block::default(),
                                ast::Block::default(),
                            )
                            .into(),
                        );
                        edges.push((
                            self.block_to_node(
                                ((block_start + index + 1) as isize + d as isize) as usize,
                            )?,
                            BlockEdge::new(BranchType::Then),
                        ));
                        edges.push((
                            self.block_to_node(block_start + index + 2)?,
                            BlockEdge::new(BranchType::Else),
                        ));
                    }
                    OpCode::LOP_JUMPBACK | OpCode::LOP_JUMP => {
                        edges.push((
                            self.block_to_node(
                                ((block_start + index + 1) as isize + d as isize) as usize,
                            )?,
                            BlockEdge::new(BranchType::Unconditional),
                        ));
                    }
                    OpCode::LOP_CMPPROTO => {
                        // CMPPROTO guards an optimized inlined path using a runtime-only
                        // prototype id. Luau source cannot express that identity test, so
                        // retain the generic fallback path that the guard jumps to.
                        edges.push((
                            self.block_to_node(
                                ((block_start + index + 1) as isize + d as isize) as usize,
                            )?,
                            BlockEdge::new(BranchType::Unconditional),
                        ));
                    }
                    OpCode::LOP_JUMPXEQKNIL => {
                        let a = self.register(a as _);
                        statements.push(
                            ast::If::new(
                                ast::Binary::new(
                                    a.into(),
                                    ast::Literal::Nil.into(),
                                    ast::BinaryOperation::Equal,
                                )
                                .into(),
                                ast::Block::default(),
                                ast::Block::default(),
                            )
                            .into(),
                        );
                        if aux & (1 << 31) != 0 {
                            edges.push((
                                self.block_to_node(
                                    ((block_start + index + 1) as isize + d as isize) as usize,
                                )?,
                                BlockEdge::new(BranchType::Else),
                            ));
                            edges.push((
                                self.block_to_node(block_start + index + 2)?,
                                BlockEdge::new(BranchType::Then),
                            ));
                        } else {
                            edges.push((
                                self.block_to_node(
                                    ((block_start + index + 1) as isize + d as isize) as usize,
                                )?,
                                BlockEdge::new(BranchType::Then),
                            ));
                            edges.push((
                                self.block_to_node(block_start + index + 2)?,
                                BlockEdge::new(BranchType::Else),
                            ));
                        }
                    }
                    OpCode::LOP_JUMPXEQKB => {
                        let a = self.register(a as _);
                        let literal = if aux & 1 != 0 {
                            ast::Literal::Boolean(true)
                        } else {
                            ast::Literal::Boolean(false)
                        };
                        statements.push(
                            ast::If::new(
                                ast::Binary::new(
                                    a.into(),
                                    literal.into(),
                                    ast::BinaryOperation::Equal,
                                )
                                .into(),
                                ast::Block::default(),
                                ast::Block::default(),
                            )
                            .into(),
                        );
                        if aux & (1 << 31) != 0 {
                            edges.push((
                                self.block_to_node(
                                    ((block_start + index + 1) as isize + d as isize) as usize,
                                )?,
                                BlockEdge::new(BranchType::Else),
                            ));
                            edges.push((
                                self.block_to_node(block_start + index + 2)?,
                                BlockEdge::new(BranchType::Then),
                            ));
                        } else {
                            edges.push((
                                self.block_to_node(
                                    ((block_start + index + 1) as isize + d as isize) as usize,
                                )?,
                                BlockEdge::new(BranchType::Then),
                            ));
                            edges.push((
                                self.block_to_node(block_start + index + 2)?,
                                BlockEdge::new(BranchType::Else),
                            ));
                        }
                    }
                    OpCode::LOP_JUMPXEQKN | OpCode::LOP_JUMPXEQKS => {
                        let a = self.register(a as _);
                        let literal =
                            self.constant((aux & ((1 << 24) - 1)) as _, block_start + index)?;
                        statements.push(
                            ast::If::new(
                                ast::Binary::new(
                                    a.into(),
                                    literal.into(),
                                    ast::BinaryOperation::Equal,
                                )
                                .into(),
                                ast::Block::default(),
                                ast::Block::default(),
                            )
                            .into(),
                        );
                        if aux & (1 << 31) != 0 {
                            edges.push((
                                self.block_to_node(
                                    ((block_start + index + 1) as isize + d as isize) as usize,
                                )?,
                                BlockEdge::new(BranchType::Else),
                            ));
                            edges.push((
                                self.block_to_node(block_start + index + 2)?,
                                BlockEdge::new(BranchType::Then),
                            ));
                        } else {
                            edges.push((
                                self.block_to_node(
                                    ((block_start + index + 1) as isize + d as isize) as usize,
                                )?,
                                BlockEdge::new(BranchType::Then),
                            ));
                            edges.push((
                                self.block_to_node(block_start + index + 2)?,
                                BlockEdge::new(BranchType::Else),
                            ));
                        }
                    }
                    OpCode::LOP_FORNPREP => {
                        // TODO: do this properly
                        let limit = self.register(a as _);
                        let step = self.register(a as usize + 1);
                        let counter = self.register(a as usize + 2);
                        statements.push(ast::NumForInit::new(counter, limit, step).into());

                        let loop_node = self
                            .function
                            .predecessor_blocks(self.block_to_node(block_start + index + 1)?)
                            .filter(|&p| {
                                self.function
                                    .block(p)
                                    .unwrap()
                                    .last()
                                    .is_some_and(|s| matches!(s, ast::Statement::NumForNext(_)))
                            })
                            .exactly_one()
                            .map_err(|_| {
                                self.lift_error(
                                    Some(block_start + index),
                                    "unique numeric-for loop predecessor",
                                    "FORNPREP's loop target does not have exactly one \
                                     predecessor block ending in NumForNext",
                                )
                            })?;
                        edges.push((loop_node, BlockEdge::new(BranchType::Unconditional)));
                    }
                    OpCode::LOP_FORNLOOP => {
                        let limit = self.register(a as _);
                        let step = self.register(a as usize + 1);
                        let counter = self.register(a as usize + 2);
                        statements
                            .push(ast::NumForNext::new(counter, limit.into(), step.into()).into());
                        edges.push((
                            self.block_to_node(
                                ((block_start + index + 1) as isize + d as isize) as usize,
                            )?,
                            BlockEdge::new(BranchType::Then),
                        ));
                        edges.push((
                            self.block_to_node(block_start + index + 1)?,
                            BlockEdge::new(BranchType::Else),
                        ));
                    }
                    OpCode::LOP_FORGPREP
                    | OpCode::LOP_FORGPREP_INEXT
                    | OpCode::LOP_FORGPREP_NEXT => {
                        let generator = self.register(a as _);
                        let state = self.register(a as usize + 1);
                        let counter = self.register(a as usize + 2);
                        statements.push(ast::GenericForInit::new(generator, state, counter).into());
                        let loop_index =
                            self.jump_target(block_start + index + 1, d as isize, block_start + index)?;
                        let loop_instruction =
                            self.function_list[self.function.id].instructions.get(loop_index);
                        if !matches!(
                            loop_instruction,
                            Some(Instruction::AD {
                                op_code: OpCode::LOP_FORGLOOP,
                                ..
                            })
                        ) {
                            return Err(self.lift_error(
                                Some(block_start + index),
                                "forgprep target is a forgloop",
                                format!(
                                    "FORGPREP target instruction {loop_index} is \
                                     {loop_instruction:?}, expected LOP_FORGLOOP"
                                ),
                            ));
                        }
                        edges.push((
                            self.block_to_node(loop_index)?,
                            BlockEdge::new(BranchType::Unconditional),
                        ));
                    }
                    // TODO: i think vm can assume generator is next/inext based on aux,
                    // so what happens if the generator passed isnt next and the env isnt tainted?
                    // this could be done with some custom bytecode
                    // same applies to fastcall
                    OpCode::LOP_FORGLOOP => {
                        let generator = self.register(a as _);
                        let state = self.register(a as usize + 1);
                        let _counter = self.register(a as usize + 2);
                        statements.push(
                            ast::GenericForNext::new(
                                (a as usize + 3..a as usize + 3 + (aux & 0xff) as usize)
                                    .map(|r| self.register(r))
                                    .collect::<Vec<_>>(),
                                generator.into(),
                                state,
                            )
                            .into(),
                        );
                        edges.push((
                            self.block_to_node(
                                ((block_start + index + 1) as isize + d as isize) as usize,
                            )?,
                            BlockEdge::new(BranchType::Then),
                        ));
                        edges.push((
                            self.block_to_node(block_start + index + 1)?,
                            BlockEdge::new(BranchType::Else),
                        ));
                    }
                    OpCode::LOP_DUPTABLE => {
                        let dtable_constant =
                            self.function_list[self.function.id].constants.get(d as usize);
                        let entries = match dtable_constant {
                            Some(BytecodeConstant::Table { entries }) => entries.clone(),
                            other => {
                                return Err(self.lift_error(
                                    Some(block_start + index),
                                    "constant kind supported at this operand",
                                    format!(
                                        "DUPTABLE constant index {d} is {other:?}, expected a \
                                         table"
                                    ),
                                ));
                            }
                        };
                        let mut values = Vec::with_capacity(entries.len());
                        for (key_index, value_index) in entries {
                            let key = self.constant(key_index, block_start + index)?;
                            let value = match value_index {
                                Some(value_index)
                                    if matches!(
                                        self.function_list[self.function.id]
                                            .constants
                                            .get(value_index),
                                        Some(BytecodeConstant::Nil)
                                    ) =>
                                {
                                    continue;
                                }
                                Some(value_index) => self.constant(value_index, block_start + index)?,
                                None => ast::Literal::Number(0.0),
                            };
                            values.push((Some(key.into()), value.into()));
                        }
                        statements.push(
                            ast::Assign::new(
                                vec![self.register(a as _).into()],
                                vec![ast::Table(values).into()],
                            )
                            .into(),
                        );
                    }
                    OpCode::LOP_DUPCLOSURE | OpCode::LOP_NEWCLOSURE => {
                        let dest_local = self.register(a as _);
                        let operand = usize::try_from(d).map_err(|_| {
                            self.lift_error(
                                Some(block_start + index),
                                "closure operand is non-negative",
                                format!("{op_code:?} references child index {d}"),
                            )
                        })?;
                        let func_index = match op_code {
                            OpCode::LOP_NEWCLOSURE => self.function_list[self.function.id]
                                .functions
                                .get(operand)
                                .copied()
                                .ok_or_else(|| {
                                    self.lift_error(
                                        Some(block_start + index),
                                        "closure index within declared children",
                                        format!(
                                            "instruction references child {operand} but the \
                                             prototype declares {} child function(s)",
                                            self.function_list[self.function.id].functions.len()
                                        ),
                                    )
                                })?,
                            _ => {
                                let constant = self.function_list[self.function.id]
                                    .constants
                                    .get(operand)
                                    .ok_or_else(|| {
                                        self.lift_error(
                                            Some(block_start + index),
                                            "constant index within constant table",
                                            format!(
                                                "instruction references constant {operand} but \
                                                 the prototype declares {} constant(s)",
                                                self.function_list[self.function.id].constants.len()
                                            ),
                                        )
                                    })?;
                                match constant {
                                    &BytecodeConstant::Closure(func_index) => func_index,
                                    other => {
                                        return Err(self.lift_error(
                                            Some(block_start + index),
                                            "DUPCLOSURE operand names a closure constant",
                                            format!(
                                                "constant {operand} is {other:?}, not a closure"
                                            ),
                                        ));
                                    }
                                }
                            }
                        };
                        if func_index >= self.function_list.len() {
                            return Err(self.lift_error(
                                Some(block_start + index),
                                "closure target within prototype table",
                                format!(
                                    "closure names prototype {func_index} but the bytecode \
                                     contains {} prototype(s)",
                                    self.function_list.len()
                                ),
                            ));
                        }
                        let func_name_index = self.function_list[func_index].function_name;
                        let func_name = self.debug_name(func_name_index);

                        let func = &self.function_list[func_index];
                        let mut upvalues_passed = Vec::with_capacity(func.num_upvalues.into());
                        for capture_index in 0..func.num_upvalues as usize {
                            let (capture_instruction_index, capture_instruction) =
                                iter.next().ok_or_else(|| {
                                    self.lift_error(
                                        Some(block_start + index),
                                        "newclosure followed by one capture per upvalue",
                                        format!(
                                            "closure declares {} upvalue(s) but the block ran \
                                             out of instructions after capturing {capture_index}",
                                            func.num_upvalues
                                        ),
                                    )
                                })?;
                            let mut capture_origin = self.source_origin(
                                block_start + capture_instruction_index,
                                *capture_instruction,
                            );
                            capture_origin.capture = Some(cfg::provenance::CaptureOrigin {
                                closure_function_id: func_index,
                                capture_index,
                            });
                            emitted_origins.insert(capture_origin);
                            let capture_instruction_pos = block_start + capture_instruction_index;
                            let local = match capture_instruction {
                                &Instruction::BC {
                                    op_code: OpCode::LOP_CAPTURE,
                                    a: capture_type,
                                    b: source,
                                    ..
                                } => match capture_type {
                                    // capture value
                                    0 => ast::Upvalue::Copy(self.register(source as _)),
                                    // capture ref
                                    1 => ast::Upvalue::Ref(self.register(source as _)),
                                    // capture upval
                                    2 => {
                                        ast::Upvalue::Ref(self.upvalue(source, capture_instruction_pos)?)
                                    }
                                    _ => {
                                        return Err(self.lift_error(
                                            Some(capture_instruction_pos),
                                            "supported capture kind",
                                            format!(
                                                "LOP_CAPTURE type {capture_type} is not 0 \
                                                 (value), 1 (ref), or 2 (upvalue)"
                                            ),
                                        ));
                                    }
                                },
                                other => {
                                    return Err(self.lift_error(
                                        Some(capture_instruction_pos),
                                        "newclosure followed by one capture per upvalue",
                                        format!(
                                            "expected LOP_CAPTURE while capturing upvalue \
                                             {capture_index}, found {other:?}"
                                        ),
                                    ));
                                }
                            };
                            upvalues_passed.push(local);
                        }

                        let function = Arc::<Mutex<_>>::default();
                        self.child_functions
                            .insert(ByAddress(function.clone()), func_index);
                        function.lock().name = func_name;
                        statements.push(
                            ast::Assign::new(
                                vec![dest_local.into()],
                                vec![
                                    ast::Closure {
                                        function: ByAddress(function),
                                        upvalues: upvalues_passed,
                                    }
                                    .into(),
                                ],
                            )
                            .into(),
                        );
                    }
                    _ => {
                        return Err(self.lift_error(
                            Some(block_start + index),
                            "supported opcode",
                            format!(
                                "{:?} is not a supported instruction in the AD encoding",
                                instruction.op_code()
                            ),
                        ));
                    }
                },
                Instruction::E { op_code, e } => match op_code {
                    OpCode::LOP_JUMPX => {
                        edges.push((
                            self.block_to_node(
                                ((block_start + index + 1) as isize + e as isize) as usize,
                            )?,
                            BlockEdge::new(BranchType::Unconditional),
                        ));
                    }
                    _ => {
                        return Err(self.lift_error(
                            Some(block_start + index),
                            "supported opcode",
                            format!(
                                "{:?} is not a supported instruction in the E encoding",
                                instruction.op_code()
                            ),
                        ));
                    }
                },
            }
            if !origins_attached {
                while origins.len() < statements.len() {
                    origins.push(emitted_origins.clone());
                }
            }
            assert_eq!(statements.len(), origins.len());
            if terminate {
                break;
            }
        }

        let last_index = iter
            .next()
            .map(|(i, _)| block_start + i - 1)
            .unwrap_or(block_end.saturating_sub(1));
        if edges.is_empty()
            && !Self::is_terminator(self.function_list[self.function.id].instructions[last_index])
        {
            if last_index + 1 == self.function_list[self.function.id].instructions.len() {
                statements
                    .push(ast::Comment::new("warning: block does not return".to_string()).into());
            } else {
                edges.push((
                    self.block_to_node(last_index + 1)?,
                    BlockEdge::new(BranchType::Unconditional),
                ));
            }
        }

        assert_eq!(statements.len(), origins.len());
        Ok((statements, origins, edges))
    }

    fn register(&mut self, index: usize) -> ast::RcLocal {
        if let Some(local) = self.register_map.get(&index) {
            return local.clone();
        }

        let local = ast::RcLocal::new(ast::Local::new(self.debug_name_for_register(index)));
        let debug_lifetimes = self.function_list[self.function.id]
            .debug_locals
            .iter()
            .filter(|debug_local| debug_local.register as usize == index)
            .filter_map(|debug_local| {
                let name = self
                    .string_table
                    .get(debug_local.name.checked_sub(1)?)?
                    .clone();
                Some(cfg::provenance::DebugLifetime::new(
                    name,
                    debug_local.start_pc,
                    debug_local.end_pc,
                ))
            })
            .collect();
        let binding = if index < self.function_list[self.function.id].num_parameters as usize {
            cfg::provenance::BindingIdentity::parameter(self.function.id, index)
        } else {
            cfg::provenance::BindingIdentity::local(self.function.id, index)
        };
        self.function.set_register_family(
            local.clone(),
            cfg::provenance::RegisterFamily::new(self.function.id, index, binding, debug_lifetimes),
        );
        self.register_map.insert(index, local.clone());
        local
    }

    fn source_origin(
        &self,
        instruction: usize,
        value: Instruction,
    ) -> cfg::provenance::SourceOrigin {
        cfg::provenance::SourceOrigin::new(
            self.function.id,
            instruction,
            self.source_line(instruction),
            format!("{:?}", value.op_code()),
        )
    }

    fn source_line(&self, instruction: usize) -> Option<usize> {
        let function = &self.function_list[self.function.id];
        let gap = function.line_gap_log2?;
        let deltas = function.line_info_delta.as_ref()?;
        let absolute_deltas = function.abs_line_info_delta.as_ref()?;
        let relative = deltas
            .iter()
            .take(instruction + 1)
            .fold(0u8, |line, delta| line.wrapping_add(*delta));
        let interval = instruction >> gap;
        let absolute = absolute_deltas
            .iter()
            .take(interval + 1)
            .copied()
            .sum::<i32>();
        usize::try_from(absolute + i32::from(relative)).ok()
    }

    fn debug_name(&self, string_index: usize) -> Option<String> {
        let bytes = self.string_table.get(string_index.checked_sub(1)?)?;
        ast::is_valid_identifier(bytes)
            .then(|| String::from_utf8(bytes.clone()).ok())
            .flatten()
    }

    fn debug_name_for_register(&self, register: usize) -> Option<String> {
        let function = &self.function_list[self.function.id];
        let local = whole_function_debug_local(
            &function.debug_locals,
            register,
            function.instructions.len(),
        )?;
        self.debug_name(local.name)
    }

    /// Resolves a 1-based string-table reference into the referenced bytes, returning an error
    /// instead of underflowing or indexing out of bounds for a reference the string table
    /// cannot satisfy.
    fn string_table_entry(
        &self,
        string_index: usize,
        instruction: usize,
    ) -> Result<&Vec<u8>, DecompileError> {
        string_index
            .checked_sub(1)
            .and_then(|index| self.string_table.get(index))
            .ok_or_else(|| {
                self.lift_error(
                    Some(instruction),
                    "string table index within declared strings",
                    format!(
                        "instruction references string index {string_index} but the string \
                         table declares {} entr(ies)",
                        self.string_table.len()
                    ),
                )
            })
    }

    fn constant(&mut self, index: usize, instruction: usize) -> Result<ast::Literal, DecompileError> {
        if let Some(literal) = self.constant_map.get(&index) {
            return Ok(literal.clone());
        }
        let constant = self.function_list[self.function.id]
            .constants
            .get(index)
            .ok_or_else(|| {
                self.lift_error(
                    Some(instruction),
                    "constant index within declared constants",
                    format!(
                        "instruction references constant index {index} but the prototype \
                         declares {} constant(s)",
                        self.function_list[self.function.id].constants.len()
                    ),
                )
            })?
            .clone();
        let converted_constant = match constant {
            BytecodeConstant::Nil => ast::Literal::Nil,
            BytecodeConstant::Boolean(v) => ast::Literal::Boolean(v),
            BytecodeConstant::Number(v) => ast::Literal::Number(v),
            BytecodeConstant::Integer(v) => ast::Literal::Integer(v),
            BytecodeConstant::String(v) => {
                ast::Literal::String(self.string_table_entry(v, instruction)?.clone())
            }
            BytecodeConstant::VectorF(x, y, z, _) => ast::Literal::Vector(x, y, z),
            BytecodeConstant::VectorD(x, y, z, _) => ast::Literal::VectorD(x, y, z),
            other => {
                return Err(self.lift_error(
                    Some(instruction),
                    "constant kind supported at this operand",
                    format!("constant index {index} is a {other:?}, which this opcode cannot use"),
                ));
            }
        };
        Ok(self
            .constant_map
            .entry(index)
            .or_insert(converted_constant)
            .clone())
    }

    fn constant_string(&self, index: usize, instruction: usize) -> Result<String, DecompileError> {
        let constant = self.function_list[self.function.id]
            .constants
            .get(index)
            .ok_or_else(|| {
                self.lift_error(
                    Some(instruction),
                    "constant index within declared constants",
                    format!(
                        "instruction references constant index {index} but the prototype \
                         declares {} constant(s)",
                        self.function_list[self.function.id].constants.len()
                    ),
                )
            })?;
        let BytecodeConstant::String(string_index) = constant else {
            return Err(self.lift_error(
                Some(instruction),
                "constant kind supported at this operand",
                format!("constant index {index} is a {constant:?}, expected a string"),
            ));
        };
        Ok(String::from_utf8_lossy(self.string_table_entry(*string_index, instruction)?).into_owned())
    }

    /// Resolves a constant operand that must specifically be a string literal (globals,
    /// imports, method names), returning an error instead of panicking when the referenced
    /// constant is a different literal kind.
    fn constant_bytes(
        &mut self,
        index: usize,
        instruction: usize,
    ) -> Result<Vec<u8>, DecompileError> {
        let literal = self.constant(index, instruction)?;
        literal.into_string().map_err(|literal| {
            self.lift_error(
                Some(instruction),
                "constant kind supported at this operand",
                format!("constant index {index} is a {literal:?}, expected a string"),
            )
        })
    }

    /// Consumes the pending "open" multi-value result left by a preceding call or vararg
    /// expression, returning an error instead of panicking when this instruction's b/c == 0
    /// encoding expects one but nothing was left pending.
    fn take_open_result(
        &self,
        top: &mut Option<(ast::RValue, u8)>,
        instruction: usize,
    ) -> Result<(ast::RValue, u8), DecompileError> {
        top.take().ok_or_else(|| {
            self.lift_error(
                Some(instruction),
                "open multi-value result available",
                "instruction consumes an open-ended result (b/c == 0) but no preceding call \
                 or vararg expression left one pending",
            )
        })
    }

    fn block_to_node(&self, insn_index: usize) -> Result<NodeIndex, DecompileError> {
        self.blocks.get(&insn_index).copied().ok_or_else(|| {
            self.lift_error(
                None,
                "resolved block for instruction index",
                format!("instruction index {insn_index} was not discovered as a block boundary"),
            )
        })
    }

    fn is_terminator(instruction: Instruction) -> bool {
        match instruction {
            Instruction::BC { op_code, c, .. } => match op_code {
                OpCode::LOP_RETURN => true,
                OpCode::LOP_LOADB if c != 0 => true,
                _ => false,
            },
            Instruction::AD { op_code, .. } => matches!(
                op_code,
                OpCode::LOP_JUMP
                    | OpCode::LOP_JUMPBACK
                    | OpCode::LOP_CMPPROTO
                    | OpCode::LOP_JUMPIF
                    | OpCode::LOP_JUMPIFNOT
                    | OpCode::LOP_JUMPIFEQ
                    | OpCode::LOP_JUMPIFLE
                    | OpCode::LOP_JUMPIFLT
                    | OpCode::LOP_JUMPIFNOTEQ
                    | OpCode::LOP_JUMPIFNOTLE
                    | OpCode::LOP_JUMPIFNOTLT
                    | OpCode::LOP_JUMPXEQKNIL
                    | OpCode::LOP_JUMPXEQKB
                    | OpCode::LOP_JUMPXEQKN
                    | OpCode::LOP_JUMPXEQKS
                    | OpCode::LOP_FORNPREP
                    | OpCode::LOP_FORNLOOP
                    | OpCode::LOP_FORGPREP
                    | OpCode::LOP_FORGLOOP
                    | OpCode::LOP_FORGPREP_INEXT
                    | OpCode::LOP_FORGPREP_NEXT
            ),
            Instruction::E { op_code, .. } => matches!(op_code, OpCode::LOP_JUMPX),
        }
    }
}

#[cfg(test)]
mod naming_tests {
    use super::{DebugLocal, whole_function_debug_local};

    fn debug_local(start_pc: usize, end_pc: usize, register: u8) -> DebugLocal {
        DebugLocal {
            name: 1,
            start_pc,
            end_pc,
            register,
        }
    }

    #[test]
    fn accepts_only_one_full_function_debug_lifetime() {
        assert!(whole_function_debug_local(&[debug_local(0, 8, 2)], 2, 8).is_some());
        assert!(whole_function_debug_local(&[debug_local(1, 8, 2)], 2, 8).is_none());
        assert!(whole_function_debug_local(&[debug_local(0, 0, 2)], 2, 8).is_none());
        assert!(
            whole_function_debug_local(&[debug_local(0, 8, 2), debug_local(0, 8, 2)], 2, 8)
                .is_none()
        );
    }
}

#[cfg(test)]
mod robustness_tests {
    use super::{BytecodeFunction, Instruction, Lifter, OpCode, ast};

    /// A minimal prototype with no upvalues, no parameters, and the given instructions --
    /// enough to drive [`Lifter::lift`] directly without going through the deserializer.
    fn prototype(instructions: Vec<Instruction>) -> BytecodeFunction {
        BytecodeFunction {
            max_stack_size: 2,
            num_parameters: 0,
            num_upvalues: 0,
            is_vararg: false,
            flags: 0,
            instructions,
            constants: Vec::new(),
            functions: Vec::new(),
            line_defined: 0,
            function_name: 0,
            line_gap_log2: None,
            line_info_delta: None,
            abs_line_info_delta: None,
            debug_locals: Vec::new(),
            debug_upvalues: Vec::new(),
            feedback: Vec::new(),
            cost: None,
        }
    }

    #[test]
    fn getupval_beyond_declared_upvalues_errors_instead_of_panicking() {
        let functions = vec![prototype(vec![
            Instruction::BC {
                op_code: OpCode::LOP_GETUPVAL,
                a: 1,
                b: 0,
                c: 0,
                aux: 0,
            },
            Instruction::BC {
                op_code: OpCode::LOP_RETURN,
                a: 0,
                b: 1,
                c: 0,
                aux: 0,
            },
        ])];
        let strings = Vec::new();

        let error = Lifter::lift(&functions, &strings, 0).unwrap_err();

        assert_eq!(error.phase, super::DecompilePhase::Lift);
        assert_eq!(error.invariant, "upvalue index within declared upvalues");
        assert!(error.detail.contains("upvalue index 0"));
        assert!(error.detail.contains("declares 0 upvalue"));
    }

    #[test]
    fn setupval_beyond_declared_upvalues_errors_instead_of_panicking() {
        let functions = vec![prototype(vec![
            Instruction::BC {
                op_code: OpCode::LOP_SETUPVAL,
                a: 0,
                b: 3,
                c: 0,
                aux: 0,
            },
            Instruction::BC {
                op_code: OpCode::LOP_RETURN,
                a: 0,
                b: 1,
                c: 0,
                aux: 0,
            },
        ])];
        let strings = Vec::new();

        let error = Lifter::lift(&functions, &strings, 0).unwrap_err();

        assert_eq!(error.phase, super::DecompilePhase::Lift);
        assert_eq!(error.invariant, "upvalue index within declared upvalues");
        assert!(error.detail.contains("upvalue index 3"));
    }

    #[test]
    fn jump_before_first_instruction_errors_instead_of_panicking() {
        let functions = vec![prototype(vec![Instruction::AD {
            op_code: OpCode::LOP_JUMPBACK,
            a: 0,
            d: -5,
            aux: 0,
        }])];
        let strings = Vec::new();

        let error = Lifter::lift(&functions, &strings, 0).unwrap_err();

        assert_eq!(error.phase, super::DecompilePhase::Lift);
        assert_eq!(error.invariant, "jump target within instruction stream");
    }

    /// A conditional branch in the last slot makes its fallthrough boundary land exactly one
    /// past the last instruction. That boundary is the implicit end of the function and lifts
    /// to an empty terminal block.
    #[test]
    fn trailing_conditional_fallthrough_lifts_to_an_empty_terminal_block() {
        let functions = vec![prototype(vec![
            Instruction::BC {
                op_code: OpCode::LOP_LOADNIL,
                a: 0,
                b: 0,
                c: 0,
                aux: 0,
            },
            Instruction::AD {
                op_code: OpCode::LOP_JUMPIF,
                a: 0,
                d: -1,
                aux: 0,
            },
        ])];
        let strings = Vec::new();

        let (function, _, _) = Lifter::lift(&functions, &strings, 0)
            .expect("a fallthrough boundary at the end of the function is liftable");

        assert_eq!(
            function
                .blocks()
                .filter(|(_, block)| block.is_empty())
                .count(),
            2,
            "the entry block and the terminal boundary block are both empty"
        );
    }

    /// A branch whose target is exactly one past the last instruction names the same implicit
    /// end-of-function boundary and is equally liftable.
    #[test]
    fn branch_to_end_of_function_lifts_to_an_empty_terminal_block() {
        let functions = vec![prototype(vec![
            Instruction::AD {
                op_code: OpCode::LOP_JUMP,
                a: 0,
                d: 1,
                aux: 0,
            },
            Instruction::BC {
                op_code: OpCode::LOP_RETURN,
                a: 0,
                b: 1,
                c: 0,
                aux: 0,
            },
        ])];
        let strings = Vec::new();

        let (function, _, _) =
            Lifter::lift(&functions, &strings, 0).expect("a branch to the end of the function is liftable");

        assert!(
            function
                .blocks()
                .any(|(_, block)| block.iter().any(|s| matches!(s, ast::Statement::Return(_)))),
            "the reachable RETURN survives the empty terminal block"
        );
    }

    /// An aux-carrying conditional in the last slot pushes its fallthrough boundary two past
    /// the last instruction, which no instruction stream can contain.
    #[test]
    fn block_boundary_past_end_of_function_errors_instead_of_panicking() {
        let functions = vec![prototype(vec![
            Instruction::BC {
                op_code: OpCode::LOP_LOADNIL,
                a: 0,
                b: 0,
                c: 0,
                aux: 0,
            },
            Instruction::AD {
                op_code: OpCode::LOP_JUMPIFEQ,
                a: 0,
                d: -1,
                aux: 0,
            },
        ])];
        let strings = Vec::new();

        let error = Lifter::lift(&functions, &strings, 0).unwrap_err();

        assert_eq!(error.phase, super::DecompilePhase::Lift);
        assert_eq!(error.invariant, "block boundary within instruction stream");
        assert!(error.detail.contains("block starts at instruction 3"));
        assert!(error.detail.contains("only has 2 instruction(s)"));
    }

    /// Adding to a `u8` operand before widening overflows once the argument window reaches
    /// past register 255: a panic under `overflow-checks`, and a wrapped -- here empty --
    /// register range without them. Widening first keeps the full window in both profiles.
    #[test]
    fn call_spanning_the_top_register_keeps_its_arguments_instead_of_overflowing() {
        let functions = vec![prototype(vec![
            Instruction::BC {
                op_code: OpCode::LOP_CALL,
                a: 254,
                b: 4,
                c: 1,
                aux: 0,
            },
            Instruction::BC {
                op_code: OpCode::LOP_RETURN,
                a: 0,
                b: 1,
                c: 0,
                aux: 0,
            },
        ])];
        let strings = Vec::new();

        let (function, _, _) = Lifter::lift(&functions, &strings, 0)
            .expect("a call spanning the top register is liftable");

        let argument_counts = function
            .blocks()
            .flat_map(|(_, block)| block.iter())
            .filter_map(|statement| match statement {
                ast::Statement::Call(call) => Some(call.arguments.len()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(argument_counts, vec![3]);
    }

    /// `LOP_CAPTURE` with capture type 2 reads the *enclosing* prototype's upvalue list, the
    /// sibling of the `LOP_GETUPVAL`/`LOP_SETUPVAL` site.
    #[test]
    fn capture_of_undeclared_upvalue_errors_instead_of_panicking() {
        let mut parent = prototype(vec![
            Instruction::AD {
                op_code: OpCode::LOP_NEWCLOSURE,
                a: 0,
                d: 0,
                aux: 0,
            },
            Instruction::BC {
                op_code: OpCode::LOP_CAPTURE,
                a: 2,
                b: 0,
                c: 0,
                aux: 0,
            },
            Instruction::BC {
                op_code: OpCode::LOP_RETURN,
                a: 0,
                b: 1,
                c: 0,
                aux: 0,
            },
        ]);
        parent.functions = vec![1];
        let mut child = prototype(vec![Instruction::BC {
            op_code: OpCode::LOP_RETURN,
            a: 0,
            b: 1,
            c: 0,
            aux: 0,
        }]);
        child.num_upvalues = 1;
        let functions = vec![parent, child];
        let strings = Vec::new();

        let error = Lifter::lift(&functions, &strings, 0).unwrap_err();

        assert_eq!(error.phase, super::DecompilePhase::Lift);
        assert_eq!(error.invariant, "upvalue index within declared upvalues");
        assert!(error.detail.contains("upvalue index 0"));
        assert!(error.detail.contains("declares 0 upvalue"));
    }

    /// `LOP_NEWCLOSURE` indexes the prototype's child table with a raw operand.
    #[test]
    fn newclosure_beyond_declared_children_errors_instead_of_panicking() {
        let functions = vec![prototype(vec![
            Instruction::AD {
                op_code: OpCode::LOP_NEWCLOSURE,
                a: 0,
                d: 4,
                aux: 0,
            },
            Instruction::BC {
                op_code: OpCode::LOP_RETURN,
                a: 0,
                b: 1,
                c: 0,
                aux: 0,
            },
        ])];
        let strings = Vec::new();

        let error = Lifter::lift(&functions, &strings, 0).unwrap_err();

        assert_eq!(error.phase, super::DecompilePhase::Lift);
        assert_eq!(error.invariant, "closure index within declared children");
    }

    /// `LOP_DUPCLOSURE` reads a constant that a malformed chunk need not have made a closure.
    #[test]
    fn dupclosure_naming_a_non_closure_constant_errors_instead_of_panicking() {
        let mut function = prototype(vec![
            Instruction::AD {
                op_code: OpCode::LOP_DUPCLOSURE,
                a: 0,
                d: 0,
                aux: 0,
            },
            Instruction::BC {
                op_code: OpCode::LOP_RETURN,
                a: 0,
                b: 1,
                c: 0,
                aux: 0,
            },
        ]);
        function.constants = vec![super::BytecodeConstant::Boolean(true)];
        let functions = vec![function];
        let strings = Vec::new();

        let error = Lifter::lift(&functions, &strings, 0).unwrap_err();

        assert_eq!(error.phase, super::DecompilePhase::Lift);
        assert_eq!(
            error.invariant,
            "DUPCLOSURE operand names a closure constant"
        );
    }
}
