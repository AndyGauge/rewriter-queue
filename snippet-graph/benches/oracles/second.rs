#[cfg(test)]
mod oracle {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn log() -> (KvLog, PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("oracle_kv_{}_{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sub").join("log.kv");
        (KvLog::new(path.clone()), path)
    }

    #[test]
    fn new_keeps_path() {
        let (l, p) = log();
        assert_eq!(l.path, p);
    }

    #[test]
    fn missing_file_behaviour() {
        let (l, p) = log();
        assert!(matches!(l.get("a"), Err(KvError::KeyNotFound { .. })));
        assert!(l.keys().unwrap().is_empty());
        assert_eq!(l.line_count().unwrap(), 0);
        assert_eq!(l.compact().unwrap(), 0);
        assert!(!p.exists());
    }

    #[test]
    fn append_creates_dirs_and_writes_exact_line() {
        let (l, p) = log();
        l.append("k", "v").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "k\tv\n");
    }

    #[test]
    fn get_returns_latest() {
        let (l, _) = log();
        l.append("k", "1").unwrap();
        l.append("k", "2").unwrap();
        assert_eq!(l.get("k").unwrap(), "2");
        assert_eq!(l.line_count().unwrap(), 2);
    }

    #[test]
    fn get_unknown_key() {
        let (l, _) = log();
        l.append("a", "1").unwrap();
        match l.get("b") {
            Err(KvError::KeyNotFound { key }) => assert_eq!(key, "b"),
            other => panic!("unexpected {:?}", other),
        }
    }

    #[test]
    fn escapes_written_and_decoded() {
        let (l, p) = log();
        l.append("k", "a\\b\nc").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "k\ta\\\\b\\nc\n");
        assert_eq!(l.get("k").unwrap(), "a\\b\nc");
        assert_eq!(l.line_count().unwrap(), 1);
    }

    #[test]
    fn value_may_contain_tab_and_be_empty() {
        let (l, _) = log();
        l.append("t", "a\tb").unwrap();
        l.append("e", "").unwrap();
        assert_eq!(l.get("t").unwrap(), "a\tb");
        assert_eq!(l.get("e").unwrap(), "");
    }

    #[test]
    fn delete_writes_bare_key_line() {
        let (l, p) = log();
        l.append("k", "v").unwrap();
        l.delete("k").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "k\tv\nk\n");
        assert!(matches!(l.get("k"), Err(KvError::KeyNotFound { .. })));
        assert!(l.keys().unwrap().is_empty());
    }

    #[test]
    fn delete_unknown_key_works() {
        let (l, p) = log();
        l.delete("ghost").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "ghost\n");
        assert_eq!(l.line_count().unwrap(), 1);
    }

    #[test]
    fn set_after_delete_revives() {
        let (l, _) = log();
        l.append("k", "1").unwrap();
        l.delete("k").unwrap();
        l.append("k", "2").unwrap();
        assert_eq!(l.get("k").unwrap(), "2");
    }

    #[test]
    fn invalid_keys_rejected() {
        let (l, p) = log();
        for bad in ["", "a\tb", "a\nb"] {
            assert!(matches!(l.append(bad, "v"), Err(KvError::InvalidKey { .. })));
            assert!(matches!(l.delete(bad), Err(KvError::InvalidKey { .. })));
        }
        assert!(!p.exists());
    }

    #[test]
    fn invalid_key_carries_key() {
        let (l, _) = log();
        match l.append("a\tb", "v") {
            Err(KvError::InvalidKey { key }) => assert_eq!(key, "a\tb"),
            other => panic!("unexpected {:?}", other),
        }
    }

    #[test]
    fn keys_sorted_and_live_only() {
        let (l, _) = log();
        l.append("b", "1").unwrap();
        l.append("c", "1").unwrap();
        l.append("a", "1").unwrap();
        l.delete("c").unwrap();
        l.append("b", "2").unwrap();
        assert_eq!(l.keys().unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn line_count_counts_all_lines() {
        let (l, _) = log();
        l.append("a", "1").unwrap();
        l.delete("a").unwrap();
        l.append("b", "1").unwrap();
        assert_eq!(l.line_count().unwrap(), 3);
    }

    #[test]
    fn compact_rewrites_sorted_and_counts_removed() {
        let (l, p) = log();
        l.append("b", "1").unwrap();
        l.append("a", "x\ny").unwrap();
        l.append("b", "2").unwrap();
        l.append("c", "3").unwrap();
        l.delete("c").unwrap();
        assert_eq!(l.line_count().unwrap(), 5);
        let removed = l.compact().unwrap();
        assert_eq!(removed, 3);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\tx\\ny\nb\t2\n");
        assert_eq!(l.line_count().unwrap(), 2);
        assert_eq!(l.get("a").unwrap(), "x\ny");
        assert_eq!(l.get("b").unwrap(), "2");
    }

    #[test]
    fn compact_idempotent() {
        let (l, p) = log();
        l.append("a", "1").unwrap();
        l.append("a", "2").unwrap();
        assert_eq!(l.compact().unwrap(), 1);
        let first = std::fs::read_to_string(&p).unwrap();
        assert_eq!(l.compact().unwrap(), 0);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), first);
    }

    #[test]
    fn compact_all_deleted_leaves_empty_file() {
        let (l, p) = log();
        l.append("a", "1").unwrap();
        l.delete("a").unwrap();
        assert_eq!(l.compact().unwrap(), 2);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "");
        assert_eq!(l.line_count().unwrap(), 0);
    }

    #[test]
    fn io_error_display() {
        let e = KvError::Io {
            path: "/p".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::Other, "boom"),
        };
        assert_eq!(e.to_string(), "IO error at /p: boom");
    }
}
