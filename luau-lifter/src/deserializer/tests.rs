use std::convert::TryFrom;

use super::{bytecode::Bytecode, constant::Constant, deserialize, version::BytecodeVersion};
use crate::{
    instruction::{Instruction, InstructionEncoding},
    op_code::OpCode,
};

fn push_uleb128(bytes: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        bytes.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn minimal_prototype(version: u8, debug_info: bool) -> Vec<u8> {
    let mut proto = vec![
        1, // max stack
        0, // parameters
        0, // upvalues
        0, // vararg
        0, // flags
        0, // type-info byte count
        1, // instruction count
    ];
    proto.extend_from_slice(&22u32.to_le_bytes()); // RETURN R0, 0
    proto.extend_from_slice(&[
        0, // constant count
        0, // child-prototype count
        0, // line defined
        0, // function name
        0, // no line info
    ]);

    if debug_info {
        proto.push(1); // has debug info
        proto.push(1); // one local
        proto.extend_from_slice(&[
            0, // name string reference
            0, // start pc
            1, // end pc
            0, // register
            1, // one upvalue
            0, // upvalue-name string reference
        ]);
    } else {
        proto.push(0);
    }

    if version >= 11 {
        proto.push(0); // feedback count
    }

    proto
}

fn minimal_chunk(version: u8, debug_info: bool, proto_extension: &[u8]) -> Vec<u8> {
    let mut bytes = vec![
        version, 1, // type version
        0, // string count
        1, // prototype count
    ];
    let mut proto = minimal_prototype(version, debug_info);
    proto.extend_from_slice(proto_extension);

    if version >= 12 {
        push_uleb128(&mut bytes, proto.len() as u64);
    }
    bytes.extend_from_slice(&proto);
    bytes.push(0); // main prototype
    bytes
}

#[test]
fn accepts_every_bytecode_version_from_4_through_12() {
    for version in 4..=12 {
        let parsed = deserialize(&minimal_chunk(version, false, &[]), 1);
        assert!(parsed.is_ok(), "version {version}: {parsed:?}");
    }
}

#[test]
fn rejects_versions_outside_4_through_12_without_panicking() {
    for version in [3, 13, 255] {
        let outcome =
            std::panic::catch_unwind(|| deserialize(&minimal_chunk(version, false, &[]), 1));
        assert!(outcome.is_ok(), "version {version} panicked");
        let error = outcome
            .unwrap()
            .expect_err("unsupported version was accepted");
        assert!(error.contains("unsupported bytecode version"));
    }
}

#[test]
fn rejects_type_versions_outside_one_through_three() {
    for type_version in [0, 4, 255] {
        let mut bytes = minimal_chunk(12, false, &[]);
        bytes[1] = type_version;
        assert!(
            deserialize(&bytes, 1).is_err(),
            "type version {type_version} was accepted"
        );
    }
}

#[test]
fn rejects_trailing_chunk_bytes() {
    let mut bytes = minimal_chunk(12, false, &[]);
    bytes.extend_from_slice(&[0xaa, 0xbb]);
    assert!(deserialize(&bytes, 1).is_err());
}

#[test]
fn rejects_v12_prototype_size_past_input() {
    let mut bytes = minimal_chunk(12, false, &[]);
    bytes[4] = bytes[4].saturating_add(10);
    assert!(deserialize(&bytes, 1).is_err());
}

#[test]
fn accepts_v12_unknown_bytes_inside_declared_prototype() {
    let bytes = minimal_chunk(12, false, &[0xaa, 0xbb]);
    let parsed = deserialize(&bytes, 1).expect("v12 extension bytes should stay proto-local");
    assert!(matches!(parsed, Bytecode::Chunk(_)));
}

#[test]
fn parses_debug_locals_and_upvalues_without_panicking() {
    let parsed = deserialize(&minimal_chunk(12, true, &[]), 1).expect("debug records");
    let Bytecode::Chunk(chunk) = parsed else {
        panic!("expected chunk");
    };
    assert_eq!(chunk.functions[0].debug_locals.len(), 1);
    assert_eq!(chunk.functions[0].debug_upvalues, vec![0]);
}

fn version(value: u8) -> BytecodeVersion {
    BytecodeVersion::new(value).unwrap()
}

#[test]
fn parses_table_with_constants_payload() {
    let mut bytes = vec![8, 2, 3];
    bytes.extend_from_slice(&4i32.to_le_bytes());
    bytes.push(5);
    bytes.extend_from_slice(&(-1i32).to_le_bytes());

    let (remaining, constant) = Constant::parse(&bytes, version(7)).unwrap();
    assert!(remaining.is_empty());
    assert_eq!(
        constant,
        Constant::Table {
            entries: vec![(3, Some(4)), (5, None)]
        }
    );
}

#[test]
fn parses_exact_signed_integer_payloads() {
    for (expected, sign, magnitude) in [
        (9_007_199_254_740_993i64, 0u8, 9_007_199_254_740_993u64),
        (i64::MIN, 1u8, 1u64 << 63),
    ] {
        let mut bytes = vec![9, sign];
        push_uleb128(&mut bytes, magnitude);
        let (remaining, constant) = Constant::parse(&bytes, version(8)).unwrap();
        assert!(remaining.is_empty());
        assert_eq!(constant, Constant::Integer(expected));
    }
}

#[test]
fn parses_class_shape_and_double_vector_payloads() {
    let class_shape = [10, 1, 2, 1, 2, 3, 4];
    let (remaining, constant) = Constant::parse(&class_shape, version(10)).unwrap();
    assert!(remaining.is_empty());
    assert_eq!(
        constant,
        Constant::ClassShape {
            class_name: 1,
            properties: vec![2, 3],
            methods: vec![4],
        }
    );

    let mut vector = vec![11];
    for value in [1.25f64, 2.5, 3.75, 1.000_000_000_000_000_2] {
        vector.extend_from_slice(&value.to_le_bytes());
    }
    let (remaining, constant) = Constant::parse(&vector, version(4)).unwrap();
    assert!(remaining.is_empty());
    assert_eq!(
        constant,
        Constant::VectorD(1.25, 2.5, 3.75, 1.000_000_000_000_000_2)
    );
}

#[test]
fn rejects_constants_before_their_minimum_version() {
    for (tag, minimum_version) in [(8, 7), (9, 8), (10, 10)] {
        let bytes = [tag];
        assert!(
            Constant::parse(&bytes, version(minimum_version - 1)).is_err(),
            "tag {tag} accepted before v{minimum_version}"
        );
    }
}

#[test]
fn later_opcode_metadata_matches_wire_format() {
    let expected = [
        (OpCode::LOP_GETUDATAKS, 83, InstructionEncoding::Abc, 9),
        (OpCode::LOP_SETUDATAKS, 84, InstructionEncoding::Abc, 9),
        (OpCode::LOP_NAMECALLUDATA, 85, InstructionEncoding::Abc, 9),
        (OpCode::LOP_NEWCLASSMEMBER, 86, InstructionEncoding::Abc, 10),
        (OpCode::LOP_CALLFB, 87, InstructionEncoding::Abc, 11),
        (OpCode::LOP_CMPPROTO, 88, InstructionEncoding::Ad, 11),
    ];

    for (opcode, ordinal, encoding, minimum_version) in expected {
        assert_eq!(opcode as u8, ordinal);
        assert_eq!(opcode.encoding(), encoding);
        assert!(opcode.has_aux());
        assert_eq!(opcode.minimum_version(), minimum_version);
    }
}

#[test]
fn every_wire_opcode_has_layout_and_word_length() {
    for ordinal in 0..OpCode::LOP__COUNT as u8 {
        let opcode = OpCode::try_from(ordinal).unwrap();
        let _ = opcode.encoding();
        assert!(matches!(opcode.word_len(), 1 | 2));
    }
}

#[test]
fn rejects_opcodes_before_their_minimum_version() {
    for (opcode, minimum_version) in [
        (OpCode::LOP_GETUDATAKS, 9),
        (OpCode::LOP_NEWCLASSMEMBER, 10),
        (OpCode::LOP_CALLFB, 11),
        (OpCode::LOP_CMPPROTO, 11),
    ] {
        let word = opcode as u32;
        assert!(
            Instruction::parse(word, 1, version(minimum_version - 1)).is_err(),
            "{opcode:?} accepted before v{minimum_version}"
        );
    }
}

#[test]
fn rejects_opcode_count_sentinel_without_panicking() {
    let result =
        std::panic::catch_unwind(|| Instruction::parse(OpCode::LOP__COUNT as u32, 1, version(12)));
    assert!(result.is_ok());
    assert!(result.unwrap().is_err());
}
