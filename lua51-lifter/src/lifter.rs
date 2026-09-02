use std::{cmp::Reverse, collections::BinaryHeap};

use by_address::ByAddress;
use cfg::block::{BlockEdge, BranchType};
use cfg::provenance::{OriginSet, SourceOrigin};
use either::Either;

use itertools::Itertools;
use parking_lot::Mutex;
use rustc_hash::FxHashMap;

use ast::{RcLocal, Statement};
use cfg::function::Function;

use lua51_deserializer::{
    Function as BytecodeFunction, Instruction, Value,
    argument::{Constant, Register, RegisterOrConstant},
};

use petgraph::{Direction, stable_graph::NodeIndex, visit::EdgeRef};

use triomphe::Arc;

const VARARG_ISVARARG: u8 = 2;
const VARARG_NEEDSARG: u8 = 4;

pub struct Lifter<'a, 'b> {
    bytecode: &'a BytecodeFunction<'a>,
    nodes: FxHashMap<usize, NodeIndex>,
    insert_between: FxHashMap<NodeIndex, (NodeIndex, Statement, OriginSet)>,
    statement_origins: FxHashMap<NodeIndex, Vec<OriginSet>>,
    locals: FxHashMap<Register, RcLocal>,
    constants: FxHashMap<usize, ast::Literal>,
    function: Function,
    upvalues: Vec<RcLocal>,
    lifted_functions:
        &'b mut Vec<(Arc<Mutex<ast::Function>>, Function, Vec<RcLocal>, bool)>,
    next_function_id: &'b mut usize,
}

impl<'a, 'b> Lifter<'a, 'b> {
    fn debug_name(bytes: &[u8]) -> Option<String> {
        ast::is_valid_identifier(bytes)
            .then(|| std::str::from_utf8(bytes).ok().map(str::to_owned))
            .flatten()
    }

    fn debug_lifetimes_by_register(&self) -> Vec<Vec<cfg::provenance::DebugLifetime>> {
        let mut lifetimes = vec![Vec::new(); usize::from(self.bytecode.maximum_stack_size)];
        let mut active_ends = BinaryHeap::new();
        for local in &self.bytecode.locals {
            while active_ends
                .peek()
                .is_some_and(|Reverse(end)| *end <= local.range.start)
            {
                active_ends.pop();
            }
            if local.range.start < local.range.end {
                lifetimes[active_ends.len()].push(cfg::provenance::DebugLifetime::new(
                    local.name.to_vec(),
                    local.range.start as usize,
                    local.range.end as usize,
                ));
                active_ends.push(Reverse(local.range.end));
            }
        }
        lifetimes
    }

    fn allocate_locals(&mut self) {
        self.upvalues
            .reserve(self.bytecode.number_of_upvalues as usize);
        for index in 0..self.bytecode.number_of_upvalues {
            let local = RcLocal::new(ast::Local::new(
                self.bytecode
                    .upvalues
                    .get(index as usize)
                    .and_then(|name| Self::debug_name(name)),
            ));
            self.function.set_binding(
                local.clone(),
                cfg::provenance::BindingIdentity::Upvalue {
                    function_id: self.function.id,
                    index: index as usize,
                },
            );
            self.upvalues.push(local);
        }

        self.locals
            .reserve(self.bytecode.maximum_stack_size as usize);
        let mut debug_lifetimes = self.debug_lifetimes_by_register();
        for i in 0..self.bytecode.maximum_stack_size {
            let register_lifetimes = std::mem::take(&mut debug_lifetimes[i as usize]);
            let is_legacy_arg = self.bytecode.vararg_flag & VARARG_NEEDSARG != 0
                && i == self.bytecode.number_of_parameters;
            let name = if is_legacy_arg {
                Some("arg".to_owned())
            } else {
                match register_lifetimes.as_slice() {
                    [lifetime]
                        if lifetime.start_instruction == 0
                            && lifetime.end_instruction == self.bytecode.code.len() =>
                    {
                        Self::debug_name(&lifetime.name)
                    }
                    _ => None,
                }
            };
            let local = RcLocal::new(ast::Local::new(name));
            let binding = if i < self.bytecode.number_of_parameters {
                cfg::provenance::BindingIdentity::parameter(self.function.id, i as usize)
            } else {
                cfg::provenance::BindingIdentity::local(self.function.id, i as usize)
            };
            self.function.set_register_family(
                local.clone(),
                cfg::provenance::RegisterFamily::new(
                    self.function.id,
                    i as usize,
                    binding,
                    register_lifetimes,
                ),
            );
            if i < self.bytecode.number_of_parameters || is_legacy_arg {
                self.function.parameters.push(local.clone());
            }
            self.locals.insert(Register(i), local);
        }
    }

