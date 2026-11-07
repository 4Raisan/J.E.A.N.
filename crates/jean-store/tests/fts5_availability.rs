//! Does the SQLite this build ships actually have FTS5?
//!
//! The searchable-history feature is built on `USING fts5`, so this is checked
//! here rather than assumed from the `bundled` feature flag. `bundled` makes
//! `libsqlite3-sys` compile SQLite from source inside this build; which optional
//! modules that compilation enabled is a property of the resulting binary, not of
//! the dependency name. An FTS5 search that silently degraded into a `LIKE`
//! scan over every row would still *answer* — it would just be unbounded — so
//! the probe has to be the real thing: create the virtual table and match it.

use rusqlite::Connection;

/// The compile-time options SQLite itself reports, as one lower-case string.
fn compile_options(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("PRAGMA compile_options")
        .expect("PRAGMA compile_options is core SQLite");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("read compile options");
    rows.filter_map(Result::ok).collect()
}

#[test]
fn the_shipped_sqlite_reports_fts5_as_compiled_in() {
    let conn = Connection::open_in_memory().unwrap();
    let options = compile_options(&conn);
    // SQLite reports the option without the `SQLITE_` prefix it carries as a C
    // define, so match what the pragma actually prints rather than the macro.
    assert!(
        options
            .iter()
            .any(|o| o.eq_ignore_ascii_case("ENABLE_FTS5")),
        "FTS5 is not in the compile options this build reports; got {options:?}"
    );
}

#[test]
fn an_fts5_virtual_table_can_be_created_matched_and_snippeted() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE VIRTUAL TABLE probe USING fts5(text, tokenize = 'unicode61 remove_diacritics 2');
         INSERT INTO probe(rowid, text) VALUES (1, 'open the report and print it');
         INSERT INTO probe(rowid, text) VALUES (2, 'rename the file to invoice_final');",
    )
    .expect(
        "creating an fts5 table must work in this build; if this fails, \
             searchable history is not possible and must be reported as an \
             incompatibility, not replaced with a scan",
    );

    let hits: Vec<i64> = conn
        .prepare("SELECT rowid FROM probe WHERE probe MATCH ?1 ORDER BY rowid DESC LIMIT ?2")
        .unwrap()
        .query_map(rusqlite::params!["invoice", 5], |row| row.get(0))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(
        hits,
        vec![2],
        "the MATCH should find the row that says invoice"
    );

    // Snippets are the second half of the promise: a result has to show *where*
    // it matched without returning the whole column. FTS5 marks the matched
    // token, which may be a prefix of the indexed word, not the word around it.
    let snippet: String = conn
        .prepare("SELECT snippet(probe, 0, '[', ']', '…', 8) FROM probe WHERE probe MATCH ?1")
        .unwrap()
        .query_row(rusqlite::params!["invoice"], |row| row.get(0))
        .unwrap();
    assert!(
        snippet.contains("[invoice]"),
        "snippet did not mark the match: {snippet:?}"
    );
}

#[test]
fn a_malformed_fts5_query_is_an_error_and_not_a_result_set() {
    // The search API has to be able to tell an operator their query was
    // unparseable. FTS5 raises `SQL logic error` (or a syntax error) at *step*
    // time, not at prepare time — so a wrapper that only checks `prepare` would
    // let a broken query through as "no results".
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE VIRTUAL TABLE probe USING fts5(text);
         INSERT INTO probe(rowid, text) VALUES (1, 'plain words');",
    )
    .unwrap();

    let mut stmt = conn
        .prepare("SELECT rowid FROM probe WHERE probe MATCH ?1")
        .unwrap();
    // rusqlite can surface an FTS5 syntax error either when the query starts or
    // when the first row is pulled, so both shapes have to be examined before
    // claiming the error was caught.
    let outcome = stmt
        .query_map(rusqlite::params!["AND OR"], |row| row.get::<_, i64>(0))
        .map(|mut rows| rows.next().map(|row| row.map(|_| ())));
    let error = match outcome {
        Err(e) => e,
        Ok(None) => panic!("a malformed FTS5 query was accepted and matched nothing"),
        Ok(Some(Ok(()))) => panic!("a malformed FTS5 query was accepted and returned a row"),
        Ok(Some(Err(e))) => e,
    };
    let text = error.to_string();
    assert!(
        text.contains("fts5") || text.contains("fts"),
        "expected an fts5 syntax error, got: {text}"
    );
}

#[test]
fn a_quoted_fts5_phrase_is_matched_literally() {
    // Literal mode quotes the user's text, which is what stops a term like
    // `NEAR` or a stray `:` from being read as query syntax.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE VIRTUAL TABLE probe USING fts5(text);
         INSERT INTO probe(rowid, text) VALUES (1, 'the word NEAR appeared');
         INSERT INTO probe(rowid, text) VALUES (2, 'nothing relevant here');",
    )
    .unwrap();

    let count = |query: &str| {
        let mut stmt = conn
            .prepare("SELECT COUNT(*) FROM probe WHERE probe MATCH ?1")
            .unwrap();
        stmt.query_row(rusqlite::params![query], |row| row.get::<_, i64>(0))
            .unwrap()
    };
    assert_eq!(count("\"NEAR\""), 1, "a quoted term is a literal match");
    assert_eq!(
        count("\"NOT\""),
        0,
        "quoting also stops an operator's word from acting as an operator"
    );
}
