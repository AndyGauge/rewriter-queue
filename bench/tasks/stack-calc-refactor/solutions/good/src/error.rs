use std::fmt;

#[derive(Debug, PartialEq)]
pub enum CalcError {
    Empty,
    UnknownToken(String),
    Underflow(String),
    DivisionByZero,
    Overflow(String),
    WrongResultCount(usize),
}

impl fmt::Display for CalcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalcError::Empty => write!(f, "empty expression"),
            CalcError::UnknownToken(t) => write!(f, "unknown token '{t}'"),
            CalcError::Underflow(t) => write!(f, "stack underflow at '{t}'"),
            CalcError::DivisionByZero => write!(f, "division by zero"),
            CalcError::Overflow(t) => write!(f, "overflow at '{t}'"),
            CalcError::WrongResultCount(n) => write!(f, "expected 1 value on stack, found {n}"),
        }
    }
}