    // TODO: support jumps to invalid destinations
    // including cases where there is usize::MAX instructions and the last instruction
    // skips forward, overflowing
    fn create_block_map(&mut self) {
        self.nodes.insert(0, self.function.new_block());
        for (insn_index, insn) in self.bytecode.code.iter().enumerate() {
            match *insn {
                Instruction::LoadBoolean {
                    skip_next: true, ..
                } => {
                    self.nodes
                        .entry(insn_index + 1)
                        .or_insert_with(|| self.function.new_block());
                    self.nodes
                        .entry(insn_index + 2)
                        .or_insert_with(|| self.function.new_block());
                }
                Instruction::Equal { .. }
                | Instruction::LessThan { .. }
                | Instruction::LessThanOrEqual { .. }
                | Instruction::Test { .. }
                | Instruction::TestSet { .. }
                | Instruction::IterateGenericForLoop { .. } => {
                    self.nodes
                        .entry(insn_index + 1)
                        .or_insert_with(|| self.function.new_block());
                    self.nodes
                        .entry(insn_index + 2)
                        .or_insert_with(|| self.function.new_block());
                }
                Instruction::Jump(skip) => {
                    let dest_index = (insn_index + 1)
                        .checked_add_signed(skip.try_into().unwrap())
                        .unwrap();
                    self.nodes
                        .entry(dest_index)
                        .or_insert_with(|| self.function.new_block());
                    self.nodes
                        .entry(insn_index + 1)
                        .or_insert_with(|| self.function.new_block());
                }
                Instruction::IterateNumericForLoop { skip, .. }
                | Instruction::InitNumericForLoop { skip, .. } => {
                    self.nodes
                        .entry(
                            (insn_index + 1)
                                .checked_add_signed(skip.try_into().unwrap())
                                .unwrap(),
                        )
                        .or_insert_with(|| self.function.new_block());
                    self.nodes
                        .entry(insn_index + 1)
                        .or_insert_with(|| self.function.new_block());
                }
                Instruction::Return(..) => {
                    self.nodes
                        .entry(insn_index + 1)
                        .or_insert_with(|| self.function.new_block());
                }
                _ => {}
            }
        }
    }

    fn code_ranges(&self) -> Vec<(usize, usize)> {
        let mut nodes = self.nodes.keys().cloned().collect::<Vec<_>>();
        nodes.sort_unstable();
        let ends = nodes
            .iter()
            .skip(1)
            .map(|&s| s - 1)
            .chain(std::iter::once(self.bytecode.code.len() - 1));
        nodes.iter().cloned().zip(ends).collect()
    }

    fn constant(&mut self, constant: Constant) -> ast::Literal {
        self.constants
            .entry(constant.0 as usize)
            .or_insert_with(
                || match self.bytecode.constants.get(constant.0 as usize).unwrap() {
                    Value::Nil => ast::Literal::Nil,
                    Value::Boolean(v) => ast::Literal::Boolean(*v),
                    Value::Number(v) => ast::Literal::Number(*v),
                    Value::String(v) => ast::Literal::String(v.to_vec()),
                },
            )
            .clone()
    }

    fn register_or_constant(&mut self, value: RegisterOrConstant) -> ast::RValue {
        match value.0 {
            Either::Left(register) => self.locals[&register].clone().into(),
            Either::Right(constant) => self.constant(constant).into(),
        }
    }

    fn source_origin(&self, instruction: usize) -> OriginSet {
        let source_line = self
            .bytecode
            .positions
            .get(instruction)
            .and_then(|position| usize::try_from(position.source).ok());
        [SourceOrigin::new(
            self.function.id,
            instruction,
            source_line,
            "Lua51",
        )]
        .into()
    }

    fn synthetic_origin(&self, opcode: &'static str) -> OriginSet {
        [SourceOrigin::new(
            self.function.id,
            self.bytecode.code.len(),
            None,
            opcode,
        )]
        .into()
    }

