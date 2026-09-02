use std::fmt;

use nom::{
    Err, IResult,
    error::{Error, ErrorKind},
};

pub use function::Function;
pub use instruction::{Instruction, argument};
pub use value::Value;

pub mod chunk;
pub mod function;
pub mod instruction;
pub mod local;
pub mod value;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DeserializeError {
    pub offset: usize,
    pub detail: String,
}

impl fmt::Display for DeserializeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "byte offset {}: {}", self.offset, self.detail)
    }
}

impl std::error::Error for DeserializeError {}

pub fn deserialize(input: &[u8]) -> Result<chunk::Chunk<'_>, DeserializeError> {
    match chunk::Chunk::parse(input) {
        Ok((remaining, chunk)) if remaining.is_empty() => Ok(chunk),
        Ok((remaining, _)) => Err(DeserializeError {
            offset: input.len().saturating_sub(remaining.len()),
            detail: format!("{} trailing bytes after chunk", remaining.len()),
        }),
        Err(Err::Error(error) | Err::Failure(error)) => Err(DeserializeError {
            offset: input.len().saturating_sub(error.input.len()),
            detail: format!("invalid Lua 5.1 chunk ({:?})", error.code),
        }),
        Err(Err::Incomplete(_)) => Err(DeserializeError {
            offset: input.len(),
            detail: "truncated Lua 5.1 chunk".to_owned(),
        }),
    }
}

