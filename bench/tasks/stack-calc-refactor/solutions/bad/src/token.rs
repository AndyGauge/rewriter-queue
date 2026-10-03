use crate::error::CalcError;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Token {
    Num(i64),
    Bin(BinOp),
    Dup,
    Swap,
}

pub fn parse(word: &str) -> Result<Token, CalcError> {
    Ok(match word {
        "+" => Token::Bin(BinOp::Add),
        "-" => Token::Bin(BinOp::Sub),
        "*" => Token::Bin(BinOp::Mul),
        "/" => Token::Bin(BinOp::Div),
        "%" => Token::Bin(BinOp::Rem),
        "dup" => Token::Dup,
        "swap" => Token::Swap,
        _ => Token::Num(
            word.parse::<i64>()
                .map_err(|_| CalcError::UnknownToken(word.to_string()))?,
        ),
    })
}
