use crate::error::CalcError;
use crate::token::{parse, BinOp, Token};

pub fn evaluate(expression: &str) -> Result<i64, CalcError> {
    let mut stack: Vec<i64> = Vec::new();
    let mut seen = false;
    for word in expression.split_whitespace() {
        seen = true;
        step(&mut stack, word, parse(word)?)?;
    }
    if !seen {
        return Err(CalcError::Empty);
    }
    if stack.len() != 1 {
        return Err(CalcError::WrongResultCount(stack.len()));
    }
    Ok(stack[0])
}

fn step(stack: &mut Vec<i64>, word: &str, token: Token) -> Result<(), CalcError> {
    let underflow = || CalcError::Underflow(word.to_string());
    match token {
        Token::Num(v) => stack.push(v),
        Token::Dup => {
            let top = *stack.last().ok_or_else(underflow)?;
            stack.push(top);
        }
        Token::Swap => {
            if stack.len() < 2 {
                return Err(underflow());
            }
            let n = stack.len();
            stack.swap(n - 1, n - 2);
        }
        Token::Bin(op) => {
            if stack.len() < 2 {
                return Err(underflow());
            }
            let b = stack.pop().ok_or_else(underflow)?;
            let a = stack.pop().ok_or_else(underflow)?;
            stack.push(apply(op, a, b, word)?);
        }
    }
    Ok(())
}

fn apply(op: BinOp, a: i64, b: i64, word: &str) -> Result<i64, CalcError> {
    if b == 0 && matches!(op, BinOp::Div | BinOp::Rem) {
        return Err(CalcError::DivisionByZero);
    }
    let result = match op {
        BinOp::Add => a.checked_add(b),
        BinOp::Sub => a.checked_sub(b),
        BinOp::Mul => a.checked_mul(b),
        BinOp::Div => a.checked_div(b),
        BinOp::Rem => a.checked_rem(b),
    };
    result.ok_or_else(|| CalcError::Overflow(word.to_string()))
}
