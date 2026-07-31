use crate::{LocalRefs, LocalRefsMut, RValueRefs, RValueRefsMut};
use smallvec::smallvec;
use std::fmt;

use crate::{Literal, LocalRw, RValue, RcLocal, Reduce, SideEffects, Traverse};

use super::{Unary, UnaryOperation};

#[derive(Debug, PartialEq, Eq, PartialOrd, Copy, Clone)]
pub enum BinaryOperation {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Concat,
    Equal,
    NotEqual,
    LessThanOrEqual,
    GreaterThanOrEqual,
    LessThan,
    GreaterThan,
    And,
    Or,
    IDiv,
}

impl BinaryOperation {
    pub fn is_comparator(&self) -> bool {
        matches!(
            self,
            BinaryOperation::Equal
                | BinaryOperation::NotEqual
                | BinaryOperation::LessThanOrEqual
                | BinaryOperation::GreaterThanOrEqual
                | BinaryOperation::LessThan
                | BinaryOperation::GreaterThan
        )
    }
}

impl fmt::Display for BinaryOperation {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                BinaryOperation::Add => "+",
                BinaryOperation::Sub => "-",
                BinaryOperation::Mul => "*",
                BinaryOperation::Div => "/",
                BinaryOperation::Mod => "%",
                BinaryOperation::Pow => "^",
                BinaryOperation::Concat => "..",
                BinaryOperation::Equal => "==",
                BinaryOperation::NotEqual => "~=",
                BinaryOperation::LessThanOrEqual => "<=",
                BinaryOperation::GreaterThanOrEqual => ">=",
                BinaryOperation::LessThan => "<",
                BinaryOperation::GreaterThan => ">",
                BinaryOperation::And => "and",
                BinaryOperation::Or => "or",
                BinaryOperation::IDiv => "//",
            }
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Binary {
    pub left: Box<RValue>,
    pub right: Box<RValue>,
    pub operation: BinaryOperation,
}

impl Traverse for Binary {
    fn rvalues_mut(&mut self) -> RValueRefsMut<'_> {
        smallvec![&mut *self.left, &mut *self.right]
    }

    fn rvalues(&self) -> RValueRefs<'_> {
        smallvec![&*self.left, &*self.right]
    }
}

impl SideEffects for Binary {
    fn has_side_effects(&self) -> bool {
        // TODO: do this properly
        match self.operation {
            BinaryOperation::And | BinaryOperation::Or => {
                self.left.has_side_effects() || self.right.has_side_effects()
            }
            _ => true,
        }
    }
}

