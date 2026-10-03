use std::process::exit;

fn usage() -> ! {
    eprintln!("Usage: rect <x1> <y1> <x2> <y2>");
    exit(1)
}

fn render(x1: i64, y1: i64, x2: i64, y2: i64) -> String {
    let width = x1.abs_diff(x2) + 1;
    let height = y1.abs_diff(y2) + 1;
    let mut out = String::new();
    for row in 0..height {
        let row_edge = row == 0 || row == height - 1;
        for col in 0..width {
            let col_edge = col == 0 || col == width - 1;
            out.push(match (row_edge, col_edge) {
                (true, true) => '+',
                (true, false) => '-',
                (false, true) => '|',
                (false, false) => ' ',
            });
        }
        out.push('\n');
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 4 {
        usage();
    }
    let n: Vec<i64> = args
        .iter()
        .map(|a| a.parse().unwrap_or_else(|_| usage()))
        .collect();
    println!("{}", render(n[0], n[1], n[2], n[3]));
}
