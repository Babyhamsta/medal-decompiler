use nom::{
    Err, IResult,
    error::{Error, ErrorKind},
    number::complete::{le_u8, le_u32},
};

use crate::{
    bounded_count,
    instruction::{Instruction, position::Position},
    local::Local,
    value::{self, Value},
};

#[derive(Debug)]
pub struct Function<'a> {
    pub name: &'a [u8],
    pub line_defined: u32,
    pub last_line_defined: u32,
    pub number_of_upvalues: u8,
    pub vararg_flag: u8,
    pub maximum_stack_size: u8,
    pub code: Vec<Instruction>,
    pub constants: Vec<Value<'a>>,
    pub closures: Vec<Function<'a>>,
    pub positions: Vec<Position>,
    pub locals: Vec<Local<'a>>,
    pub upvalues: Vec<&'a [u8]>,
    pub number_of_parameters: u8,
}

impl<'a> Function<'a> {
    pub fn parse(input: &'a [u8]) -> IResult<&'a [u8], Self> {
        Self::parse_with_size_t(input, 4)
    }

    pub(crate) fn parse_with_size_t(
        input: &'a [u8],
        size_t_width: u8,
    ) -> IResult<&'a [u8], Self> {
        Self::parse_with_depth(input, 0, size_t_width)
    }

    fn parse_with_depth(
        input: &'a [u8],
        depth: usize,
        size_t_width: u8,
    ) -> IResult<&'a [u8], Self> {
        const MAX_DEPTH: usize = 256;
        if depth > MAX_DEPTH {
            return Err(Err::Failure(Error::new(input, ErrorKind::TooLarge)));
        }
        let (input, name) = value::parse_string_with_size_t(input, size_t_width)?;
        let (input, line_defined) = le_u32(input)?;
        let (input, last_line_defined) = le_u32(input)?;
        let (input, number_of_upvalues) = le_u8(input)?;
        let (input, number_of_parameters) = le_u8(input)?;
        let (input, vararg_flag) = le_u8(input)?;
        let (input, maximum_stack_size) = le_u8(input)?;
        let (input, code_length) = le_u32(input)?;
        let (input, code) = parse_code(input, code_length as usize)?;
        let (input, constants_length) = le_u32(input)?;
        let (input, constants) = bounded_count(input, constants_length as usize, 1, |input| {
            Value::parse_with_size_t(input, size_t_width)
        })?;
        let (input, closures_length) = le_u32(input)?;
        let minimum_function_size = usize::from(size_t_width) + 36;
        let (input, closures) = bounded_count(
            input,
            closures_length as usize,
            minimum_function_size,
            |input| {
                Self::parse_with_depth(input, depth + 1, size_t_width)
            },
        )?;
        let (input, positions) = Position::parse(input)?;
        let (input, locals) = Local::parse_list_with_size_t(input, size_t_width)?;
        let (input, upvalues) = value::parse_strings_with_size_t(input, size_t_width)?;

        Ok((
            input,
            Self {
                name,
                line_defined,
                last_line_defined,
                number_of_upvalues,
                vararg_flag,
                maximum_stack_size,
                code,
                constants,
                closures,
                positions,
                locals,
                upvalues,
                number_of_parameters,
            },
        ))
    }
}

fn parse_code(input: &[u8], word_count: usize) -> IResult<&[u8], Vec<Instruction>> {
    if word_count > input.len() / 4 {
        return Err(Err::Failure(Error::new(input, ErrorKind::TooLarge)));
    }
    let mut code = Vec::new();
    code.try_reserve_exact(word_count)
        .map_err(|_| Err::Failure(Error::new(input, ErrorKind::TooLarge)))?;
    let mut remaining = input;
    while code.len() < word_count {
        let (next, mut instruction) = Instruction::parse(remaining)?;
        remaining = next;
        if matches!(
            instruction,
            Instruction::SetList {
                block_number: 0,
                ..
            }
        ) {
            if code.len() + 1 >= word_count {
                return Err(Err::Failure(Error::new(remaining, ErrorKind::Eof)));
            }
            let (next, block_number) = le_u32(remaining)?;
            if block_number == 0 || block_number > i32::MAX as u32 {
                return Err(Err::Failure(Error::new(remaining, ErrorKind::Verify)));
            }
            if let Instruction::SetList {
                block_number: target,
                ..
            } = &mut instruction
            {
                *target = block_number;
            }
            code.push(instruction);
            code.push(Instruction::ExtraWord(block_number));
            remaining = next;
        } else {
            code.push(instruction);
        }
    }
    Ok((remaining, code))
}
