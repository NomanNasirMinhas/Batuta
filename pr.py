p = 'crates/batuta-app/src/tui/mod.rs'
s = open(p, encoding='utf-8').read()


def sub(old, new):
    global s
    assert old in s, f"NOT FOUND: {old[:70]!r}"
    s = s.replace(old, new, 1)


sub(
    "    use super::{apply_dupe_result, handle_key, refresh, reveal_arg, Source};\n    use batuta_ipc::Response;",
    "    use super::{apply_dupe_result, handle_key, refresh, reveal_arg, Source};\n    use batuta_ipc::{Response, Row};",
)

sub(
    "    fn press(app: &mut App, code: KeyCode) {",
    r'''    #[test]
    fn moving_off_a_row_cancels_the_second_half_of_a_tab() {
        let mut app = App::default();
        typed(&mut app, "file");
        app.apply_rows(
            vec![Row {
                path: r"C:\data\file_0001.bin".into(),
                size: 1,
                mtime: 0,
                is_dir: false,
                files: 0,
                own: 0,
            }],
            1,
            0,
            10,
        );

        press(&mut app, KeyCode::Tab);
        assert_eq!(app.query, r"C:\data\", "first Tab steps into the directory");
        assert!(app.pending_file.is_some(), "the file is owed a second Tab");

        // Any other key means the user moved on, and completing later to a
        // file they have since navigated away from would be baffling.
        press(&mut app, KeyCode::Down);
        assert_eq!(app.pending_file, None, "the second step must not survive");
    }

    fn press(app: &mut App, code: KeyCode) {''',
)

open(p, 'w', encoding='utf-8').write(s)
print("mod.rs test added")

p = 'README.md'
s = open(p, encoding='utf-8').read()
sub(
    "| `Tab` | complete the query to the highlighted directory (search view only) |",
    "| `Tab` | complete to the highlighted directory, or a file's directory then the file (search view only) |",
)
sub(
    r'''`Tab` completes the drill-down: the highlighted directory's real path becomes
the query, with a trailing separator so you keep narrowing inside it. It works
from a plain name search too — you rarely know the path you want in advance,
which is the whole reason for searching — so `downloads`, `Tab` on the folder
you meant, and carry on typing inside it. Only directories complete: a file has
nothing to go deeper into, and completing to one would leave a query matching
just that file.''',
    r'''`Tab` completes the drill-down: the highlighted directory's real path becomes
the query, with a trailing separator so you keep narrowing inside it. It works
from a plain name search too — you rarely know the path you want in advance,
which is the whole reason for searching — so `downloads`, `Tab` on the folder
you meant, and carry on typing inside it.

A file completes in two steps. The first `Tab` lands in the directory holding
it, which puts the siblings on screen and is usually what you were after; a
second `Tab` names the file itself. Going straight to the file would leave a
query matching only that one file, with nothing left to narrow. The file is
remembered across the first step rather than re-derived, because completing the
directory refetches and moves the selection off it. Any key other than `Tab`
drops the second step.''',
)
open(p, 'w', encoding='utf-8').write(s)
print("README updated")
