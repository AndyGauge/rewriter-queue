use std::io::Read;

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn count(data: &[u8]) -> (usize, usize, usize) {
    let lines = data.iter().filter(|&&b| b == b'\n').count();
    let mut words = 0;
    let mut in_word = false;
    for &b in data {
        if is_space(b) {
            in_word = false;
        } else if !in_word {
            in_word = true;
            words += 1;
        }
    }
    (lines, words, data.len())
}

fn main() {
    let mut data = Vec::new();
    if std::io::stdin().read_to_end(&mut data).is_err() {
        std::process::exit(1);
    }
    let (lines, words, bytes) = count(&data);
    println!("{lines} {words} {bytes}");
}