    // TODO: rename to one of: lift_instructions, lift_range, lift_instruction_range, lift_block?
    fn lift_instruction(
        &mut self,
        start: usize,
        end: usize,
        statements: &mut Vec<Statement>,
        origins: &mut Vec<OriginSet>,
    ) {
        if end > start {
            statements.reserve(end - start + 1);
        }
        let mut top: Option<(ast::RValue, u8)> = None;
        // TODO: we should consume the instructions, reducing clones
        let mut iter = self.bytecode.code[start..=end].iter().enumerate();
        while let Some((offset, instruction)) = iter.next() {
            let instruction_index = start + offset;
            let statement_start = statements.len();
            match instruction {
                Instruction::Move {
                    destination,
                    source,
                } => {
                    statements.push(
                        ast::Assign::new(
                            vec![self.locals[destination].clone().into()],
                            vec![self.locals[source].clone().into()],
                        )
                        .into(),
                    );
                }
                &Instruction::LoadBoolean {
                    destination, value, ..
                } => {
                    statements.push(
                        ast::Assign::new(
                            vec![self.locals[&destination].clone().into()],
                            vec![ast::Literal::Boolean(value).into()],
                        )
                        .into(),
                    );
                }
                &Instruction::LoadConstant {
                    destination,
                    source,
                } => {
                    statements.push(
                        ast::Assign::new(
                            vec![self.locals[&destination].clone().into()],
                            vec![self.constant(source).into()],
                        )
                        .into(),
                    );
                }
                Instruction::LoadNil(registers) => {
                    for register in registers {
                        statements.push(
                            ast::Assign::new(
                                vec![self.locals[register].clone().into()],
                                vec![ast::Literal::Nil.into()],
                            )
                            .into(),
                        );
                    }
                }
                &Instruction::GetGlobal {
                    destination,
                    global,
                } => {
                    let global_str = self.constant(global).as_string().unwrap().clone();
                    statements.push(
                        ast::Assign::new(
                            vec![self.locals[&destination].clone().into()],
                            vec![ast::Global::new(global_str).into()],
                        )
                        .into(),
                    );
                }
                &Instruction::SetGlobal { destination, value } => {
                    let global_str = self.constant(destination).as_string().unwrap().clone();
                    statements.push(
                        ast::Assign::new(
                            vec![ast::Global::new(global_str).into()],
                            vec![self.locals[&value].clone().into()],
                        )
                        .into(),
                    );
                }
                &Instruction::GetIndex {
                    destination,
                    object,
                    key,
                } => {
                    statements.push(
                        ast::Assign::new(
                            vec![self.locals[&destination].clone().into()],
                            vec![
                                ast::Index::new(
                                    self.locals[&object].clone().into(),
                                    self.register_or_constant(key),
                                )
                                .into(),
                            ],
                        )
                        .into(),
                    );
                }
                &Instruction::Test { value, invert } => {
                    let value = self.locals[&value].clone().into();
                    let condition = if invert {
                        ast::Unary::new(value, ast::UnaryOperation::Not).into()
                    } else {
                        value
                    };
                    statements.push(
                        ast::If::new(condition, ast::Block::default(), ast::Block::default())
                            .into(),
                    )
                }
                Instruction::Not {
                    destination,
                    operand,
                } => {
                    statements.push(
                        ast::Assign::new(
                            vec![self.locals[destination].clone().into()],
                            vec![
                                ast::Unary::new(
                                    self.locals[operand].clone().into(),
                                    ast::UnaryOperation::Not,
                                )
                                .into(),
                            ],
                        )
                        .into(),
                    );
                }
                Instruction::Length {
                    destination,
                    operand,
                } => {
                    statements.push(
                        ast::Assign::new(
                            vec![self.locals[destination].clone().into()],
                            vec![
                                ast::Unary::new(
                                    self.locals[operand].clone().into(),
                                    ast::UnaryOperation::Length,
                                )
                                .into(),
                            ],
                        )
                        .into(),
                    );
                }
                Instruction::Minus {
                    destination,
                    operand,
                } => {
                    statements.push(
                        ast::Assign::new(
                            vec![self.locals[destination].clone().into()],
                            vec![
                                ast::Unary::new(
                                    self.locals[operand].clone().into(),
                                    ast::UnaryOperation::Negate,
                                )
                                .into(),
                            ],
                        )
                        .into(),
                    );
                }
                &Instruction::Return(values, b) => {
                    let values = if b != 0 {
                        (values.0..values.0 + (b - 1))
                            .map(|r| self.locals[&Register(r)].clone().into())
                            .collect()
                    } else {
                        let (tail, end) = top.take().unwrap();
                        (values.0..end)
                            .map(|r| self.locals[&Register(r)].clone().into())
                            .chain(std::iter::once(tail))
                            .collect()
                    };
                    statements.push(ast::Return::new(values).into());
                }
                Instruction::Jump(..) => {}
                &Instruction::Add {
                    destination,
                    lhs,
                    rhs,
                }
                | &Instruction::Sub {
                    destination,
                    lhs,
                    rhs,
                }
                | &Instruction::Mul {
                    destination,
                    lhs,
                    rhs,
                }
                | &Instruction::Div {
                    destination,
                    lhs,
                    rhs,
                }
                | &Instruction::Mod {
                    destination,
                    lhs,
                    rhs,
                }
                | &Instruction::Pow {
                    destination,
                    lhs,
                    rhs,
                } => {
                    statements.push(
                        ast::Assign::new(
                            vec![self.locals[&destination].clone().into()],
                            vec![
                                ast::Binary::new(
                                    self.register_or_constant(lhs),
                                    self.register_or_constant(rhs),
                                    match instruction {
                                        Instruction::Add { .. } => ast::BinaryOperation::Add,
                                        Instruction::Sub { .. } => ast::BinaryOperation::Sub,
                                        Instruction::Mul { .. } => ast::BinaryOperation::Mul,
                                        Instruction::Div { .. } => ast::BinaryOperation::Div,
                                        Instruction::Mod { .. } => ast::BinaryOperation::Mod,
                                        Instruction::Pow { .. } => ast::BinaryOperation::Pow,
                                        _ => unreachable!(),
                                    },
                                )
                                .into(),
                            ],
                        )
                        .into(),
                    );
                }
                Instruction::Concatenate {
                    destination,
                    operands,
                } => {
                    assert!(operands.len() >= 2);
                    let mut operands = operands.into_iter().rev();

                    let right = operands.next().unwrap();
                    let left = operands.next().unwrap();
                    let mut concat = ast::Binary::new(
                        self.locals[left].clone().into(),
                        self.locals[right].clone().into(),
                        ast::BinaryOperation::Concat,
                    );
                    for r in operands {
                        concat = ast::Binary::new(
                            self.locals[r].clone().into(),
                            concat.into(),
                            ast::BinaryOperation::Concat,
                        );
                    }
                    statements.push(
                        ast::Assign::new(
                            vec![self.locals[destination].clone().into()],
                            vec![concat.into()],
                        )
                        .into(),
                    );
                }
                &Instruction::LessThan { lhs, rhs, invert } => {
                    let lhs = self.register_or_constant(lhs);
                    let rhs = self.register_or_constant(rhs);
                    let value = ast::Binary::new(lhs, rhs, ast::BinaryOperation::LessThan).into();
                    let condition = if invert {
                        ast::Unary::new(value, ast::UnaryOperation::Not).into()
                    } else {
                        value
                    };
                    statements.push(
                        ast::If::new(condition, ast::Block::default(), ast::Block::default())
                            .into(),
                    )
                }
                &Instruction::LessThanOrEqual { lhs, rhs, invert } => {
                    let lhs = self.register_or_constant(lhs);
                    let rhs = self.register_or_constant(rhs);
                    let value =
                        ast::Binary::new(lhs, rhs, ast::BinaryOperation::LessThanOrEqual).into();
                    let condition = if invert {
                        ast::Unary::new(value, ast::UnaryOperation::Not).into()
                    } else {
                        value
                    };
                    statements.push(
                        ast::If::new(condition, ast::Block::default(), ast::Block::default())
                            .into(),
                    )
                }
                &Instruction::Equal { lhs, rhs, invert } => {
                    let lhs = self.register_or_constant(lhs);
                    let rhs = self.register_or_constant(rhs);
                    let value = ast::Binary::new(lhs, rhs, ast::BinaryOperation::Equal).into();
                    let condition = if invert {
                        ast::Unary::new(value, ast::UnaryOperation::Not).into()
                    } else {
                        value
                    };
                    statements.push(
                        ast::If::new(condition, ast::Block::default(), ast::Block::default())
                            .into(),
                    )
                }
                Instruction::TestSet {
                    destination,
                    value,
                    invert,
                } => {
                    let value: ast::RValue = self.locals[value].clone().into();
                    statements.push(
                        ast::If::new(
                            if *invert {
                                ast::Unary {
                                    value: Box::new(value.clone()),
                                    operation: ast::UnaryOperation::Not,
                                }
                                .into()
                            } else {
                                value.clone()
                            },
                            ast::Block::default(),
                            ast::Block::default(),
                        )
                        .into(),
                    );

                    let assign = ast::Assign::new(
                        vec![self.locals[destination].clone().into()],
                        vec![value.clone()],
                    );

                    let origin = self.source_origin(instruction_index);
                    assert!(
                        self.insert_between
                            .insert(
                                self.nodes[&start],
                                (self.nodes[&(end + 1)], assign.into(), origin),
                            )
                            .is_none()
                    );
                }
                &Instruction::PrepMethodCall {
                    destination,
                    self_arg,
                    object,
                    method,
                } => {
                    let destination = self.locals[&destination].clone();
                    let self_arg = self.locals[&self_arg].clone();
                    let object = self.locals[&object].clone();
                    statements.push(
                        ast::Assign::new(vec![self_arg.into()], vec![object.clone().into()]).into(),
                    );
                    statements.push(
                        ast::Assign::new(
                            vec![destination.into()],
                            vec![
                                ast::Index::new(object.into(), self.register_or_constant(method))
                                    .into(),
                            ],
                        )
                        .into(),
                    );
                }
                &Instruction::TailCall {
                    function,
                    arguments,
                } => {
                    let arguments = if arguments != 0 {
                        (function.0 + 1..function.0 + arguments)
                            .map(|r| self.locals[&Register(r)].clone().into())
                            .collect()
                    } else {
                        let top = top.take().unwrap();
                        (function.0 + 1..top.1)
                            .map(|r| self.locals[&Register(r)].clone().into())
                            .chain(std::iter::once(top.0))
                            .collect()
                    };

                    let call = ast::Call::new(self.locals[&function].clone().into(), arguments);
                    statements.push(ast::Return::new(vec![call.into()]).into());
                }
                &Instruction::Call {
                    function,
                    arguments,
                    return_values,
                } => {
                    let arguments = if arguments != 0 {
                        (function.0 + 1..function.0 + arguments)
                            .map(|r| self.locals[&Register(r)].clone().into())
                            .collect()
                    } else {
                        let top = top.take().unwrap();
                        (function.0 + 1..top.1)
                            .map(|r| self.locals[&Register(r)].clone().into())
                            .chain(std::iter::once(top.0))
                            .collect()
                    };

                    let call = ast::Call::new(self.locals[&function].clone().into(), arguments);
                    if return_values != 0 {
                        if return_values == 1 {
                            statements.push(call.into());
                        } else {
                            statements.push(
                                ast::Assign::new(
                                    (function.0..function.0 + return_values - 1)
                                        .map(|r| self.locals[&Register(r)].clone().into())
                                        .collect_vec(),
                                    vec![ast::RValue::Select(call.into())],
                                )
                                .into(),
                            );
                        }
                    } else {
                        top = Some((call.into(), function.0));
                    }
                }
                Instruction::GetUpvalue {
                    destination,
                    upvalue,
                } => {
                    statements.push(
                        ast::Assign::new(
                            vec![self.locals[destination].clone().into()],
                            vec![self.upvalues[upvalue.0 as usize].clone().into()],
                        )
                        .into(),
                    );
                }
                Instruction::SetUpvalue {
                    destination,
                    source,
                } => {
                    statements.push(
                        ast::Assign::new(
                            vec![self.upvalues[destination.0 as usize].clone().into()],
                            vec![self.locals[source].clone().into()],
                        )
                        .into(),
                    );
                }
                &Instruction::VarArg(destination, b) => {
                    let vararg = ast::VarArg {};
                    if b != 0 {
                        statements.push(
                            ast::Assign::new(
                                (destination.0..destination.0 + b - 1)
                                    .map(|r| self.locals[&Register(r)].clone().into())
                                    .collect(),
                                vec![ast::RValue::Select(vararg.into())],
                            )
                            .into(),
                        );
                    } else {
                        top = Some((vararg.into(), destination.0));
                    }
                }
                // TODO: STYLE: rename to NewClosure?
                Instruction::Closure {
                    destination,
                    function,
                } => {
                    let closure = &self.bytecode.closures[function.0 as usize];

                    let mut upvalues_passed = Vec::with_capacity(closure.number_of_upvalues.into());
                    for _ in 0..closure.number_of_upvalues {
                        let (_, capture) = iter.next().unwrap();
                        let local = match capture {
                            Instruction::Move {
                                destination: _,
                                source,
                            } => self.locals[source].clone(),
                            Instruction::GetUpvalue {
                                destination: _,
                                upvalue,
                            } => self.upvalues[upvalue.0 as usize].clone(),
                            _ => panic!(),
                        };
                        upvalues_passed.push(local);
                    }

                    let ast_function = Arc::<Mutex<_>>::default();

                    let (function, upvalues, has_legacy_arg) =
                        Lifter::lift(closure, self.lifted_functions, self.next_function_id);
                    self.lifted_functions.push((
                        ast_function.clone(),
                        function,
                        upvalues,
                        has_legacy_arg,
                    ));

                    statements.push(
                        ast::Assign::new(
                            vec![self.locals[destination].clone().into()],
                            vec![
                                ast::Closure {
                                    function: ByAddress(ast_function),
                                    upvalues: upvalues_passed
                                        .into_iter()
                                        .map(ast::Upvalue::Ref)
                                        .collect(),
                                }
                                .into(),
                            ],
                        )
                        .into(),
                    );
                }
                Instruction::NewTable { destination, .. } => {
                    statements.push(
                        ast::Assign::new(
                            vec![self.locals[destination].clone().into()],
                            vec![ast::Table::default().into()],
                        )
                        .into(),
                    );
                }
                &Instruction::SetList {
                    table,
                    number_of_elements,
                    block_number,
                } => {
                    const FIELDS_PER_FLUSH: usize = 50;

                    let setlist = if number_of_elements != 0 {
                        ast::SetList::new(
                            self.locals[&table].clone(),
                            (block_number - 1) as usize * FIELDS_PER_FLUSH + 1,
                            (table.0 + 1..table.0 + 1 + number_of_elements)
                                .map(|r| self.locals[&Register(r)].clone().into())
                                .collect(),
                            None,
                        )
                    } else {
                        let top = top.take().unwrap();
                        ast::SetList::new(
                            self.locals[&table].clone(),
                            (block_number - 1) as usize * FIELDS_PER_FLUSH + 1,
                            (table.0 + 1..top.1)
                                .map(|r| self.locals[&Register(r)].clone().into())
                                .collect(),
                            Some(top.0),
                        )
                    };
                    statements.push(setlist.into());
                }
                Instruction::Close(start) => {
                    // TODO: REFACTOR: self.locals.iter() + skip
                    let locals = (start.0..self.bytecode.maximum_stack_size)
                        .map(|i| self.locals[&Register(i)].clone())
                        .collect();
                    statements.push(ast::Close { locals }.into());
                }
                &Instruction::SetIndex { object, key, value } => {
                    let key = self.register_or_constant(key);
                    let value = self.register_or_constant(value);

                    statements.push(
                        ast::Assign::new(
                            vec![
                                ast::Index {
                                    left: Box::new(self.locals[&object].clone().into()),
                                    right: Box::new(key),
                                }
                                .into(),
                            ],
                            vec![value],
                        )
                        .into(),
                    );
                }
                Instruction::InitNumericForLoop { control, .. } => {
                    let (internal_counter, limit, step) = (
                        self.locals[&control[0]].clone(),
                        self.locals[&control[1]].clone(),
                        self.locals[&control[2]].clone(),
                    );
                    statements.push(ast::NumForInit::new(internal_counter, limit, step).into());
                }
                &Instruction::IterateNumericForLoop { ref control, skip } => {
                    let (internal_counter, limit, step, external_counter) = (
                        self.locals[&control[0]].clone(),
                        self.locals[&control[1]].clone(),
                        self.locals[&control[2]].clone(),
                        self.locals[&control[3]].clone(),
                    );
                    statements.push(
                        ast::NumForNext::new(internal_counter.clone(), limit.into(), step.into())
                            .into(),
                    );

                    let body_node = self.get_node(
                        &((end + 1)
                            .checked_add_signed(skip.try_into().unwrap())
                            .unwrap()),
                    );
                    let origin = self.source_origin(instruction_index);
                    assert!(
                        self.insert_between
                            .insert(
                                self.nodes[&start],
                                (
                                    body_node,
                                    ast::Assign::new(
                                        vec![external_counter.into()],
                                        vec![internal_counter.into()],
                                    )
                                    .into(),
                                    origin,
                                )
                            )
                            .is_none()
                    );
                }
                Instruction::IterateGenericForLoop {
                    generator,
                    state,
                    internal_control,
                    vars,
                } => {
                    let generator = self.locals[generator].clone();
                    let state = self.locals[state].clone();
                    let internal_control = self.locals[internal_control].clone();
                    let vars = vars
                        .iter()
                        .map(|x| self.locals[x].clone())
                        .collect::<Vec<_>>();
                    let control = vars[0].clone();
                    statements.push(
                        ast::Assign::new(
                            vars.into_iter().map(|l| l.into()).collect(),
                            vec![
                                ast::Call::new(
                                    generator.clone().into(),
                                    vec![state.clone().into(), internal_control.clone().into()],
                                )
                                .into(),
                            ],
                        )
                        .into(),
                    );
                    statements.push(
                        ast::If::new(
                            ast::Binary::new(
                                control.clone().into(),
                                ast::Literal::Nil.into(),
                                ast::BinaryOperation::NotEqual,
                            )
                            .into(),
                            ast::Block::default(),
                            ast::Block::default(),
                        )
                        .into(),
                    );

                    let body_node = self.get_node(&(end + 1));
                    let origin = self.source_origin(instruction_index);
                    assert!(
                        self.insert_between
                            .insert(
                                self.nodes[&start],
                                (
                                    body_node,
                                    ast::Assign::new(
                                        vec![internal_control.clone().into()],
                                        vec![control.clone().into()],
                                    )
                                    .into(),
                                    origin,
                                )
                            )
                            .is_none()
                    );
                }
                Instruction::ExtraWord(_) => {}
            }

            let origin = self.source_origin(instruction_index);
            origins.extend(
                std::iter::repeat_n(origin, statements.len().saturating_sub(statement_start)),
            );

            if matches!(
                instruction,
                Instruction::Return { .. } | Instruction::TailCall { .. }
            ) {
                break;
            }
        }
    }

