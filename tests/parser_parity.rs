//! The subject parser must agree with the original python implementation.
//! `fixtures/subjects.python.jsonl` was produced by running the python
//! `parse_subject` over `fixtures/subjects.txt`.

use atlas::parser::{is_obfuscated, parse_subject};
use serde_json::{Value, json};

#[test]
fn matches_python_parser() {
    let subjects = include_str!("fixtures/subjects.txt");
    let expected = include_str!("fixtures/subjects.python.jsonl");

    let mut checked = 0;

    for (subject, want) in subjects.lines().zip(expected.lines()) {
        let want: Value = serde_json::from_str(want).unwrap();
        let got = match parse_subject(subject) {
            None => Value::Null,
            Some(p) => json!({
                "filename": p.filename,
                "release_name": p.release_name,
                "part": p.part,
                "total_parts": p.total_parts,
                "file_index": p.file_index,
                "file_total": p.file_total,
                "obf": is_obfuscated(&p.release_name),
            }),
        };

        assert_eq!(got, want, "subject: {subject}");
        checked += 1;
    }

    assert_eq!(checked, subjects.lines().count());
}
