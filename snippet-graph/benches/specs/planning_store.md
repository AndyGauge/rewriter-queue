A file-backed project planning store, crate deps: std and thiserror only.

`StoreError` (thiserror): `Io { path: String, source: std::io::Error }` ("IO error at {path}: {source}"), `NoteNotFound { section, slug }`, `PhaseNotFound { name }`.

`ProjectStore { root: PathBuf }` with `new(root: impl Into<PathBuf>)`.

Private helpers are free functions (not methods) and must all be used: ensure parent dir exists; write_file (creates parent dirs); read_file; list_md_files(dir) -> sorted file stems of `.md` files, empty if dir missing; walk_md_files(base, dir) -> sorted relative paths of `.md` files recursively.

Methods, all returning Result<_, StoreError>:
- get_mission() -> String: reads `<root>/mission.md`; if absent returns "# Mission\n\n*(not set — use set_mission to define the collapse objective)*\n".
- set_mission(content)
- set_phase(name, status, description): writes `<root>/phases/<name>.md` as "# Phase: {name}\n\n**Status:** {status}\n\n{description}\n"
- get_phase(name) (PhaseNotFound if missing), list_phases() -> Vec<String>
- write_note(section, slug, content) at `<root>/notes/<section>/<slug>.md`; read_note(section, slug) (NoteNotFound if missing)
- list_notes(section: Option<&str>) -> Vec<String>: with a section, returns "section/name" entries (no extension); without, all relative paths under notes with their `.md` extension, sorted. Error kinds other than those named here are unspecified.
- search_notes(query) -> Vec<(String, String)>: case-insensitive match over all notes; each result is (relative path, up to 3 matching lines joined by "\n").
