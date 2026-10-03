mod error;
mod eval;
mod token;

use std::process::exit;

fn main() {
    let expression = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    match eval::evaluate(&expression) {
        Ok(value) => println!("{value}"),
        Err(e) => {
            eprintln!("error: {e}");
            exit(2);
        }
    }
}
