use nom::{
    Err, IResult,
    bytes::complete::take,
    error::{Error, ErrorKind},
    number::complete::le_u8,
};

use super::{chunk::Chunk, version::BytecodeVersion};

#[derive(Debug)]
pub enum Bytecode {
    Error(String),
    Chunk(Chunk),
}

impl Bytecode {
    pub fn parse(input: &[u8], encode_key: u8) -> IResult<&[u8], Bytecode> {
        let (input, status_code) = le_u8(input)?;
        match status_code {
            0 => {
                let (input, error_msg) = take(input.len())(input)?;
                Ok((
                    input,
                    Bytecode::Error(String::from_utf8_lossy(error_msg).to_string()),
                ))
            }
            4..=12 => {
                let version = BytecodeVersion::new(status_code)
                    .map_err(|_| Err::Failure(Error::new(input, ErrorKind::Verify)))?;
                let (input, chunk) = Chunk::parse(input, encode_key, version)?;
                Ok((input, Bytecode::Chunk(chunk)))
            }
            _ => Err(Err::Failure(Error::new(input, ErrorKind::Verify))),
        }
    }
}
