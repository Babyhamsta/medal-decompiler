use std::ops::Range;

use nom::{
    Err, IResult,
    error::{Error, ErrorKind},
    number::complete::le_u32,
};

use crate::{bounded_count, value::parse_string_with_size_t};

#[derive(Debug)]
pub struct Local<'a> {
    pub name: &'a [u8],
    pub range: Range<u32>,
}

impl<'a> Local<'a> {
    pub fn parse_list(input: &'a [u8]) -> IResult<&'a [u8], Vec<Self>> {
        Self::parse_list_with_size_t(input, 4)
    }

    pub(crate) fn parse_list_with_size_t(
        input: &'a [u8],
        size_t_width: u8,
    ) -> IResult<&'a [u8], Vec<Self>> {
        let (input, length) = le_u32(input)?;

        bounded_count(
            input,
            length as usize,
            usize::from(size_t_width) + 8,
            |input| Self::parse_with_size_t(input, size_t_width),
        )
    }

    fn parse_with_size_t(input: &'a [u8], size_t_width: u8) -> IResult<&'a [u8], Self> {
        let (input, name) = parse_string_with_size_t(input, size_t_width)?;
        if name.is_empty() || name.last() != Some(&0) {
            return Err(Err::Failure(Error::new(input, ErrorKind::Verify)));
        }
        let (input, start) = le_u32(input)?;
        let (input, end) = le_u32(input)?;

        Ok((
            input,
            Self {
                name: &name[..name.len() - 1],
                range: (start..end),
            },
        ))
    }
}