    // TODO: REFACTOR: this function doesnt need to exist
    fn get_node(&'a self, index: &'a usize) -> NodeIndex {
        self.nodes[index]
    }

    fn lift_blocks(&mut self) {
        let ranges = self.code_ranges();
        for (start, end) in ranges {
            if start == self.bytecode.code.len() {
                self.statement_origins
                    .insert(self.nodes[&start], Vec::new());
                continue;
            }
            // TODO: gotta be a better way
            // we need to do this in case that the body of a for loop is after the for loop instruction
            // see: IterateNumericForLoop
            let mut statements =
                std::mem::take(self.function.block_mut(self.nodes[&start]).unwrap());
            let mut origins = Vec::new();
            self.lift_instruction(start, end, &mut statements, &mut origins);
            *self.function.block_mut(self.nodes[&start]).unwrap() = statements;
            self.statement_origins.insert(self.nodes[&start], origins);

            match self.bytecode.code[end] {
                Instruction::Equal { .. }
                | Instruction::LessThan { .. }
                | Instruction::LessThanOrEqual { .. }
                | Instruction::Test { .. }
                | Instruction::TestSet { .. }
                | Instruction::IterateGenericForLoop { .. } => {
                    self.function.set_edges(
                        self.nodes[&start],
                        vec![
                            (self.get_node(&(end + 1)), BlockEdge::new(BranchType::Then)),
                            (self.get_node(&(end + 2)), BlockEdge::new(BranchType::Else)),
                        ],
                    );
                }
                Instruction::IterateNumericForLoop { skip, .. } => {
                    self.function.set_edges(
                        self.nodes[&start],
                        vec![
                            (
                                self.get_node(
                                    &((end + 1)
                                        .checked_add_signed(skip.try_into().unwrap())
                                        .unwrap()),
                                ),
                                BlockEdge::new(BranchType::Then),
                            ),
                            (self.get_node(&(end + 1)), BlockEdge::new(BranchType::Else)),
                        ],
                    );
                }
                Instruction::Jump(skip) | Instruction::InitNumericForLoop { skip, .. } => {
                    self.function.set_edges(
                        self.nodes[&start],
                        vec![(
                            self.get_node(
                                &((end + 1)
                                    .checked_add_signed(skip.try_into().unwrap())
                                    .unwrap()),
                            ),
                            BlockEdge::new(BranchType::Unconditional),
                        )],
                    );
                }
                Instruction::Return { .. } | Instruction::TailCall { .. } => {}
                Instruction::LoadBoolean { skip_next, .. } => {
                    let successor = self.get_node(&(end + 1 + skip_next as usize));
                    self.function.set_edges(
                        self.nodes[&start],
                        vec![(successor, BlockEdge::new(BranchType::Unconditional))],
                    );
                }
                _ => {
                    if end + 1 != self.bytecode.code.len() {
                        self.function.set_edges(
                            self.nodes[&start],
                            vec![(
                                self.get_node(&(end + 1)),
                                BlockEdge::new(BranchType::Unconditional),
                            )],
                        );
                    }
                }
            }
        }
    }

