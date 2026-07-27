use nom::{
    Err, IResult,
    error::{Error, ErrorKind},
    number::complete::{le_i32, le_u8, le_u32},
};
use nom_leb128::{leb128_u64, leb128_usize};

use super::{
    constant::Constant,
    list::{parse_list, parse_list_len},
    version::BytecodeVersion,
};

use crate::{instruction::*, op_code::OpCode};

#[derive(Debug)]
pub struct DebugLocal {
    pub name: usize,
    pub start_pc: usize,
    pub end_pc: usize,
    pub register: u8,
}

#[derive(Debug)]
pub struct FeedbackSlot {
    pub kind: u8,
    pub pc: usize,
}

#[derive(Debug)]
pub struct Function {
    pub max_stack_size: u8,
    pub num_parameters: u8,
    pub num_upvalues: u8,
    pub is_vararg: bool,
    pub flags: u8,
    //pub instructions: Vec<u32>,
    pub instructions: Vec<Instruction>,
    pub constants: Vec<Constant>,
    pub functions: Vec<usize>,
    pub line_defined: usize,
    pub function_name: usize,
    pub line_gap_log2: Option<u8>,
    pub line_info_delta: Option<Vec<u8>>,
    pub abs_line_info_delta: Option<Vec<i32>>,
    pub debug_locals: Vec<DebugLocal>,
    pub debug_upvalues: Vec<usize>,
    pub feedback: Vec<FeedbackSlot>,
    pub cost: Option<u64>,
}

impl Function {
    fn parse_instructions(
        words: &[u32],
        encode_key: u8,
        version: BytecodeVersion,
    ) -> Result<Vec<Instruction>, String> {
        let mut instructions = Vec::with_capacity(words.len());
        let mut pc = 0;

        while pc < words.len() {
            let instruction = Instruction::parse(words[pc], encode_key, version)?;
            let opcode = instruction.op_code();
            if opcode.has_aux() {
                let aux = *words
                    .get(pc + 1)
                    .ok_or_else(|| format!("missing AUX word after {opcode:?} at pc {pc}"))?;
                instructions.push(instruction.with_aux(aux)?);
                for _ in 1..opcode.word_len() {
                    instructions.push(Instruction::BC {
                        op_code: OpCode::LOP_NOP,
                        a: 0,
                        b: 0,
                        c: 0,
                        aux: 0,
                    });
                }
                pc += opcode.word_len();
            } else {
                instructions.push(instruction);
                pc += 1;
            }
        }

        Ok(instructions)
    }

    pub(crate) fn parse(
        input: &[u8],
        encode_key: u8,
        version: BytecodeVersion,
    ) -> IResult<&[u8], Self> {
        let (input, max_stack_size) = le_u8(input)?;
        let (input, num_parameters) = le_u8(input)?;
        let (input, num_upvalues) = le_u8(input)?;
        let (input, is_vararg) = le_u8(input)?;

        let (input, flags) = le_u8(input)?;
        let (input, _) = parse_list(input, le_u8)?;

        let (input, u32_instructions) = parse_list(input, le_u32)?;
        //let (input, instructions) = parse_list(input, Function::parse_instrution)?;
        if u32_instructions.is_empty() {
            return Err(Err::Failure(Error::new(input, ErrorKind::Verify)));
        }
        let instructions = Self::parse_instructions(&u32_instructions, encode_key, version)
            .map_err(|_| Err::Failure(Error::new(input, ErrorKind::Verify)))?;
        let (input, constants) = parse_list(input, |i| Constant::parse(i, version))?;
        let (input, functions) = parse_list(input, leb128_usize)?;
        let (input, line_defined) = leb128_usize(input)?;
        let (input, function_name) = leb128_usize(input)?;
        let (input, has_line_info) = le_u8(input)?;
        let (input, line_gap_log2) = match has_line_info {
            0 => (input, None),
            _ => {
                let (input, line_gap_log2) = le_u8(input)?;
                (input, Some(line_gap_log2))
            }
        };
        let (input, line_info_delta) = match has_line_info {
            0 => (input, None),
            _ => {
                let (input, line_info_delta) =
                    parse_list_len(input, le_u8, u32_instructions.len())?;
                (input, Some(line_info_delta))
            }
        };
        let (input, abs_line_info_delta) = match has_line_info {
            0 => (input, None),
            _ => {
                let interval_count = if u32_instructions.is_empty() {
                    0
                } else {
                    ((u32_instructions.len() - 1) >> line_gap_log2.unwrap()) + 1
                };
                let (input, abs_line_info_delta) = parse_list_len(input, le_i32, interval_count)?;
                (input, Some(abs_line_info_delta))
            }
        };
        let (mut input, has_debug_info) = le_u8(input)?;
        let mut debug_locals = Vec::new();
        let mut debug_upvalues = Vec::new();
        if has_debug_info != 0 {
            let (next, num_locvars) = leb128_usize(input)?;
            input = next;
            debug_locals.reserve(num_locvars);
            for _ in 0..num_locvars {
                let (next, name) = leb128_usize(input)?;
                let (next, start_pc) = leb128_usize(next)?;
                let (next, end_pc) = leb128_usize(next)?;
                let (next, register) = le_u8(next)?;
                input = next;
                debug_locals.push(DebugLocal {
                    name,
                    start_pc,
                    end_pc,
                    register,
                });
            }
            let (next, num_debug_upvalues) = leb128_usize(input)?;
            input = next;
            debug_upvalues.reserve(num_debug_upvalues);
            for _ in 0..num_debug_upvalues {
                let (next, name) = leb128_usize(input)?;
                input = next;
                debug_upvalues.push(name);
            }
        }

        let mut feedback = Vec::new();
        if version.has_feedback() {
            let (next, feedback_count) = leb128_usize(input)?;
            input = next;
            feedback.reserve(feedback_count);
            for _ in 0..feedback_count {
                let (next, kind) = le_u8(input)?;
                let (next, pc) = leb128_usize(next)?;
                input = next;
                feedback.push(FeedbackSlot { kind, pc });
            }
        }

        let mut cost = None;
        if version.has_cost() && flags & 0x08 != 0 {
            let (next, value) = leb128_u64(input)?;
            input = next;
            cost = Some(value);
        }

        Ok((
            input,
            Self {
                max_stack_size,
                num_parameters,
                num_upvalues,
                is_vararg: is_vararg != 0u8,
                flags,
                instructions,
                constants,
                functions,
                line_defined,
                function_name,
                line_gap_log2,
                line_info_delta,
                abs_line_info_delta,
                debug_locals,
                debug_upvalues,
                feedback,
                cost,
            },
        ))
    }
}
