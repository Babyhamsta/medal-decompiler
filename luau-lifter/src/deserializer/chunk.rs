use super::{function::Function, list::parse_list, parse_string, version::BytecodeVersion};
use nom::{
    Err, IResult,
    bytes::complete::take,
    error::{Error, ErrorKind},
    number::complete::le_u8,
};
use nom_leb128::leb128_usize;

#[derive(Debug)]
pub struct Chunk {
    pub string_table: Vec<Vec<u8>>,
    pub functions: Vec<Function>,
    pub main: usize,
}

impl Chunk {
    pub(crate) fn parse(
        input: &[u8],
        encode_key: u8,
        version: BytecodeVersion,
    ) -> IResult<&[u8], Self> {
        let (input, types_version) = le_u8(input)?;
        if !(1..=3).contains(&types_version) {
            return Err(Err::Failure(Error::new(input, ErrorKind::Verify)));
        }
        let (input, string_table) = parse_list(input, parse_string)?;
        let mut input = input;
        if types_version == 3 {
            loop {
                let (next, index) = le_u8(input)?;
                input = next;
                if index == 0 {
                    break;
                }
                let (next, _) = leb128_usize(input)?;
                input = next;
            }
        }

        let (next, function_count) = leb128_usize(input)?;
        input = next;
        let mut functions = Vec::with_capacity(function_count);
        for _ in 0..function_count {
            if version.has_sized_prototypes() {
                let (next, prototype_size) = leb128_usize(input)?;
                let (outer, prototype_input) = take(prototype_size)(next)?;
                let (_, function) = Function::parse(prototype_input, encode_key, version)?;
                functions.push(function);
                input = outer;
            } else {
                let (next, function) = Function::parse(input, encode_key, version)?;
                functions.push(function);
                input = next;
            }
        }
        let (input, main) = leb128_usize(input)?;

        if main >= functions.len() {
            return Err(Err::Failure(Error::new(input, ErrorKind::Verify)));
        }

        Ok((
            input,
            Self {
                string_table,
                functions,
                main,
            },
        ))
    }
}