pub(crate) fn bounded_count<'a, O, F>(
    input: &'a [u8],
    count: usize,
    minimum_width: usize,
    mut parser: F,
) -> IResult<&'a [u8], Vec<O>>
where
    F: FnMut(&'a [u8]) -> IResult<&'a [u8], O>,
{
    const PREALLOC_CHUNK: usize = 4096;

    if minimum_width == 0 || count > input.len() / minimum_width {
        return Err(Err::Failure(Error::new(input, ErrorKind::TooLarge)));
    }
    let mut values = Vec::new();
    let mut remaining = input;
    for _ in 0..count {
        if values.len() == values.capacity() {
            values
                .try_reserve_exact((count - values.len()).min(PREALLOC_CHUNK))
                .map_err(|_| Err::Failure(Error::new(remaining, ErrorKind::TooLarge)))?;
        }
        let before = remaining.len();
        let (next, value) = parser(remaining)?;
        if next.len() >= before {
            return Err(Err::Failure(Error::new(remaining, ErrorKind::Many0)));
        }
        values.push(value);
        remaining = next;
    }
    Ok((remaining, values))
}

#[cfg(test)]
mod tests {
    use super::{Instruction, deserialize};

    fn push_u32(output: &mut Vec<u8>, value: u32) {
        output.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u64(output: &mut Vec<u8>, value: u64) {
        output.extend_from_slice(&value.to_le_bytes());
    }

    fn header() -> Vec<u8> {
        vec![0x1b, b'L', b'u', b'a', 0x51, 0, 1, 4, 4, 4, 8, 0]
    }

    fn instruction_abc(opcode: u32, a: u32, b: u32, c: u32) -> u32 {
        opcode | (a << 6) | (c << 14) | (b << 23)
    }

    fn chunk_with_code(code: &[u32]) -> Vec<u8> {
        let mut output = header();
        push_u32(&mut output, 0);
        push_u32(&mut output, 0);
        push_u32(&mut output, 0);
        output.extend_from_slice(&[0, 0, 0, 2]);
        push_u32(&mut output, code.len() as u32);
        for word in code {
            push_u32(&mut output, *word);
        }
        push_u32(&mut output, 0);
        push_u32(&mut output, 0);
        push_u32(&mut output, 0);
        push_u32(&mut output, 0);
        push_u32(&mut output, 0);
        output
    }

    #[test]
    fn parses_minimal_chunk_and_rejects_trailing_bytes() {
        let chunk = chunk_with_code(&[instruction_abc(30, 0, 1, 0)]);
        assert!(deserialize(&chunk).is_ok());

        let mut trailing = chunk;
        trailing.push(0);
        let error = deserialize(&trailing).unwrap_err();
        assert!(error.detail.contains("trailing bytes"));
    }

    #[test]
    fn parses_little_endian_chunk_with_64_bit_size_t() {
        let mut chunk = header();
        chunk[8] = 8;
        push_u64(&mut chunk, 0);
        push_u32(&mut chunk, 0);
        push_u32(&mut chunk, 0);
        chunk.extend_from_slice(&[0, 0, 0, 2]);
        push_u32(&mut chunk, 1);
        push_u32(&mut chunk, instruction_abc(30, 0, 1, 0));
        push_u32(&mut chunk, 0);
        push_u32(&mut chunk, 0);
        push_u32(&mut chunk, 0);
        push_u32(&mut chunk, 0);
        push_u32(&mut chunk, 0);

        assert!(deserialize(&chunk).is_ok());
    }

    #[test]
    fn unsupported_header_returns_error_without_panicking() {
        let mut chunk = chunk_with_code(&[instruction_abc(30, 0, 1, 0)]);
        chunk[4] = 0x52;

        let result = std::panic::catch_unwind(|| deserialize(&chunk));

        assert!(result.is_ok());
        assert!(result.unwrap().is_err());
    }

    #[test]
    fn impossible_code_count_fails_before_allocation() {
        let mut chunk = header();
        push_u32(&mut chunk, 0);
        push_u32(&mut chunk, 0);
        push_u32(&mut chunk, 0);
        chunk.extend_from_slice(&[0, 0, 0, 2]);
        push_u32(&mut chunk, u32::MAX);

        let error = deserialize(&chunk).unwrap_err();

        assert!(error.detail.contains("TooLarge"));
    }

    #[test]
    fn impossible_closure_count_fails_before_allocation() {
        let mut chunk = header();
        push_u32(&mut chunk, 0);
        push_u32(&mut chunk, 0);
        push_u32(&mut chunk, 0);
        chunk.extend_from_slice(&[0, 0, 0, 2]);
        push_u32(&mut chunk, 1);
        push_u32(&mut chunk, instruction_abc(30, 0, 1, 0));
        push_u32(&mut chunk, 0);
        push_u32(&mut chunk, u32::MAX);

        let error = deserialize(&chunk).unwrap_err();

        assert!(error.detail.contains("TooLarge"));
    }

    #[test]
    fn nine_bit_register_operand_is_rejected_instead_of_narrowed() {
        let chunk = chunk_with_code(&[
            instruction_abc(0, 0, 256, 0),
            instruction_abc(30, 0, 1, 0),
        ]);

        assert!(deserialize(&chunk).is_err());
    }

    #[test]
    fn truncated_debug_sections_are_not_silently_accepted() {
        let mut chunk = chunk_with_code(&[instruction_abc(30, 0, 1, 0)]);
        chunk.truncate(chunk.len() - 4);

        assert!(deserialize(&chunk).is_err());
    }

    #[test]
    fn zero_length_string_constant_returns_error() {
        let mut chunk = header();
        push_u32(&mut chunk, 0);
        push_u32(&mut chunk, 0);
        push_u32(&mut chunk, 0);
        chunk.extend_from_slice(&[0, 0, 0, 2]);
        push_u32(&mut chunk, 1);
        push_u32(&mut chunk, instruction_abc(30, 0, 1, 0));
        push_u32(&mut chunk, 1);
        chunk.push(4);
        push_u32(&mut chunk, 0);
        push_u32(&mut chunk, 0);
        push_u32(&mut chunk, 0);
        push_u32(&mut chunk, 0);
        push_u32(&mut chunk, 0);

        assert!(deserialize(&chunk).is_err());
    }

    #[test]
    fn extended_setlist_word_is_consumed_without_decoding_it_as_opcode() {
        let code = [
            instruction_abc(10, 0, 0, 0),
            instruction_abc(34, 0, 1, 0),
            2,
            instruction_abc(30, 0, 2, 0),
        ];
        let chunk = chunk_with_code(&code);

        let parsed = deserialize(&chunk).unwrap();

        assert!(matches!(
            parsed.function.code[1],
            Instruction::SetList {
                block_number: 2,
                ..
            }
        ));
        assert!(matches!(parsed.function.code[2], Instruction::ExtraWord(2)));
    }

    #[test]
    fn extended_setlist_rejects_nonpositive_signed_block() {
        let code = [
            instruction_abc(10, 0, 0, 0),
            instruction_abc(34, 0, 1, 0),
            u32::MAX,
            instruction_abc(30, 0, 2, 0),
        ];

        assert!(deserialize(&chunk_with_code(&code)).is_err());
    }
}
