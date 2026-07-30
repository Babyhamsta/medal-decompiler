use nom::{
    Err, IResult,
    bytes::complete::take,
    error::{Error, ErrorKind},
};
use nom_leb128::leb128_usize;

pub mod bytecode;
pub mod chunk;
pub mod constant;
pub mod function;
mod list;
pub mod version;

#[cfg(test)]
mod tests;

fn parse_string(input: &[u8]) -> IResult<&[u8], Vec<u8>> {
    let (input, length) = leb128_usize(input)?;
    let (input, bytes) = take(length)(input)?;
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(length)
        .map_err(|_| Err::Failure(Error::new(input, ErrorKind::TooLarge)))?;
    owned.extend_from_slice(bytes);
    Ok((input, owned))
}

pub fn deserialize(bytecode: &[u8], encode_key: u8) -> Result<bytecode::Bytecode, String> {
    if let Some(&version) = bytecode.first()
        && version != 0
    {
        version::BytecodeVersion::new(version)?;
    }

    match bytecode::Bytecode::parse(bytecode, encode_key) {
        Ok(([], deserialized_bytecode)) => Ok(deserialized_bytecode),
        Ok((remaining, _)) => Err(format!(
            "bytecode chunk has {} unexplained trailing byte(s)",
            remaining.len()
        )),
        Err(err) => Err(err.to_string()),
    }
}

/*#[test]
fn main() -> anyhow::Result<()> {
    let compiler = Compiler::new()
        .set_debug_level(1).set_optimization_level(2);
    let bytecode = compiler.compile("asd = test");
    println!("{:#?}", bytecode);
    let deserialized = deserialize(&bytecode);
    println!("{:#?}", deserialized);
    Ok(())
}*/
