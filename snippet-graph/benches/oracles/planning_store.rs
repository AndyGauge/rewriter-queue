#[cfg(test)]
mod oracle {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn store() -> (ProjectStore, PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("oracle_ps_{}_{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        (ProjectStore::new(dir.clone()), dir)
    }

    const DEFAULT_MISSION: &str =
        "# Mission\n\n*(not set — use set_mission to define the collapse objective)*\n";

    #[test]
    fn new_keeps_root() {
        let (s, dir) = store();
        assert_eq!(s.root, dir);
    }

    #[test]
    fn mission_default_when_absent() {
        let (s, _) = store();
        assert_eq!(s.get_mission().unwrap(), DEFAULT_MISSION);
    }

    #[test]
    fn mission_roundtrip_reads_mission_md() {
        let (s, dir) = store();
        s.set_mission("Collapse everything.\n").unwrap();
        assert_eq!(s.get_mission().unwrap(), "Collapse everything.\n");
        assert_eq!(
            std::fs::read_to_string(dir.join("mission.md")).unwrap(),
            "Collapse everything.\n"
        );
    }

    #[test]
    fn mission_reads_file_written_directly() {
        let (s, dir) = store();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mission.md"), "direct").unwrap();
        assert_eq!(s.get_mission().unwrap(), "direct");
    }

    #[test]
    fn mission_ignores_mission_txt() {
        let (s, dir) = store();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mission.txt"), "wrong").unwrap();
        assert_eq!(s.get_mission().unwrap(), DEFAULT_MISSION);
    }

    #[test]
    fn mission_overwrite() {
        let (s, _) = store();
        s.set_mission("one").unwrap();
        s.set_mission("two").unwrap();
        assert_eq!(s.get_mission().unwrap(), "two");
    }

    #[test]
    fn mission_unreadable_is_io_error() {
        let (s, dir) = store();
        std::fs::create_dir_all(dir.join("mission.md")).unwrap();
        let err = s.get_mission().unwrap_err();
        assert!(matches!(err, StoreError::Io { .. }));
        assert!(err.to_string().starts_with("IO error at "));
        assert!(err.to_string().contains("mission.md"));
    }

    #[test]
    fn phase_format_and_path() {
        let (s, dir) = store();
        s.set_phase("alpha", "active", "Do the thing.").unwrap();
        let expected = "# Phase: alpha\n\n**Status:** active\n\nDo the thing.\n";
        assert_eq!(s.get_phase("alpha").unwrap(), expected);
        assert_eq!(
            std::fs::read_to_string(dir.join("phases").join("alpha.md")).unwrap(),
            expected
        );
    }

    #[test]
    fn phase_missing() {
        let (s, _) = store();
        match s.get_phase("nope") {
            Err(StoreError::PhaseNotFound { name }) => assert_eq!(name, "nope"),
            other => panic!("unexpected {:?}", other),
        }
    }

    #[test]
    fn phases_listed_sorted() {
        let (s, _) = store();
        assert!(s.list_phases().unwrap().is_empty());
        s.set_phase("b", "done", "x").unwrap();
        s.set_phase("a", "done", "x").unwrap();
        s.set_phase("c", "done", "x").unwrap();
        assert_eq!(s.list_phases().unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn phases_ignore_non_md() {
        let (s, dir) = store();
        s.set_phase("a", "done", "x").unwrap();
        std::fs::write(dir.join("phases").join("junk.txt"), "x").unwrap();
        assert_eq!(s.list_phases().unwrap(), vec!["a"]);
    }

    #[test]
    fn note_roundtrip_and_path() {
        let (s, dir) = store();
        s.write_note("design", "intro", "Hello\n").unwrap();
        assert_eq!(s.read_note("design", "intro").unwrap(), "Hello\n");
        assert_eq!(
            std::fs::read_to_string(dir.join("notes").join("design").join("intro.md")).unwrap(),
            "Hello\n"
        );
    }

    #[test]
    fn note_missing() {
        let (s, _) = store();
        match s.read_note("design", "ghost") {
            Err(StoreError::NoteNotFound { section, slug }) => {
                assert_eq!(section, "design");
                assert_eq!(slug, "ghost");
            }
            other => panic!("unexpected {:?}", other),
        }
    }

    #[test]
    fn list_notes_with_section() {
        let (s, _) = store();
        s.write_note("design", "b", "x").unwrap();
        s.write_note("design", "a", "x").unwrap();
        s.write_note("other", "z", "x").unwrap();
        assert_eq!(
            s.list_notes(Some("design")).unwrap(),
            vec!["design/a", "design/b"]
        );
        assert!(s.list_notes(Some("empty")).unwrap().is_empty());
    }

    #[test]
    fn list_notes_without_section_covers_all_sorted() {
        let (s, _) = store();
        assert!(s.list_notes(None).unwrap().is_empty());
        s.write_note("design", "b", "x").unwrap();
        s.write_note("alpha", "z", "x").unwrap();
        s.write_note("design", "a", "x").unwrap();
        let all = s.list_notes(None).unwrap();
        assert_eq!(all.len(), 3);
        let mut sorted = all.clone();
        sorted.sort();
        assert_eq!(all, sorted);
        assert!(all[0].contains("alpha") && all[0].contains('z'));
        assert!(all[1].contains("design") && all[1].contains('a'));
        assert!(all[2].contains("design") && all[2].contains('b'));
    }

    #[test]
    fn search_is_case_insensitive_and_returns_lines() {
        let (s, _) = store();
        s.write_note("s", "one", "first\nHello World\nlast\n").unwrap();
        s.write_note("s", "two", "nothing here\n").unwrap();
        let hits = s.search_notes("hello").unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].0.contains("one"));
        assert_eq!(hits[0].1, "Hello World");
    }

    #[test]
    fn search_caps_at_three_lines_joined_by_newline() {
        let (s, _) = store();
        s.write_note("s", "many", "x1\nx2\nskip\nx3\nx4\n").unwrap();
        let hits = s.search_notes("X").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, "x1\nx2\nx3");
    }

    #[test]
    fn search_no_match_is_empty() {
        let (s, _) = store();
        s.write_note("s", "one", "abc\n").unwrap();
        assert!(s.search_notes("zzz").unwrap().is_empty());
    }

    #[test]
    fn search_spans_sections() {
        let (s, _) = store();
        s.write_note("a", "n", "needle\n").unwrap();
        s.write_note("b", "n", "needle\n").unwrap();
        assert_eq!(s.search_notes("needle").unwrap().len(), 2);
    }

    #[test]
    fn io_error_display() {
        let e = StoreError::Io {
            path: "/p".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::Other, "boom"),
        };
        assert_eq!(e.to_string(), "IO error at /p: boom");
    }
}
