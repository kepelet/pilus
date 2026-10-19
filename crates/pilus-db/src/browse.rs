//! A read-only look at what the database holds, for a page that shows it.
//!
//! Nothing here writes. Table names come from `sqlite_master` and are checked
//! against it before they reach a query, so a name a caller made up is
//! [`Error::NoTable`] and never SQL. Cells are cut to [`CELL_LIMIT`] characters
//! on the way out, because a cached GitHub body or a long message would
//! otherwise be copied whole into a row of a table meant to be skimmed.

use rusqlite::types::ValueRef;

use crate::Db;
use crate::error::{Error, Result};

/// The longest text kept in one cell, in characters.
pub const CELL_LIMIT: usize = 240;

/// One table: how big it is and what its columns are.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableInfo {
    pub name: String,
    pub rows: u64,
    pub columns: Vec<String>,
}

/// One value, as the page shows it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Cell {
    Null,
    Integer(i64),
    Real(String),
    /// Text cut to [`CELL_LIMIT`] characters; `cut` says whether it was.
    Text { text: String, cut: bool },
    /// Bytes are not shown, only how many there are.
    Blob(usize),
}

impl Cell {
    /// The cell as one line of text, newlines flattened so a row stays a row.
    pub fn line(&self) -> String {
        match self {
            Self::Null => "NULL".to_string(),
            Self::Integer(value) => value.to_string(),
            Self::Real(value) => value.clone(),
            Self::Text { text, cut } => {
                let flat: String = text
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect();
                if *cut { format!("{flat}…") } else { flat }
            }
            Self::Blob(len) => format!("<{len} bytes>"),
        }
    }

    /// Whether this is NULL, which a page draws muted.
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

/// A window onto a table's rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rows {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    /// Rows the table has in all.
    pub total: u64,
    /// The first row shown, counted from zero.
    pub offset: u64,
}

/// What the database file is like, for the page's header.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Summary {
    /// `PRAGMA user_version`: how many migrations have run.
    pub schema: i64,
    /// Bytes the file takes, without its log.
    pub bytes: u64,
    pub tables: Vec<TableInfo>,
}

impl Db {
    /// Every table (not SQLite's own), with its row count and columns.
    pub fn summary(&self) -> Result<Summary> {
        self.with(|conn| {
            let schema: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
            let pages: i64 = conn.pragma_query_value(None, "page_count", |row| row.get(0))?;
            let page_size: i64 = conn.pragma_query_value(None, "page_size", |row| row.get(0))?;
            let mut names = conn.prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )?;
            let names: Vec<String> = names
                .query_map([], |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            let mut tables = Vec::with_capacity(names.len());
            for name in names {
                tables.push(TableInfo {
                    rows: count(conn, &name)?,
                    columns: columns(conn, &name)?,
                    name,
                });
            }
            Ok(Summary {
                schema,
                bytes: (pages.max(0) as u64) * (page_size.max(0) as u64),
                tables,
            })
        })
    }

    /// Up to `limit` rows of `table` from `offset`, in the table's own order.
    pub fn rows(&self, table: &str, offset: u64, limit: u64) -> Result<Rows> {
        self.with(|conn| {
            let known: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master
                 WHERE type = 'table' AND name = ?1 AND name NOT LIKE 'sqlite_%')",
                [table],
                |row| row.get(0),
            )?;
            if !known {
                return Err(Error::NoTable(table.to_string()));
            }
            let columns = columns(conn, table)?;
            let mut statement =
                conn.prepare(&format!("SELECT * FROM {} LIMIT ?1 OFFSET ?2", quote(table)))?;
            let width = columns.len();
            let rows = statement
                .query_map([limit as i64, offset as i64], |row| {
                    (0..width)
                        .map(|index| row.get_ref(index).map(cell))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(Rows {
                columns,
                rows,
                total: count(conn, table)?,
                offset,
            })
        })
    }
}

fn count(conn: &rusqlite::Connection, table: &str) -> Result<u64> {
    let rows: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {}", quote(table)), [], |row| {
        row.get(0)
    })?;
    Ok(rows.max(0) as u64)
}

fn columns(conn: &rusqlite::Connection, table: &str) -> Result<Vec<String>> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({})", quote(table)))?;
    Ok(statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<_, _>>()?)
}

/// `name` as an identifier, which only ever comes from `sqlite_master`.
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn cell(value: ValueRef<'_>) -> Cell {
    match value {
        ValueRef::Null => Cell::Null,
        ValueRef::Integer(value) => Cell::Integer(value),
        ValueRef::Real(value) => Cell::Real(value.to_string()),
        ValueRef::Text(bytes) => {
            let text = String::from_utf8_lossy(bytes);
            let mut chars = text.chars();
            let kept: String = chars.by_ref().take(CELL_LIMIT).collect();
            Cell::Text {
                cut: chars.next().is_some(),
                text: kept,
            }
        }
        ValueRef::Blob(bytes) => Cell::Blob(bytes.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_lists_every_table_with_its_rows_and_columns() {
        let db = Db::in_memory();
        db.put_doc("a", &1u8).unwrap();
        db.put_doc("b", &2u8).unwrap();
        let summary = db.summary().unwrap();
        assert_eq!(summary.schema, 1);
        let docs = summary.tables.iter().find(|t| t.name == "docs").unwrap();
        assert_eq!(docs.rows, 2);
        assert_eq!(docs.columns, ["name", "body", "updated"]);
        assert!(summary.tables.iter().any(|t| t.name == "chat_lines"));
        assert!(summary.tables.iter().all(|t| !t.name.starts_with("sqlite_")));
        assert!(summary.bytes > 0);
    }

    #[test]
    fn rows_come_back_in_pages_with_the_total() {
        let db = Db::in_memory();
        for n in 0..5 {
            db.put_doc(&format!("d{n}"), &n).unwrap();
        }
        let page = db.rows("docs", 2, 2).unwrap();
        assert_eq!((page.total, page.offset, page.rows.len()), (5, 2, 2));
        assert_eq!(page.rows[0][0].line(), "d2");
        assert_eq!(db.rows("docs", 4, 10).unwrap().rows.len(), 1);
        assert!(db.rows("docs", 9, 10).unwrap().rows.is_empty());
    }

    #[test]
    fn long_text_is_cut_and_other_types_are_named() {
        let db = Db::in_memory();
        db.with(|conn| {
            conn.execute("CREATE TABLE odd (t, i, r, b, n)", [])?;
            conn.execute(
                "INSERT INTO odd VALUES (?1, 7, 1.5, x'0102', NULL)",
                [&"é".repeat(CELL_LIMIT + 5)],
            )?;
            Ok(())
        })
        .unwrap();
        let row = &db.rows("odd", 0, 1).unwrap().rows[0];
        let Cell::Text { text, cut } = &row[0] else {
            panic!("text");
        };
        assert_eq!((text.chars().count(), *cut), (CELL_LIMIT, true));
        assert!(row[0].line().ends_with('…'));
        assert_eq!(row[1], Cell::Integer(7));
        assert_eq!(row[2].line(), "1.5");
        assert_eq!(row[3].line(), "<2 bytes>");
        assert!(row[4].is_null());
    }

    #[test]
    fn a_name_that_is_not_a_table_never_reaches_sql() {
        let db = Db::in_memory();
        for name in ["nope", "docs; DROP TABLE docs", "sqlite_master", "\"docs\""] {
            assert!(matches!(db.rows(name, 0, 1), Err(Error::NoTable(_))), "{name}");
        }
        assert_eq!(db.summary().unwrap().tables.iter().filter(|t| t.name == "docs").count(), 1);
    }
}
