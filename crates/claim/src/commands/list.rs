//! `claim list`: the store inventory — a plain list of the claims the store holds.
//!
//! This runs nothing and computes no status: the CLI stores no verdicts, so there
//! is no history to derive a status from (see `docs/design/CLI-HUB-BOUNDARY.md`).
//! Freshness and staleness are the hub's, derived from the reported stream it holds.
//! `list` reports what the store *contains* — id, statement, file, supports count —
//! as a filtered, aligned table (or a `--json` array).
//!
//! Filters combine with AND — every one given must hold — so a claim survives to
//! the output only if it matches the path prefix *and* the supports target *and* the
//! text term, and so on. This makes `--path src/ --supports x` mean exactly "claims
//! under src/ that support x", which is what a reader expects.

use anyhow::{Context, Result};
use claim_core::Claim;
use serde::Serialize;

use crate::cli::ListArgs;
use crate::output::{emit, Format};
use claim_store::{claim_matches_path, discover, LoadError, LoadedClaim};

/// Run `claim list`.
///
/// A malformed or duplicate-id claim file is *reported* (in the envelope's `errors`
/// array, or an error line in the human output) rather than aborting the listing —
/// the well-formed claims still show. Their presence then makes the command exit 2:
/// the good claims are emitted first, then an `Err` is not returned — the exit code
/// carries the fault. Loud and useful, never a store silenced by one bad file.
///
/// Returns the process exit code: `0` when every file loaded, `2` when any claim
/// file could not be loaded (the good claims are still listed).
///
/// # Errors
///
/// Fails only when no store is found. A per-file load error is not an `Err` — it is
/// reported in the output and reflected in the returned exit code.
pub fn run(args: &ListArgs, format: Format) -> Result<i32> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let store = discover(&cwd)?;
    let load = store.load_all()?;

    let mut rows = Vec::new();
    for loaded in &load.claims {
        if keep(args, loaded) {
            rows.push(Row::new(loaded));
        }
    }

    let exit = if load.errors.is_empty() { 0 } else { 2 };
    let inventory = Inventory {
        status: "ok",
        exit,
        claims: &rows,
        errors: &load.errors,
    };
    emit(format, &inventory, || human(&rows, &load.errors))?;
    Ok(exit)
}

/// Whether a claim passes every active filter (AND semantics).
///
/// A filter that was not given is not a constraint. Kept as one predicate so the
/// AND-combination is in one place and a new filter is one clause, not a new pass
/// over the corpus.
fn keep(args: &ListArgs, loaded: &LoadedClaim) -> bool {
    if let Some(prefix) = &args.path {
        if !claim_matches_path(&loaded.path, &loaded.claim.supports, prefix) {
            return false;
        }
    }

    if let Some(target) = &args.supports {
        if !loaded
            .claim
            .supports
            .iter()
            .any(|s| s.as_str() == target.as_str())
        {
            return false;
        }
    }

    if let Some(term) = &args.term {
        if !text_matches(&loaded.claim, term) {
            return false;
        }
    }

    true
}

/// Whether the term occurs in the claim's id or statement (case-sensitive
/// substring). A plain positional argument to `list` is this quick find.
fn text_matches(claim: &Claim, term: &str) -> bool {
    claim.id.as_str().contains(term) || claim.statement.contains(term)
}

/// The machine form of `claim list`: a self-describing envelope, so it matches the
/// shape of `check`/`drift` (an object with `status`, not a bare array), and carries
/// the load errors.
#[derive(Debug, Serialize)]
struct Inventory<'a> {
    /// Always `"ok"`: the verb ran.
    status: &'static str,
    /// The exit code (0 clean, 2 when a claim file could not be loaded), so a
    /// consumer that captured stdout need not also inspect `$?`. Matches `check`/`drift`.
    exit: i32,
    /// The matching claims.
    claims: &'a [Row],
    /// Per-file load errors (malformed or duplicate-id files); reported, not fatal.
    /// A non-empty list makes the command exit 2.
    errors: &'a [LoadError],
}

