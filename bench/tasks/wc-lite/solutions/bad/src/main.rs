use std::io::Read;

fn main() {
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text).unwrap_or(0);
    let lines = text.lines().count();
    let words = text.split(' ').count();
    println!("{} {} {}", lines, words, text.len());
}
