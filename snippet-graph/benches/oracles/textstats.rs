#[cfg(test)]
mod oracle {
    use super::*;

    fn owned(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn tokenize_splits_and_lowercases() {
        assert_eq!(tokenize("Hello, World! 42x"), owned(&["hello", "world", "42x"]));
    }

    #[test]
    fn tokenize_has_no_empty_tokens() {
        assert_eq!(tokenize("  ,, ;; "), Vec::<String>::new());
        assert_eq!(tokenize(""), Vec::<String>::new());
    }

    #[test]
    fn tokenize_treats_non_ascii_as_separator() {
        assert_eq!(tokenize("caf\u{e9}s"), owned(&["caf", "s"]));
    }

    #[test]
    fn word_counts_sorted_by_count_then_word() {
        let t = owned(&["b", "a", "b", "c", "a", "b", "d"]);
        assert_eq!(
            word_counts(&t),
            vec![
                ("b".to_string(), 3),
                ("a".to_string(), 2),
                ("c".to_string(), 1),
                ("d".to_string(), 1)
            ]
        );
    }

    #[test]
    fn word_counts_empty() {
        assert!(word_counts(&[]).is_empty());
    }

    #[test]
    fn top_words_truncates() {
        let got = top_words("a a a b b c", 2);
        assert_eq!(got, vec![("a".to_string(), 3), ("b".to_string(), 2)]);
    }

    #[test]
    fn top_words_returns_all_when_n_large() {
        assert_eq!(top_words("x y", 10).len(), 2);
        assert!(top_words("x y", 0).is_empty());
    }

    #[test]
    fn unique_words_counts_distinct() {
        assert_eq!(unique_words("The the THE cat Cat dog"), 3);
        assert_eq!(unique_words(""), 0);
    }

    #[test]
    fn word_counts_scales() {
        let tokens: Vec<String> = (0..200_000).map(|i| format!("w{}", i % 5_000)).collect();
        let start = std::time::Instant::now();
        let counts = word_counts(&tokens);
        assert_eq!(counts.len(), 5_000);
        assert!(counts.iter().all(|(_, c)| *c == 40));
        assert!(start.elapsed().as_secs() < 5);
    }
}
