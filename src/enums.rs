use std::{error::Error, fmt::Display, str::FromStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMethod {
    #[default]
    None,
    /// Ascending by file size
    Asc,
    /// Descending by file size
    Desc,
    Rand,
    Name,
    /// Modification time
    MTime,
    /// Creation time
    CTime,
    /// Accessed time
    ATime,
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecentField {
    /// Modification time
    #[default]
    MTime,
    /// Creation time
    CTime,
    /// Accessed time
    ATime,
}

#[derive(Debug, Clone, Copy)]
pub struct InvalidSortMethod;

#[derive(Debug, Clone, Copy)]
pub struct InvalidRecentField;

impl Display for InvalidSortMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid sort method")
    }
}

impl Display for InvalidRecentField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid recent field")
    }
}

impl Error for InvalidSortMethod {}
impl Error for InvalidRecentField {}

impl FromStr for SortMethod {
    type Err = InvalidSortMethod;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const PATTERNS: [(&str, SortMethod); 8] = [
            ("none", SortMethod::None),
            ("asc", SortMethod::Asc),
            ("desc", SortMethod::Desc),
            ("rand", SortMethod::Rand),
            ("name", SortMethod::Name),
            ("mtime", SortMethod::MTime),
            ("ctime", SortMethod::CTime),
            ("atime", SortMethod::ATime),
        ];

        for (pattern, method) in PATTERNS {
            if s.eq_ignore_ascii_case(pattern) {
                return Ok(method);
            }
        }

        Err(InvalidSortMethod)
    }
}

impl FromStr for RecentField {
    type Err = InvalidRecentField;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const PATTERNS: [(&str, RecentField); 3] = [("mtime", RecentField::MTime), ("ctime", RecentField::CTime), ("atime", RecentField::ATime)];

        for (pattern, field) in PATTERNS {
            if s.eq_ignore_ascii_case(pattern) {
                return Ok(field);
            }
        }

        Err(InvalidRecentField)
    }
}
