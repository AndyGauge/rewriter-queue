use std::env;
use std::process;

fn die(m: String) -> ! {
    eprintln!("error: {}", m);
    process::exit(2);
}

fn main() {
    let argv: Vec<String> = env::args().skip(1).collect();
    let s = argv.join(" ");
    let mut st: Vec<i64> = Vec::new();
    let mut count = 0;
    for t in s.split_whitespace() {
        count += 1;
        if t == "+" || t == "-" || t == "*" || t == "/" || t == "%" {
            if st.len() < 2 {
                die(format!("stack underflow at '{}'", t));
            }
            let b = st.pop().unwrap();
            let a = st.pop().unwrap();
            let r = if t == "+" {
                a.checked_add(b)
            } else if t == "-" {
                a.checked_sub(b)
            } else if t == "*" {
                a.checked_mul(b)
            } else if t == "/" {
                if b == 0 {
                    die("division by zero".to_string());
                }
                a.checked_div(b)
            } else {
                if b == 0 {
                    die("division by zero".to_string());
                }
                a.checked_rem(b)
            };
            match r {
                Some(v) => st.push(v),
                None => die(format!("overflow at '{}'", t)),
            }
        } else if t == "dup" {
            if st.len() < 1 {
                die(format!("stack underflow at '{}'", t));
            }
            let top = st[st.len() - 1];
            st.push(top);
        } else if t == "swap" {
            if st.len() < 2 {
                die(format!("stack underflow at '{}'", t));
            }
            let b = st.pop().unwrap();
            let a = st.pop().unwrap();
            st.push(b);
            st.push(a);
        } else {
            match t.parse::<i64>() {
                Ok(v) => st.push(v),
                Err(_) => die(format!("unknown token '{}'", t)),
            }
        }
    }
    if count == 0 {
        die("empty expression".to_string());
    }
    if st.len() != 1 {
        die(format!("expected 1 value on stack, found {}", st.len()));
    }
    println!("{}", st[0]);
}
