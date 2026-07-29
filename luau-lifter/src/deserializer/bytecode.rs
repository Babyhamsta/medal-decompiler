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
                let error_msg = std::str::from_utf8(error_msg)
                    .map_err(|_| Err::Failure(Error::new(input, ErrorKind::Verify)))?;
                let mut message = String::new();
                message
                    .try_reserve_exact(error_msg.len())
                    .map_err(|_| Err::Failure(Error::new(input, ErrorKind::TooLarge)))?;
                message.push_str(error_msg);
                Ok((input, Bytecode::Error(message)))
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
