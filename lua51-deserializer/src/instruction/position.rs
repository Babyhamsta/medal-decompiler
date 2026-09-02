use nom::{
    Err, IResult,
    error::{Error, ErrorKind},
    number::complete::le_u32,
};

#[derive(Debug)]
pub struct Position {
    pub instruction: usize,
    pub source: u32,
}

impl Position {
    pub fn parse(input: &[u8]) -> IResult<&[u8], Vec<Self>> {
        let (input, positions_length) = le_u32(input)?;
        let positions_length = positions_length as usize;
        if positions_length > input.len() / 4 {
            return Err(Err::Failure(Error::new(input, ErrorKind::TooLarge)));
        }

        let mut positions = Vec::new();
        positions
            .try_reserve_exact(positions_length)
            .map_err(|_| Err::Failure(Error::new(input, ErrorKind::TooLarge)))?;

        let mut remaining = input;
        for instruction in 0..positions_length {
            let (input, source) = le_u32(remaining)?;
            remaining = input;
            positions.push(Self {
                instruction,
                source,
            });
        }

        Ok((remaining, positions))
    }
}

#[cfg(test)]
mod tests {
    use super::Position;

    #[test]
    fn parses_positions_directly_into_the_destination_vector() {
        let bytes = [
            2, 0, 0, 0, // count
            10, 0, 0, 0, // instruction 0 source line
            20, 0, 0, 0, // instruction 1 source line
        ];

        let (remaining, positions) = Position::parse(&bytes).expect("valid line table");

        assert!(remaining.is_empty());
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0].instruction, 0);
        assert_eq!(positions[0].source, 10);
        assert_eq!(positions[1].instruction, 1);
        assert_eq!(positions[1].source, 20);
    }

    #[test]
    fn rejects_line_count_larger_than_remaining_input() {
        let bytes = [2, 0, 0, 0, 10, 0, 0, 0];

        assert!(Position::parse(&bytes).is_err());
    }
}
