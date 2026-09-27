//! Typed access to a TOML table that names the key in every error (`files.allow: expected an array of strings`) and
//! rejects keys nobody reads, so a typo in a configuration file fails loudly instead of being ignored.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fmt;
use toml::{Table, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TomlError(pub String);

impl fmt::Display for TomlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TomlError {}

pub type TomlResult<T> = Result<T, TomlError>;

/// [text] as a TOML document, or the parser's error on one line.
pub fn parse(text: &str) -> TomlResult<Table> {
    text.parse::<Table>().map_err(|e| {
        // The parser's report spans lines, with the source under it: where, and then what.
        let text = e.to_string();
        let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        let where_ = lines.first().copied().unwrap_or("bad TOML").trim_start_matches("TOML parse error at ");
        let what = lines.last().copied().unwrap_or_default();
        TomlError(if lines.len() > 1 { format!("{where_}: {what}") } else { where_.to_string() })
    })
}

pub struct Reader<'a> {
    table: &'a Table,
    at: String,
    read: RefCell<BTreeSet<String>>,
}

impl<'a> Reader<'a> {
    pub fn new(table: &'a Table) -> Self {
        Self::at(table, "")
    }

    fn at(table: &'a Table, at: &str) -> Self {
        Self { table, at: at.into(), read: RefCell::new(BTreeSet::new()) }
    }

    fn get(&self, key: &str) -> Option<&'a Value> {
        self.read.borrow_mut().insert(key.into());
        self.table.get(key)
    }

    pub fn fail<T>(&self, key: &str, message: &str) -> TomlResult<T> {
        Err(TomlError(format!("{}: {message}", self.path(key))))
    }

    fn path(&self, key: &str) -> String {
        if self.at.is_empty() { key.into() } else { format!("{}.{key}", self.at) }
    }

    pub fn string(&self, key: &str) -> TomlResult<Option<String>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => self.fail(key, "expected a string"),
        }
    }

    pub fn long(&self, key: &str) -> TomlResult<Option<i64>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Integer(n)) => Ok(Some(*n)),
            Some(_) => self.fail(key, "expected an integer"),
        }
    }

    pub fn int(&self, key: &str) -> TomlResult<Option<i32>> {
        match self.long(key)? {
            None => Ok(None),
            Some(n) => i32::try_from(n).map(Some).or_else(|_| self.fail(key, "out of range")),
        }
    }

    pub fn bool(&self, key: &str) -> TomlResult<Option<bool>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Boolean(b)) => Ok(Some(*b)),
            Some(_) => self.fail(key, "expected true or false"),
        }
    }

    pub fn strings(&self, key: &str) -> TomlResult<Option<Vec<String>>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| v.as_str().map(String::from).ok_or(()))
                .collect::<Result<Vec<_>, _>>()
                .map(Some)
                .or_else(|_| self.fail(key, "expected an array of strings")),
            Some(_) => self.fail(key, "expected an array of strings"),
        }
    }

    pub fn longs(&self, key: &str) -> TomlResult<Option<Vec<i64>>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| v.as_integer().ok_or(()))
                .collect::<Result<Vec<_>, _>>()
                .map(Some)
                .or_else(|_| self.fail(key, "expected an array of integers")),
            Some(_) => self.fail(key, "expected an array of integers"),
        }
    }

    pub fn raw(&self, key: &str) -> Option<&'a Value> {
        self.get(key)
    }

    pub fn table(&self, key: &str) -> TomlResult<Option<Reader<'a>>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Table(t)) => Ok(Some(Reader::at(t, &self.path(key)))),
            Some(_) => self.fail(key, "expected a table"),
        }
    }

    /// Every entry of this table as a sub-table, for tables whose keys the operator names (`[nodes.<name>]`).
    pub fn tables(&self) -> TomlResult<Vec<(String, Reader<'a>)>> {
        let keys: Vec<String> = self.table.keys().cloned().collect();
        keys.into_iter().map(|k| Ok((k.clone(), self.table(&k)?.expect("the key is there")))).collect()
    }

    /// Fails on any key that no accessor asked for. Call after reading everything.
    pub fn reject_unknown(&self) -> TomlResult<()> {
        let read = self.read.borrow();
        match self.table.keys().find(|k| !read.contains(*k)) {
            Some(k) => self.fail(k, "unknown key"),
            None => Ok(()),
        }
    }
}

/// [value] as a TOML basic string: whatever it holds stays a value, and can't start a table or a key.
pub fn quote(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_names_the_key_and_rejects_unknown_ones() {
        let table = parse("[files]\nallow = [1]\ntypo = 2").unwrap();
        let root = Reader::new(&table);
        let files = root.table("files").unwrap().unwrap();
        assert_eq!(files.strings("allow").unwrap_err().0, "files.allow: expected an array of strings");
        assert_eq!(files.reject_unknown().unwrap_err().0, "files.typo: unknown key");
    }

    #[test]
    fn a_quoted_string_stays_one_value() {
        for hostile in ["x\"\n[scripts]\nchecks = \"/tmp\"\n#", "a\\\"b", "tab\there", "\u{0}\u{7f}", "é ñ"] {
            let table = parse(&format!("k = {}", quote(hostile))).unwrap();
            assert_eq!(table.keys().collect::<Vec<_>>(), ["k"]);
            assert_eq!(table["k"].as_str(), Some(hostile));
        }
    }

    #[test]
    fn errors_are_one_line() {
        let e = parse("a = 1\n\nb = ?").unwrap_err();
        assert!(!e.0.contains('\n'), "{e}");
    }
}
