use enum_as_inner::EnumAsInner;
use nom::{
    Err, IResult,
    bytes::complete::take,
    error::{Error, ErrorKind, ParseError},
    number::complete::{le_f64, le_u8, le_u32, le_u64},
};

use crate::bounded_count;

#[derive(Debug, EnumAsInner)]
pub enum Value<'a> {
    Nil,
    Boolean(bool),
    Number(f64),
    String(&'a [u8]),
}

impl<'a> Value<'a> {
    pub fn parse(input: &'a [u8]) -> IResult<&'a [u8], Self> {
        Self::parse_with_size_t(input, 4)
    }

    pub(crate) fn parse_with_size_t(
        input: &'a [u8],
        size_t_width: u8,
    ) -> IResult<&'a [u8], Self> {
        let (input, kind) = le_u8(input)?;

        match kind {
            0 => Ok((input, Self::Nil)),
            1 => {
                let (input, value) = le_u8(input)?;
                match value {
                    0 => Ok((input, Self::Boolean(false))),
                    1 => Ok((input, Self::Boolean(true))),
                    _ => Err(Err::Failure(Error::from_error_kind(
                        input,
                        ErrorKind::Verify,
                    ))),
                }
            }
            3 => {
                let (input, value) = le_f64(input)?;

                Ok((input, Self::Number(value)))
            }
            4 => {
                let (input, value) = parse_string_with_size_t(input, size_t_width)?;

                if value.is_empty() || value.last() != Some(&0) {
                    return Err(Err::Failure(Error::from_error_kind(
                        input,
                        ErrorKind::Verify,
                    )));
                }

                // exclude null terminator
                Ok((input, Self::String(&value[..value.len() - 1])))
            }
            _ => Err(Err::Failure(Error::from_error_kind(
                input,
                ErrorKind::Switch,
            ))),
        }
    }
}

pub fn parse_string(input: &[u8]) -> IResult<&[u8], &[u8]> {
    parse_string_with_size_t(input, 4)
}

pub(crate) fn parse_string_with_size_t(
    input: &[u8],
    size_t_width: u8,
) -> IResult<&[u8], &[u8]> {
    let (input, string_length) = match size_t_width {
        4 => {
            let (input, length) = le_u32(input)?;
            let length = usize::try_from(length)
                .map_err(|_| Err::Failure(Error::new(input, ErrorKind::TooLarge)))?;
            (input, length)
        }
        8 => {
            let (input, length) = le_u64(input)?;
            let length = usize::try_from(length)
                .map_err(|_| Err::Failure(Error::new(input, ErrorKind::TooLarge)))?;
            (input, length)
        }
        _ => return Err(Err::Failure(Error::new(input, ErrorKind::Verify))),
    };
    take(string_length)(input)
}

pub fn parse_strings(input: &[u8]) -> IResult<&[u8], Vec<&[u8]>> {
    parse_strings_with_size_t(input, 4)
}

pub(crate) fn parse_strings_with_size_t(
    input: &[u8],
    size_t_width: u8,
) -> IResult<&[u8], Vec<&[u8]>> {
    let (input, string_count) = le_u32(input)?;
    let (input, strings) = bounded_count(
        input,
        string_count as usize,
        usize::from(size_t_width),
        |input| {
            let (input, value) = parse_string_with_size_t(input, size_t_width)?;
            if value.is_empty() || value.last() != Some(&0) {
                return Err(Err::Failure(Error::new(input, ErrorKind::Verify)));
            }
            Ok((input, &value[..value.len() - 1]))
        },
    )?;

    Ok((input, strings))
}

#[cfg(test)]
mod tests {
    use super::Value;

    #[test]
    fn boolean_constants_require_canonical_payloads() {
        assert!(matches!(Value::parse(&[1, 0]), Ok(([], Value::Boolean(false)))));
        assert!(matches!(Value::parse(&[1, 1]), Ok(([], Value::Boolean(true)))));
        assert!(Value::parse(&[1, 2]).is_err());
    }
}
