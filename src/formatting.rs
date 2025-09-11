use std::fmt::{self, Display};

/// Bytes formatter, prints in human-readable format (e.g., 1.23 MB)
///
/// The precision can be set using the standard precision syntax (e.g., {:.3} for 3 decimal places).
///
/// The alternate format (using `#` in the format string) uses binary prefixes (e.g., KiB, MiB).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Bytes(pub u64);

/// Time formatter, prints in human-readable format (e.g., 1.23s, 1.23min)
///
/// The time is represented in milliseconds. The alternate format (using `#` in the format string)
/// prints the long format (e.g., "1.23 seconds", "1.23 minutes").
///
/// The precision can be set using the standard precision syntax (e.g., {:.3} for 3 decimal places).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct DecimalTime(pub f64);

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct TimeBreakdown(pub f64);

/// Speed formatter, prints in human-readable format (e.g., 1.23 MB/s)
///
/// The precision can be set using the standard precision syntax (e.g., {:.3} for 3 decimal places).
///
/// The alternate format (using `#` in the format string) uses binary prefixes (e.g., MiB/s).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Speed(pub Bytes, pub DecimalTime);

impl Speed {
    pub const fn new(bytes: u64, millis: f64) -> Self {
        Self(Bytes(bytes), DecimalTime(millis))
    }

    pub const fn as_bps(&self) -> Option<f64> {
        let Speed(Bytes(bytes), DecimalTime(millis)) = *self;

        if millis <= 0.0 { None } else { Some(bytes as f64 / (millis * 0.001)) }
    }

    pub const fn is_zero(&self) -> bool {
        let Speed(Bytes(bytes), DecimalTime(millis)) = *self;

        bytes == 0 || millis == 0.0
    }

    /// Estimates the time in milliseconds to process the given number of bytes at the current speed.
    pub const fn estimate_time(&self, bytes: u64) -> Option<f64> {
        let Speed(Bytes(current_bytes), DecimalTime(millis)) = *self;

        if current_bytes == 0 || millis == 0.0 {
            None
        } else {
            Some((bytes as f64 / current_bytes as f64) * millis)
        }
    }
}

impl Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const SI_SUFFIXES: [&str; 7] = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
        const BINARY_SUFFIXES: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

        let (divisor, suffixes) = if f.alternate() { (1024.0, BINARY_SUFFIXES) } else { (1000.0, SI_SUFFIXES) };

        let mut size = self.0 as f64;
        let mut suffix_index = 0;
        while size >= divisor && suffix_index < SI_SUFFIXES.len() - 1 {
            size /= divisor;
            suffix_index += 1;
        }
        let p = f.precision().unwrap_or(2);

        write!(f, "{:.p$} {}", size, suffixes[suffix_index], p = p)
    }
}

impl Display for DecimalTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let millis = self.0.max(0.0); // clamp to non-negative

        let p = f.precision().unwrap_or(2);

        const SHORT: [&str; 5] = ["ms", "s", "min", "h", "d"];
        const LONG: [&str; 5] = [" milliseconds", " seconds", " minutes", " hours", " days"];

        let units = if f.alternate() { LONG } else { SHORT };

        if millis < 1000.0 {
            write!(f, "{:.p$}{}", millis, units[0], p = p)
        } else if millis < 60_000.0 {
            write!(f, "{:.p$}{}", millis / 1000.0, units[1], p = p)
        } else if millis < 3_600_000.0 {
            write!(f, "{:.p$}{}", millis / 60_000.0, units[2], p = p)
        } else if millis < 86_400_000.0 {
            write!(f, "{:.p$}{}", millis / 3_600_000.0, units[3], p = p)
        } else {
            write!(f, "{:.p$}{}", millis / 86_400_000.0, units[4], p = p)
        }
    }
}

impl Display for TimeBreakdown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let millis = self.0.max(0.0); // clamp to non-negative

        let p = f.precision().unwrap_or(2);

        const SHORT: [&str; 5] = ["ms", "s", "min", "h", "d"];
        const LONG: [&str; 5] = [" milliseconds", " seconds", " minutes", " hours", " days"];

        let units = if f.alternate() { LONG } else { SHORT };

        if millis < 1000.0 {
            return write!(f, "{:.p$}{}", millis, units[0], p = p);
        }

        let remaining = millis;
        let (days, remaining) = ((remaining / 86_400_000.0).floor() as u64, remaining % 86_400_000.0);
        let (hours, remaining) = ((remaining / 3_600_000.0).floor() as u64, remaining % 3_600_000.0);
        let (minutes, remaining) = ((remaining / 60_000.0).floor() as u64, remaining % 60_000.0);
        let seconds = remaining / 1000.0;

        let mut space = "";
        if days > 0 {
            write!(f, "{space}{days}{}", units[4])?;
            space = " ";
        }
        if hours > 0 {
            write!(f, "{space}{hours}{}", units[3])?;
            space = " ";
        }
        if minutes > 0 {
            write!(f, "{space}{minutes}{}", units[2])?;
            space = " ";
        }
        if seconds > 0.0 {
            write!(f, "{space}{:.p$}{}", seconds, units[1], p = p)?;
        }

        Ok(())
    }
}

impl Display for Speed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Speed(Bytes(bytes), DecimalTime(millis)) = *self;

        if millis == 0.0 {
            return f.write_str("N/A");
        }

        // passes precision/alternate to Bytes
        Bytes((bytes as f64 / (millis / 1000.0)) as u64).fmt(f)?;
        f.write_str("/s")
    }
}

pub fn strip_non_ascii(s: String, replacement: Option<&str>) -> String {
    s.replace(|c: char| !c.is_ascii(), replacement.unwrap_or("?"))
}