    pub fn lift(
        bytecode: &'a BytecodeFunction,
        lifted_functions:
            &'b mut Vec<(Arc<Mutex<ast::Function>>, Function, Vec<RcLocal>, bool)>,
        next_function_id: &'b mut usize,
    ) -> (Function, Vec<RcLocal>, bool) {
        let function_id = *next_function_id;
        *next_function_id = next_function_id
            .checked_add(1)
            .expect("function identifier overflow");
        let mut function = Function::new(function_id);
        function.is_variadic = bytecode.vararg_flag & VARARG_ISVARARG != 0;
        let mut context = Self {
            bytecode,
            nodes: FxHashMap::default(),
            insert_between: FxHashMap::default(),
            statement_origins: FxHashMap::default(),
            locals: FxHashMap::default(),
            constants: FxHashMap::default(),
            function,
            upvalues: Vec::new(),
            lifted_functions,
            next_function_id,
        };

        context.create_block_map();
        context.allocate_locals();
        context.lift_blocks();

        // TODO: STYLE: instead of naming NodeIndex vars `{}_node`, we should name them
        // `{}_index`, or if it's the corresponding var for `block`, `block_index`
        let stack_init_node = context.function.new_block();
        let stack_init_origin = context.synthetic_origin("Lua51StackInit");
        let mut stack_init_origins = Vec::new();
        let stack_init_block = context.function.block_mut(stack_init_node).unwrap();
        stack_init_block.reserve(context.locals.len());
        for (_, local) in context.locals {
            if !context.function.parameters.contains(&local) {
                let stack_init_block = context.function.block_mut(stack_init_node).unwrap();
                stack_init_block.push(
                    ast::Assign::new(vec![local.into()], vec![ast::Literal::Nil.into()]).into(),
                );
                stack_init_origins.push(stack_init_origin.clone());
            }
        }
        context
            .statement_origins
            .insert(stack_init_node, stack_init_origins);
        context.function.set_edges(
            stack_init_node,
            vec![(context.nodes[&0], BlockEdge::new(BranchType::Unconditional))],
        );
        context.function.set_entry(stack_init_node);

        for (node, (successor, stat, origin)) in context.insert_between {
            if context.function.predecessor_blocks(successor).count() == 1 {
                context
                    .function
                    .block_mut(successor)
                    .unwrap()
                    .insert(0, stat);
                context
                    .statement_origins
                    .entry(successor)
                    .or_default()
                    .insert(0, origin);
            } else {
                let between_node = context.function.new_block();
                context.function.block_mut(between_node).unwrap().push(stat);
                context
                    .statement_origins
                    .insert(between_node, vec![origin]);
                context.function.set_edges(
                    between_node,
                    vec![(successor, BlockEdge::new(BranchType::Unconditional))],
                );
                for edge in context
                    .function
                    .graph()
                    .edges_directed(node, Direction::Outgoing)
                    .filter(|e| e.target() == successor)
                    .map(|e| e.id())
                    .collect::<Vec<_>>()
                {
                    let edge = context.function.graph_mut().remove_edge(edge).unwrap();
                    context
                        .function
                        .graph_mut()
                        .add_edge(node, between_node, edge);
                }
            }
        }

        for (node, origins) in std::mem::take(&mut context.statement_origins) {
            context.function.set_statement_origins(node, origins);
        }

        (
            context.function,
            context.upvalues,
            bytecode.vararg_flag & VARARG_NEEDSARG != 0,
        )
    }
}