impl<'a: 'b, 'b> Reduce for Binary {
    fn reduce(self) -> RValue {
        // TODO: true == true, true == false, etc.
        // really anything without side effects should be true if l == r
        match (self.left.reduce(), self.right.reduce(), self.operation) {
            (
                RValue::Unary(Unary {
                    operation: UnaryOperation::Not,
                    value: left,
                }),
                RValue::Unary(Unary {
                    operation: UnaryOperation::Not,
                    value: right,
                }),
                BinaryOperation::And | BinaryOperation::Or,
            ) => Unary {
                value: Box::new(
                    Binary {
                        left,
                        right,
                        operation: if self.operation == BinaryOperation::And {
                            BinaryOperation::Or
                        } else {
                            BinaryOperation::And
                        },
                    }
                    .into(),
                ),
                operation: UnaryOperation::Not,
            }
            .into(),
            (
                RValue::Literal(Literal::Boolean(left)),
                RValue::Literal(Literal::Boolean(right)),
                BinaryOperation::And | BinaryOperation::Or,
            ) => Literal::Boolean(if self.operation == BinaryOperation::And {
                left && right
            } else {
                left || right
            })
            .into(),
            (
                RValue::Literal(Literal::Boolean(left)),
                right,
                BinaryOperation::And | BinaryOperation::Or,
            ) => match self.operation {
                BinaryOperation::And if !left => RValue::Literal(Literal::Boolean(false)),
                BinaryOperation::And => right.reduce(),
                BinaryOperation::Or if left => RValue::Literal(Literal::Boolean(true)),
                BinaryOperation::Or => right.reduce(),
                _ => unreachable!(),
            },
            (left, right, BinaryOperation::And)
                if !left.has_side_effects() && !right.has_side_effects() && left == right =>
            {
                left
            }
            (
                RValue::Binary(Binary {
                    left:
                        box value @ RValue::Unary(Unary {
                            operation: UnaryOperation::Not,
                            ..
                        }),
                    right: box RValue::Literal(Literal::Boolean(true)),
                    operation: BinaryOperation::And,
                }),
                RValue::Literal(Literal::Boolean(false)),
                BinaryOperation::Or,
            ) => value,
            (left, right, BinaryOperation::Or) if left == right => left,
            // TODO: concat numbers
            (
                RValue::Literal(Literal::String(left)),
                RValue::Literal(Literal::String(right)),
                BinaryOperation::Concat,
            ) => RValue::Literal(Literal::String(
                left.into_iter().chain(right.into_iter()).collect(),
            )),
            (left, right, operation) => Self {
                left: Box::new(left),
                right: Box::new(right),
                operation,
            }
            .into(),
        }
    }

    fn reduce_condition(self) -> RValue {
        let (left, right) = if matches!(self.operation, BinaryOperation::And | BinaryOperation::Or)
        {
            (self.left.reduce_condition(), self.right.reduce_condition())
        } else {
            (self.left.reduce(), self.right.reduce())
        };
        match (left, right, self.operation) {
            (
                RValue::Unary(Unary {
                    operation: UnaryOperation::Not,
                    value: left,
                }),
                RValue::Unary(Unary {
                    operation: UnaryOperation::Not,
                    value: right,
                }),
                BinaryOperation::And | BinaryOperation::Or,
            ) => Unary {
                value: Box::new(
                    Binary {
                        left,
                        right,
                        operation: if self.operation == BinaryOperation::And {
                            BinaryOperation::Or
                        } else {
                            BinaryOperation::And
                        },
                    }
                    .into(),
                ),
                operation: UnaryOperation::Not,
            }
            .into(),
            (
                RValue::Literal(Literal::Boolean(left)),
                RValue::Literal(Literal::Boolean(right)),
                BinaryOperation::And | BinaryOperation::Or,
            ) => Literal::Boolean(if self.operation == BinaryOperation::And {
                left && right
            } else {
                left || right
            })
            .into(),
            (
                RValue::Literal(Literal::Boolean(left)),
                right,
                BinaryOperation::And | BinaryOperation::Or,
            ) => match self.operation {
                BinaryOperation::And if !left => RValue::Literal(Literal::Boolean(false)),
                BinaryOperation::And => right.reduce(),
                BinaryOperation::Or if left => RValue::Literal(Literal::Boolean(true)),
                BinaryOperation::Or => right.reduce(),
                _ => unreachable!(),
            },
            (
                left,
                RValue::Literal(Literal::Boolean(right)),
                BinaryOperation::And | BinaryOperation::Or,
            ) => match self.operation {
                BinaryOperation::And if !right => RValue::Literal(Literal::Boolean(false)),
                BinaryOperation::And => left.reduce(),
                BinaryOperation::Or if right => RValue::Literal(Literal::Boolean(true)),
                BinaryOperation::Or => left.reduce(),
                _ => unreachable!(),
            },
            // TODO: concat numbers
            (
                RValue::Literal(Literal::String(left)),
                RValue::Literal(Literal::String(right)),
                BinaryOperation::Concat,
            ) => RValue::Literal(Literal::String(
                left.into_iter().chain(right.into_iter()).collect(),
            )),
            (left, right, operation) => Self {
                left: Box::new(left),
                right: Box::new(right),
                operation,
            }
            .into(),
        }
    }
}

impl Binary {
    pub fn new(left: RValue, right: RValue, operation: BinaryOperation) -> Self {
        Self {
            left: Box::new(left),
            right: Box::new(right),
            operation,
        }
    }

    pub fn precedence(&self) -> usize {
        match self.operation {
            BinaryOperation::Pow => 8,
            BinaryOperation::Mul
            | BinaryOperation::Div
            | BinaryOperation::Mod
            | BinaryOperation::IDiv => 6,
            BinaryOperation::Add | BinaryOperation::Sub => 5,
            BinaryOperation::Concat => 4,
            BinaryOperation::LessThan
            | BinaryOperation::GreaterThan
            | BinaryOperation::LessThanOrEqual
            | BinaryOperation::GreaterThanOrEqual
            | BinaryOperation::Equal
            | BinaryOperation::NotEqual => 3,
            BinaryOperation::And => 2,
            BinaryOperation::Or => 1,
        }
    }

    pub fn right_associative(&self) -> bool {
        matches!(
            self.operation,
            BinaryOperation::Pow | BinaryOperation::Concat
        )
    }

    /// Whether re-parsing this node's right child as part of a left-leaning
    /// chain would still produce the same program.
    ///
    /// Only `and` and `or` qualify. Both are fully associative: `a or (b or
    /// c)` and `(a or b) or c` pick the same operand, evaluate the same
    /// operands in the same order, and short-circuit at the same point, so
    /// dropping the parentheses is a purely textual change.
    ///
    /// No arithmetic operator qualifies, associative in exact arithmetic or
    /// not. Lua numbers are IEEE-754 doubles, where addition and
    /// multiplication are not associative: with `a, b, c = 1e16, -1e16, 1`,
    /// `a + (b + c)` is `0` while `a + b + c` is `1`. Reassociating those
    /// would silently change what the output computes.
    ///
    /// `..` is right-associative and so never reaches the equal-precedence
    /// case, and the comparisons cannot chain at all in Lua.
    fn right_child_reassociates(&self) -> bool {
        // And and Or hold distinct precedences, so an equal-precedence right
        // child of one is always the same operator.
        matches!(self.operation, BinaryOperation::And | BinaryOperation::Or)
    }