/// One inventory row: the machine form of a listed claim.
#[derive(Debug, Serialize)]
struct Row {
    /// The claim's id.
    id: String,
    /// The plain-language statement, so a reader sees what the claim asserts without
    /// opening the file.
    statement: String,
    /// The claim file's path relative to the store root.
    file: String,
    /// How many `supports` targets the claim declares.
    supports: usize,
}

impl Row {
    fn new(loaded: &LoadedClaim) -> Self {
        Row {
            id: loaded.claim.id.to_string(),
            statement: loaded.claim.statement.trim().to_owned(),
            file: loaded.path.clone(),
            supports: loaded.claim.supports.len(),
        }
    }
}

/// The table's fixed column headers, in order.
const HEADERS: [&str; 3] = ["ID", "SUPPORTS", "FILE"];

/// Print the inventory as an aligned table (and any load errors), or a friendly
/// note when empty.
///
/// Every column's width is `max(header width, widest cell)`, so the header row and
/// the data rows always line up regardless of the longest id.
fn human(rows: &[Row], errors: &[LoadError]) {
    if rows.is_empty() {
        println!("No claims match.");
    } else {
        let cells: Vec<[String; 3]> = rows.iter().map(row_cells).collect();
        let widths = column_widths(&cells);
        print_row(&header_cells(), &widths);
        for cell in &cells {
            print_row(cell, &widths);
        }
    }

    for err in errors {
        println!("error: {}: {}", err.file, err.message);
    }
}

/// The header cells as owned strings, so they share the printing path with data.
fn header_cells() -> [String; 3] {
    HEADERS.map(ToOwned::to_owned)
}

/// The three display cells for one row, in header order.
fn row_cells(row: &Row) -> [String; 3] {
    [row.id.clone(), row.supports.to_string(), row.file.clone()]
}

/// The width of each column: the widest of its header and every cell.
fn column_widths(cells: &[[String; 3]]) -> [usize; 3] {
    let mut widths = HEADERS.map(str::len);
    for row in cells {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    widths
}

/// Print one row left-aligned to `widths`, two spaces between columns. The last
/// column is not padded (nothing follows it).
fn print_row(cells: &[String; 3], widths: &[usize; 3]) {
    let mut line = String::new();
    for (i, (cell, w)) in cells.iter().zip(widths).enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        if i == cells.len() - 1 {
            line.push_str(cell);
        } else {
            line.push_str(&format!("{cell:<w$}"));
        }
    }
    println!("{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(id: &str, statement: &str, supports: &[&str]) -> Claim {
        let supports_block = if supports.is_empty() {
            String::new()
        } else {
            let mut b = String::from("supports:\n");
            for s in supports {
                b.push_str(&format!("  - {s}\n"));
            }
            b
        };
        let text = format!(
            "---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"true\"\n{supports_block}---\n{statement}\n"
        );
        claim_core::parse_claim_file(&format!(".claims/{id}.md"), &text).unwrap()
    }

    #[test]
    fn text_matches_id_or_statement() {
        let c = claim("payments/pin", "We pin libfoo at 4.2.", &[]);
        assert!(text_matches(&c, "libfoo"));
        assert!(text_matches(&c, "payments"));
        assert!(!text_matches(&c, "nope"));
    }

    #[test]
    fn a_row_carries_id_statement_file_and_supports_count() {
        let c = claim("pin", "A fact.", &["a", "b"]);
        let loaded = LoadedClaim {
            claim: c,
            path: ".claims/pin.md".to_owned(),
        };
        let row = Row::new(&loaded);
        assert_eq!(row.id, "pin");
        assert_eq!(row.statement, "A fact.");
        assert_eq!(row.file, ".claims/pin.md");
        assert_eq!(row.supports, 2);
    }
}
