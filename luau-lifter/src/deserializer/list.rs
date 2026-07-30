use nom::{
    Err, IResult,
    error::{Error, ErrorKind},
};
use nom_leb128::leb128_usize;

pub(crate) fn parse_list<'a, T>(
    input: &'a [u8],
    parser: impl FnMut(&'a [u8]) -> IResult<&'a [u8], T>,
) -> IResult<&'a [u8], Vec<T>> {
    let (input, length) = leb128_usize(input)?;
    parse_list_len(input, parser, length)
}

pub(crate) fn parse_list_len<'a, T>(
    mut input: &'a [u8],
    mut parser: impl FnMut(&'a [u8]) -> IResult<&'a [u8], T>,
    length: usize,
) -> IResult<&'a [u8], Vec<T>> {
    if length > input.len() {
        return Err(Err::Failure(Error::new(input, ErrorKind::TooLarge)));
    }
    let mut items = Vec::new();
    items
        .try_reserve_exact(length)
        .map_err(|_| Err::Failure(Error::new(input, ErrorKind::TooLarge)))?;
    for _ in 0..length {
        let (next, item) = parser(input)?;
        if next.len() >= input.len() {
            return Err(Err::Failure(Error::new(input, ErrorKind::Verify)));
        }
        input = next;
        items.push(item);
    }
    Ok((input, items))
}