    pub fn left_group(&self) -> bool {
        self.precedence() > self.left.precedence()
            || (self.precedence() == self.left.precedence() && self.right_associative())
    }

    pub fn right_group(&self) -> bool {
        if self.precedence() > self.right.precedence() {
            return true;
        }
        self.precedence() == self.right.precedence()
            && !self.right_associative()
            && !self.right_child_reassociates()
    }
}

impl LocalRw for Binary {
    fn values_read(&self) -> LocalRefs<'_> {
        self.left
            .values_read()
            .into_iter()
            .chain(self.right.values_read().into_iter())
            .collect()
    }

    fn values_read_mut(&mut self) -> LocalRefsMut<'_> {
        self.left
            .values_read_mut()
            .into_iter()
            .chain(self.right.values_read_mut().into_iter())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{Binary, BinaryOperation};
    use crate::{Global, RValue};

    fn value(name: &str) -> RValue {
        Global::from(name).into()
    }

    /// `a <op> (b <op> c)`, the shape short-circuit recovery builds for a
    /// chain of the same operator.
    fn right_leaning(operation: BinaryOperation) -> String {
        Binary::new(
            value("a"),
            Binary::new(value("b"), value("c"), operation).into(),
            operation,
        )
        .to_string()
    }

    #[test]
    fn a_right_leaning_and_chain_prints_flat() {
        assert_eq!(right_leaning(BinaryOperation::And), "a and b and c");
    }

    #[test]
    fn a_right_leaning_or_chain_prints_flat() {
        assert_eq!(right_leaning(BinaryOperation::Or), "a or b or c");
    }

    #[test]
    fn a_four_term_and_chain_prints_flat() {
        let inner = Binary::new(
            value("b"),
            Binary::new(value("c"), value("d"), BinaryOperation::And).into(),
            BinaryOperation::And,
        );
        let chain = Binary::new(value("a"), inner.into(), BinaryOperation::And);

        assert_eq!(chain.to_string(), "a and b and c and d");
    }

    /// Reassociating arithmetic is not a formatting change.
    ///
    /// Lua numbers are IEEE-754 doubles, so `+` and `*` are not associative:
    /// with `a, b, c = 1e16, -1e16, 1`, Luau prints `0` for `a + (b + c)` and
    /// `1` for `a + b + c`. Dropping these parentheses would silently change
    /// what the recompiled program computes, so every arithmetic operator
    /// keeps them.
    #[test]
    fn a_right_leaning_arithmetic_chain_keeps_its_parentheses() {
        for operation in [
            BinaryOperation::Add,
            BinaryOperation::Sub,
            BinaryOperation::Mul,
            BinaryOperation::Div,
            BinaryOperation::Mod,
            BinaryOperation::IDiv,
        ] {
            assert_eq!(
                right_leaning(operation),
                format!("a {operation} (b {operation} c)"),
                "{operation} reassociated"
            );
        }
    }

    #[test]
    fn a_right_leaning_comparison_keeps_its_parentheses() {
        for operation in [
            BinaryOperation::Equal,
            BinaryOperation::NotEqual,
            BinaryOperation::LessThan,
            BinaryOperation::GreaterThan,
            BinaryOperation::LessThanOrEqual,
            BinaryOperation::GreaterThanOrEqual,
        ] {
            assert_eq!(
                right_leaning(operation),
                format!("a {operation} (b {operation} c)"),
                "{operation} reassociated"
            );
        }
    }

    /// `..` and `^` are right-associative, so a right-leaning chain is what
    /// the flat text already parses as and never needed parentheses.
    #[test]
    fn a_right_associative_chain_prints_flat_and_a_left_leaning_one_does_not() {
        for operation in [BinaryOperation::Concat, BinaryOperation::Pow] {
            assert_eq!(
                right_leaning(operation),
                format!("a {operation} b {operation} c")
            );
            let left_leaning = Binary::new(
                Binary::new(value("a"), value("b"), operation).into(),
                value("c"),
                operation,
            );
            assert_eq!(
                left_leaning.to_string(),
                format!("(a {operation} b) {operation} c")
            );
        }
    }

    #[test]
    fn a_lower_precedence_right_child_still_gets_parentheses() {
        let chain = Binary::new(
            value("a"),
            Binary::new(value("b"), value("c"), BinaryOperation::Or).into(),
            BinaryOperation::And,
        );

        assert_eq!(chain.to_string(), "a and (b or c)");
    }
}

impl fmt::Display for Binary {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let parentheses = |group: bool, rvalue: &RValue| {
            if group {
                format!("({})", rvalue)
            } else {
                format!("{}", rvalue)
            }
        };

        write!(
            f,
            "{} {} {}",
            parentheses(self.left_group(), self.left.as_ref()),
            self.operation,
            parentheses(self.right_group(), self.right.as_ref()),
        )
    }
}
