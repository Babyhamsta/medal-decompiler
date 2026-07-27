use super::{list::parse_list, version::BytecodeVersion};
use nom::{
    Err, IResult,
    error::{Error, ErrorKind},
    number::complete::{le_f32, le_f64, le_i32, le_u8, le_u32},
};
use nom_leb128::{leb128_u64, leb128_usize};

const CONSTANT_NIL: u8 = 0;
const CONSTANT_BOOLEAN: u8 = 1;
const CONSTANT_NUMBER: u8 = 2;
const CONSTANT_STRING: u8 = 3;
const CONSTANT_IMPORT: u8 = 4;
const CONSTANT_TABLE: u8 = 5;
const CONSTANT_CLOSURE: u8 = 6;
const CONSTANT_VECTOR: u8 = 7;
const CONSTANT_TABLE_WITH_CONSTANTS: u8 = 8;
const CONSTANT_INTEGER: u8 = 9;
const CONSTANT_CLASS_SHAPE: u8 = 10;
const CONSTANT_VECTOR_DOUBLE: u8 = 11;

#[derive(Debug, Clone, PartialEq)]
pub enum Constant {
    Nil,
    Boolean(bool),
    Number(f64),
    String(usize),
    Import(usize),
    Table {
        entries: Vec<(usize, Option<usize>)>,
    },
    Closure(usize),
    VectorF(f32, f32, f32, f32),
    Integer(i64),
    ClassShape {
        class_name: usize,
        properties: Vec<usize>,
        methods: Vec<usize>,
    },
    VectorD(f64, f64, f64, f64),
}

impl Constant {
    pub(crate) fn parse(input: &[u8], version: BytecodeVersion) -> IResult<&[u8], Self> {
        let (input, tag) = le_u8(input)?;
        let minimum_version = match tag {
            CONSTANT_VECTOR => 5,
            CONSTANT_TABLE_WITH_CONSTANTS => 7,
            CONSTANT_INTEGER => 8,
            CONSTANT_CLASS_SHAPE => 10,
            _ => 4,
        };
        if version.value() < minimum_version {
            return Err(Err::Failure(Error::new(input, ErrorKind::Verify)));
        }

        match tag {
            CONSTANT_NIL => Ok((input, Constant::Nil)),
            CONSTANT_BOOLEAN => {
                let (input, value) = le_u8(input)?;
                Ok((input, Constant::Boolean(value != 0u8)))
            }
            CONSTANT_NUMBER => {
                let (input, value) = le_f64(input)?;
                Ok((input, Constant::Number(value)))
            }
            CONSTANT_STRING => {
                let (input, string_index) = leb128_usize(input)?;
                Ok((input, Constant::String(string_index)))
            }
            CONSTANT_IMPORT => {
                let (input, import_index) = le_u32(input)?;
                Ok((input, Constant::Import(import_index as usize)))
            }
            CONSTANT_TABLE => {
                let (input, keys) = parse_list(input, leb128_usize)?;
                Ok((
                    input,
                    Constant::Table {
                        entries: keys.into_iter().map(|key| (key, None)).collect(),
                    },
                ))
            }
            CONSTANT_CLOSURE => {
                let (input, f_id) = leb128_usize(input)?;
                Ok((input, Constant::Closure(f_id)))
            }
            CONSTANT_VECTOR => {
                let (input, x) = le_f32(input)?;
                let (input, y) = le_f32(input)?;
                let (input, z) = le_f32(input)?;
                let (input, w) = le_f32(input)?;
                Ok((input, Constant::VectorF(x, y, z, w)))
            }
            CONSTANT_TABLE_WITH_CONSTANTS => {
                let (mut input, entry_count) = leb128_usize(input)?;
                let mut entries = Vec::with_capacity(entry_count);
                for _ in 0..entry_count {
                    let (next, key) = leb128_usize(input)?;
                    let (next, value) = le_i32(next)?;
                    input = next;
                    entries.push((
                        key,
                        if value < 0 {
                            None
                        } else {
                            Some(value as usize)
                        },
                    ));
                }
                Ok((input, Constant::Table { entries }))
            }
            CONSTANT_INTEGER => {
                let (input, negative) = le_u8(input)?;
                let (input, magnitude) = leb128_u64(input)?;
                let value = if negative == 0 {
                    i64::try_from(magnitude)
                        .map_err(|_| Err::Failure(Error::new(input, ErrorKind::Verify)))?
                } else if magnitude == (1u64 << 63) {
                    i64::MIN
                } else {
                    -i64::try_from(magnitude)
                        .map_err(|_| Err::Failure(Error::new(input, ErrorKind::Verify)))?
                };
                Ok((input, Constant::Integer(value)))
            }
            CONSTANT_CLASS_SHAPE => {
                let (input, class_name) = leb128_usize(input)?;
                let (input, property_count) = leb128_usize(input)?;
                let (input, method_count) = leb128_usize(input)?;
                let (input, properties) =
                    super::list::parse_list_len(input, leb128_usize, property_count)?;
                let (input, methods) =
                    super::list::parse_list_len(input, leb128_usize, method_count)?;
                Ok((
                    input,
                    Constant::ClassShape {
                        class_name,
                        properties,
                        methods,
                    },
                ))
            }
            CONSTANT_VECTOR_DOUBLE => {
                let (input, x) = le_f64(input)?;
                let (input, y) = le_f64(input)?;
                let (input, z) = le_f64(input)?;
                let (input, w) = le_f64(input)?;
                Ok((input, Constant::VectorD(x, y, z, w)))
            }
            _ => Err(Err::Failure(Error::new(input, ErrorKind::Verify))),
        }
    }
}