#[cfg(test)]
mod tests {
    use lua51_deserializer::{
        Function as BytecodeFunction, Instruction, Value,
        argument::{Constant, Register},
        instruction::position::Position,
        local::Local as DebugLocal,
    };

    use super::Lifter;

    fn vararg_function(flag: u8) -> BytecodeFunction<'static> {
        BytecodeFunction {
            name: b"vararg",
            line_defined: 0,
            last_line_defined: 0,
            number_of_upvalues: 0,
            vararg_flag: flag,
            maximum_stack_size: 1,
            code: vec![Instruction::Return(Register(0), 2)],
            constants: Vec::new(),
            closures: Vec::new(),
            positions: Vec::new(),
            locals: Vec::new(),
            upvalues: Vec::new(),
            number_of_parameters: 0,
        }
    }

    #[test]
    fn lifted_function_supplies_binding_and_origin_metadata_to_ssa() {
        let bytecode = BytecodeFunction {
            name: b"metadata",
            line_defined: 0,
            last_line_defined: 0,
            number_of_upvalues: 0,
            vararg_flag: 0,
            maximum_stack_size: 1,
            code: vec![
                Instruction::LoadConstant {
                    destination: Register(0),
                    source: Constant(0),
                },
                Instruction::Return(Register(0), 2),
            ],
            constants: vec![Value::Number(7.0)],
            closures: Vec::new(),
            positions: Vec::new(),
            locals: Vec::new(),
            upvalues: Vec::new(),
            number_of_parameters: 0,
        };
        let mut lifted = Vec::new();
        let mut next_function_id = 0;
        let (mut function, upvalues, _) =
            Lifter::lift(&bytecode, &mut lifted, &mut next_function_id);

        cfg::ssa::construct(&mut function, &upvalues).expect("SSA construction");
    }

    #[test]
    fn legacy_vararg_arg_table_is_an_implicit_named_parameter() {
        let bytecode = vararg_function(7);
        let mut lifted = Vec::new();
        let mut next_function_id = 0;

        let (mut function, upvalues, has_legacy_arg) =
            Lifter::lift(&bytecode, &mut lifted, &mut next_function_id);

        assert!(has_legacy_arg);
        assert!(function.is_variadic);
        assert_eq!(function.parameters.len(), 1);
        assert_eq!(function.parameters[0].to_string(), "arg");
        assert!(function.block(function.entry().unwrap()).unwrap().is_empty());
        cfg::ssa::construct(&mut function, &upvalues).expect("SSA construction");
    }

    #[test]
    fn modern_vararg_mode_does_not_invent_an_arg_table() {
        let bytecode = vararg_function(2);
        let mut lifted = Vec::new();
        let mut next_function_id = 0;

        let (function, _, has_legacy_arg) =
            Lifter::lift(&bytecode, &mut lifted, &mut next_function_id);

        assert!(!has_legacy_arg);
        assert!(function.is_variadic);
        assert!(function.parameters.is_empty());
        assert_eq!(function.block(function.entry().unwrap()).unwrap().len(), 1);
    }

    #[test]
    fn statement_origins_use_bytecode_pcs_across_blocks() {
        let bytecode = BytecodeFunction {
            name: b"origins",
            line_defined: 0,
            last_line_defined: 0,
            number_of_upvalues: 0,
            vararg_flag: 0,
            maximum_stack_size: 1,
            code: vec![
                Instruction::Test {
                    value: Register(0),
                    invert: false,
                },
                Instruction::Jump(1),
                Instruction::LoadConstant {
                    destination: Register(0),
                    source: Constant(0),
                },
                Instruction::Return(Register(0), 2),
            ],
            constants: vec![Value::Number(7.0)],
            closures: Vec::new(),
            positions: (0..4)
                .map(|instruction| Position {
                    instruction,
                    source: 10 + instruction as u32,
                })
                .collect(),
            locals: vec![DebugLocal {
                name: b"x",
                range: 2..4,
            }],
            upvalues: Vec::new(),
            number_of_parameters: 0,
        };
        let mut lifted = Vec::new();
        let mut next_function_id = 0;
        let (mut function, upvalues, _) =
            Lifter::lift(&bytecode, &mut lifted, &mut next_function_id);

        let mut load_origin = None;
        for (node, block) in function.blocks() {
            for (statement, value) in block.iter().enumerate() {
                if value.as_assign().is_some_and(|assign| {
                    matches!(
                        assign.right.as_slice(),
                        [ast::RValue::Literal(ast::Literal::Number(7.0))]
                    )
                }) {
                    load_origin = function.statement_origins(node, statement);
                }
            }
        }
        let load_origin = load_origin.expect("LOADK statement origin");
        assert!(load_origin.iter().any(|origin| {
            origin.instruction == 2 && origin.source_line == Some(12)
        }));

        cfg::ssa::construct(&mut function, &upvalues).expect("SSA construction");
        assert!(function.blocks().any(|(_, block)| {
            block.iter().any(|statement| {
                ast::LocalRw::values_written(statement)
                    .iter()
                    .any(|local| local.to_string() == "x")
            })
        }));
    }

    #[test]
    fn testset_assignment_is_confined_to_its_cfg_edge() {
        let bytecode = BytecodeFunction {
            name: b"testset",
            line_defined: 0,
            last_line_defined: 0,
            number_of_upvalues: 0,
            vararg_flag: 0,
            maximum_stack_size: 2,
            code: vec![
                Instruction::Test {
                    value: Register(0),
                    invert: false,
                },
                Instruction::Jump(1),
                Instruction::TestSet {
                    destination: Register(1),
                    value: Register(0),
                    invert: false,
                },
                Instruction::Jump(0),
                Instruction::Return(Register(1), 2),
            ],
            constants: Vec::new(),
            closures: Vec::new(),
            positions: Vec::new(),
            locals: Vec::new(),
            upvalues: Vec::new(),
            number_of_parameters: 0,
        };
        let mut lifted = Vec::new();
        let mut next_function_id = 0;
        let (function, _, _) = Lifter::lift(&bytecode, &mut lifted, &mut next_function_id);

        let assignment_nodes = function
            .blocks()
            .filter_map(|(node, block)| {
                block
                    .iter()
                    .any(|statement| {
                        statement.as_assign().is_some_and(|assign| {
                            matches!(assign.right.as_slice(), [ast::RValue::Local(_)])
                        })
                    })
                    .then_some(node)
            })
            .collect::<Vec<_>>();
        assert_eq!(assignment_nodes.len(), 1);
        let assignment_node = assignment_nodes[0];
        assert_eq!(function.predecessor_blocks(assignment_node).count(), 1);

        let return_node = function
            .blocks()
            .find_map(|(node, block)| {
                block
                    .iter()
                    .any(|statement| statement.as_return().is_some())
                    .then_some(node)
            })
            .expect("return block");
        assert_ne!(assignment_node, return_node);
        assert_eq!(function.predecessor_blocks(return_node).count(), 2);
    }

    #[test]
    fn tailcall_lifts_as_a_terminal_return() {
        let bytecode = BytecodeFunction {
            name: b"tailcall",
            line_defined: 0,
            last_line_defined: 0,
            number_of_upvalues: 0,
            vararg_flag: 0,
            maximum_stack_size: 1,
            code: vec![Instruction::TailCall {
                function: Register(0),
                arguments: 1,
            }],
            constants: Vec::new(),
            closures: Vec::new(),
            positions: Vec::new(),
            locals: Vec::new(),
            upvalues: Vec::new(),
            number_of_parameters: 1,
        };
        let mut lifted = Vec::new();
        let mut next_function_id = 0;

        let (function, _, _) = Lifter::lift(&bytecode, &mut lifted, &mut next_function_id);

        assert!(function.blocks().any(|(_, block)| {
            block.iter().any(|statement| {
                statement.as_return().is_some_and(|value| {
                    matches!(value.values.as_slice(), [ast::RValue::Call(_)])
                })
            })
        }));
    }

    #[test]
    fn tailcall_ignores_the_compiler_return_epilogue() {
        let bytecode = BytecodeFunction {
            name: b"tailcall-epilogue",
            line_defined: 0,
            last_line_defined: 0,
            number_of_upvalues: 0,
            vararg_flag: 0,
            maximum_stack_size: 1,
            code: vec![
                Instruction::TailCall {
                    function: Register(0),
                    arguments: 1,
                },
                Instruction::Return(Register(0), 0),
            ],
            constants: Vec::new(),
            closures: Vec::new(),
            positions: Vec::new(),
            locals: Vec::new(),
            upvalues: Vec::new(),
            number_of_parameters: 1,
        };
        let mut lifted = Vec::new();
        let mut next_function_id = 0;

        let (function, _, _) = Lifter::lift(&bytecode, &mut lifted, &mut next_function_id);

        let returns = function
            .blocks()
            .flat_map(|(_, block)| block.iter())
            .filter_map(ast::Statement::as_return)
            .collect::<Vec<_>>();
        assert_eq!(returns.len(), 1);
        assert!(matches!(
            returns[0].values.as_slice(),
            [ast::RValue::Call(_)]
        ));
    }
}
