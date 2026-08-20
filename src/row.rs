//! The picker's row protocol: one tab-separated line per candidate.
//!
//! The model renders these lines, fzf displays and ranks the last field, and
//! the picker parses the key fields back when the user picks a row. The field
//! order is fixed by the fzf flags below, so the layout and the flags live
//! together.

use crate::status::SyncKind;

/// Fields per row: branch, path, entry kind, sync kind, changes, display.
pub const FIELD_COUNT: usize = 6;

/// fzf's field separator for both the interactive picker and the internal
/// ranking filter.
pub const DELIMITER: &str = "--delimiter=\t";

/// Only the last field is shown and searched; the rest is the payload the
/// picker acts on.
pub const WITH_NTH: &str = "--with-nth=6";

/// What fzf echoes back for the selected row: every field except the display
/// one, joined by the delimiter (see [`PickerRow::parse_selection`]).
pub const ACCEPT_NTH: &str = "--accept-nth=1,2,3,4,5";

/// Entry kinds. `create` and `pr` are action rows the picker appends to the
/// ranked list rather than candidates the model produced.
pub const KIND_WORKTREE: &str = "worktree";
pub const KIND_BRANCH: &str = "branch";
pub const KIND_REMOTE: &str = "remote";
pub const KIND_CREATE: &str = "create";
pub const KIND_PR: &str = "pr";
/// A divider the picker skips rather than acts on.
pub const KIND_SECTION: &str = "section";

/// One picker row. The borrowed form keeps rendering allocation-free and lets
/// the picker route a selection without copying its fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PickerRow<'a> {
    pub branch: &'a str,
    pub path: &'a str,
    pub entry_kind: &'a str,
    pub sync_kind: &'a str,
    pub changes: &'a str,
    pub display: &'a str,
}

impl<'a> PickerRow<'a> {
    /// An action row the picker synthesizes from the current query: it has a
    /// branch name and a kind, but no worktree behind it yet.
    pub fn action(branch: &'a str, entry_kind: &'a str, display: &'a str) -> Self {
        Self {
            branch,
            path: "",
            entry_kind,
            sync_kind: "",
            changes: "",
            display,
        }
    }

    /// Parse a full rendered row. `None` unless it carries exactly
    /// [`FIELD_COUNT`] fields, which also rejects a display field that has
    /// smuggled in a tab.
    pub fn parse(line: &'a str) -> Option<Self> {
        let mut fields = line.split('\t');
        let row = Self {
            branch: fields.next()?,
            path: fields.next()?,
            entry_kind: fields.next()?,
            sync_kind: fields.next()?,
            changes: fields.next()?,
            display: fields.next()?,
        };
        fields.next().is_none().then_some(row)
    }

    /// Parse the fields fzf echoes back for the selected row ([`ACCEPT_NTH`]).
    /// The display field is not among them, and fzf may drop trailing empty
    /// fields, so anything absent reads as empty — an action row's path and
    /// sync kind legitimately are.
    pub fn parse_selection(selection: &'a str) -> Self {
        let mut fields = selection.split('\t');
        Self {
            branch: fields.next().unwrap_or_default(),
            path: fields.next().unwrap_or_default(),
            entry_kind: fields.next().unwrap_or_default(),
            sync_kind: fields.next().unwrap_or_default(),
            changes: fields.next().unwrap_or_default(),
            display: "",
        }
    }

    pub fn sync_kind(&self) -> Option<SyncKind> {
        SyncKind::from_wire(self.sync_kind)
    }

    /// Append the row and its newline to `out`.
    pub fn write_line(&self, out: &mut String) {
        let Self {
            branch,
            path,
            entry_kind,
            sync_kind,
            changes,
            display,
        } = self;
        for field in [branch, path, entry_kind, sync_kind, changes] {
            out.push_str(field);
            out.push('\t');
        }
        out.push_str(display);
        out.push('\n');
    }

    pub fn to_line(&self) -> String {
        let mut line = String::new();
        self.write_line(&mut line);
        line
    }
}

/// The key fields of a rendered row — everything fzf does not display, which is
/// what identifies it. `None` for a line that is not a well-formed row.
pub fn key_fields(line: &str) -> Option<&str> {
    PickerRow::parse(line)?;
    line.rsplit_once('\t').map(|(key, _display)| key)
}

#[cfg(test)]
mod tests {
    use super::{key_fields, PickerRow, ACCEPT_NTH, DELIMITER, FIELD_COUNT, WITH_NTH};
    use crate::status::SyncKind;

    /// The fzf flags index the fields by position, so they only stay correct as
    /// long as they agree with the layout.
    #[test]
    fn the_fzf_flags_match_the_field_layout() {
        assert_eq!(WITH_NTH, format!("--with-nth={FIELD_COUNT}"));
        let keys: Vec<String> = (1..FIELD_COUNT).map(|field| field.to_string()).collect();
        assert_eq!(ACCEPT_NTH, format!("--accept-nth={}", keys.join(",")));
        assert_eq!(DELIMITER, "--delimiter=\t");
    }

    #[test]
    fn rows_round_trip_through_the_wire_format() {
        let row = PickerRow {
            branch: "kees/fix",
            path: "/wt/fix",
            entry_kind: "worktree",
            sync_kind: "ahead",
            changes: "clean",
            display: "kees/fix  \x1b[33m↑1\x1b[0m",
        };
        let line = row.to_line();
        assert_eq!(line.matches('\t').count(), FIELD_COUNT - 1);
        assert!(line.ends_with('\n'));
        assert_eq!(PickerRow::parse(line.trim_end_matches('\n')), Some(row));
        assert_eq!(row.sync_kind(), Some(SyncKind::Ahead));
    }

    #[test]
    fn only_six_field_lines_parse_as_rows() {
        assert_eq!(PickerRow::parse("a\tb\tc"), None);
        assert_eq!(PickerRow::parse("a\tb\tc\td\te\tf\tg"), None);
        assert_eq!(key_fields("a\tb\tc\td\te\tf"), Some("a\tb\tc\td\te"));
        assert_eq!(key_fields("a\tb\tc"), None);
    }

    /// fzf hands back only the key fields, and may drop trailing empty ones.
    #[test]
    fn a_selection_parses_with_or_without_its_trailing_empty_fields() {
        let full = PickerRow::parse_selection("query\t\tcreate\t\t");
        let trimmed = PickerRow::parse_selection("query\t\tcreate");
        for row in [full, trimmed] {
            assert_eq!(row.branch, "query");
            assert_eq!(row.path, "");
            assert_eq!(row.entry_kind, "create");
            assert_eq!(row.sync_kind, "");
            assert_eq!(row.changes, "");
        }
        assert_eq!(PickerRow::parse_selection("").branch, "");
    }

    #[test]
    fn an_action_row_has_a_branch_a_kind_and_a_display() {
        let row = PickerRow::action("7", super::KIND_PR, "#7  checkout");
        assert_eq!(row.to_line(), "7\t\tpr\t\t\t#7  checkout\n");
    }
}
