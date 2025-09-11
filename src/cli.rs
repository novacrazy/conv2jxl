use std::collections::HashSet;
use std::ops::{Deref, RangeInclusive};
use std::path::PathBuf;
use std::{error::Error, fmt::Display, str::FromStr};

/// Convert files and directories to JPEG XL format.
#[derive(argh::FromArgs, Debug)]
pub struct Conv2JxlArgs {
    /// do not use Unicode characters in output.
    #[argh(switch, short = 'U')]
    pub no_unicode: bool,

    /// process subdirectories recursively
    #[argh(switch, short = 'r')]
    pub recurse: bool,

    /// maximum recursion depth. Default is unlimited.
    /// Only applies if --recurse is set.
    /// A depth of 0 means only the given directories, 1 means their direct subdirectories, etc.
    /// Note that --ignore-recent only applies to files in the direct subdirectories of the given paths, not further down.
    #[argh(option, default = "u64::MAX")]
    pub max_depth: u64,

    /// follow symbolic links when recursing and processing files.
    #[argh(switch)]
    pub follow_links: bool,

    /// minimum recursion depth. Default is 0.
    /// Only applies if --recurse is set.
    /// A depth of 0 means only the given directories, 1 means their direct subdirectories, etc.
    #[argh(option, default = "0")]
    pub min_depth: u64,

    /// overwrite existing files.
    #[argh(switch, short = 'O')]
    pub overwrite: bool,

    /// delete original files after conversion.
    #[argh(switch, short = 'D')]
    pub delete: bool,

    /// truncate source file instead of deleting it after conversion.
    /// This can be useful to avoid issues with hardlinks or if the source and destination are on different filesystems,
    /// or if you just want to know what the original file was without using extra space. This supersedes --delete if both are set.
    #[argh(switch, short = 'T')]
    pub truncate: bool,

    /// conversion quality, from 0 to 100, where 100 is lossless.
    #[argh(option, short = 'q', default = "100")]
    pub quality: u8,

    /// if set, use this quality setting when the conversion is deemed inefficient (i.e., results in a larger file).
    /// This can be used to try to get a smaller file size for images that do not compress well at the normal quality setting.
    /// These often include images that include random noise.
    #[argh(option, short = 'Q')] // TODO!
    pub quality_if_inefficient: Option<u8>,

    /// if a file is inefficiently compressed (i.e., results in a file larger than required by --min-ratio),
    /// and its size is at least this value in bytes, then it will be re-encoded using the
    /// quality specified by --quality-if-inefficient.
    ///
    /// Default is no minimum size, i.e., all files are considered.
    ///
    /// This can be used to avoid re-encoding small files that do not compress well.
    #[argh(option, short = 'I')]
    pub min_inefficient_size: Option<u64>,

    /// effort level, from 0 to 9, where 0 is fastest and 9 is best quality.
    /// 10 exists, but uses too much memory for most systems.
    #[argh(option, short = 'e', default = "9")]
    pub effort: u8,

    /// only consider files that would result in at least this much size reduction, as a ratio, to be successfully converted.
    /// For example, a value of 0.8 means the converted file must be at least 80% the size of the original file (i.e., 20% smaller).
    /// A value of 1.0 (default) means the converted file must be smaller than the original
    /// Files that do not meet this requirement will be reverted to the original file.
    /// Setting this to higher than 1.0 will allow keeping files that are larger than the original, which is probably not what you want, but allowed.
    #[argh(option, short = 'R', default = "1.0")]
    pub min_ratio: f32,

    /// perform a trial run with no changes made, just print what would be done.
    #[argh(switch)]
    pub dry_run: bool,

    /// only convert files larger than this size in bytes. Default is 0 (no minimum).
    #[argh(option, short = 'm', default = "0")]
    pub min_size: u64,

    /// only convert files smaller than this size in bytes. Default is unlimited.
    #[argh(option, short = 'M', default = "u64::MAX")]
    pub max_size: u64,

    /// only convert images with width larger than or equal to this value. Default is 0 (no minimum).
    #[argh(option, default = "0")]
    pub min_width: u32,
    /// only convert images with width smaller than or equal to this value. Default is unlimited 2^32-1.
    #[argh(option, default = "u32::MAX")]
    pub max_width: u32,

    /// only convert images with height larger than or equal to this value. Default is 0 (no minimum).
    #[argh(option, default = "0")]
    pub min_height: u32,
    /// only convert images with height smaller than or equal to this value. Default is unlimited 2^32-1.
    #[argh(option, default = "u32::MAX")]
    pub max_height: u32,

    /// limit the number of files to convert. Default is no limit.
    #[argh(option, short = 'l')]
    pub limit: Option<usize>,

    /// use lossless recompression of JPEG files. Default is true.
    /// Set to false to allow lossy recompression of JPEG files.
    #[argh(option, short = 'j', default = "true")]
    pub lossless_jpeg: bool,

    /// disable JPEG reconstruction from JPEG XL files. Default is false,
    /// which adds a little overhead to the JPEG XL file size, but allows lossless
    /// reconstruction of the original JPEG file.
    ///
    /// Only applies if --lossless-jpeg is set.
    #[argh(switch)]
    pub disable_jpeg_reconstruction: bool,

    /// filter input images as comma-separated list of file extensions.
    /// Defaults to "png", which means only PNG files will be processed.
    /// Use "*" to process all supported files.
    #[argh(option, long = "ext", default = "FileTypes::default()")]
    pub extensions: FileTypes,

    /// removes original file extension from .<ext>.jxl and just uses .jxl
    #[argh(switch, short = 'X')]
    pub no_preserve_extension: bool,

    /// filter input images by regex pattern on full path.
    /// Only files matching the pattern will be processed.
    #[argh(option)]
    pub filter: Option<String>,

    /// exclude input images by regex pattern on full path.
    /// Files matching the pattern will be skipped.
    #[argh(option)]
    pub exclude: Option<String>,

    /// path to error log, for which errors will be appended
    #[argh(option)]
    pub error_log: Option<PathBuf>,

    /// interval (in files processed) to print a summary of progress.
    /// Default is no summary.
    #[argh(option)]
    pub summary_interval: Option<usize>,

    /// number of threads each conversion process should use.
    /// Use -1 to use all available threads, 0 (default) for single-threaded.
    #[argh(option, short = 't', default = "0")]
    pub threads: i32,

    /// number of parallel conversion processes to run.
    /// Use -1 (default) to use all available threads. Minimum is 1 if set.
    #[argh(option, short = 'p', default = "-1")]
    pub parallel: i32,

    /// use progressive encoding for JPEG XL files.
    #[argh(switch)]
    pub progressive: bool,

    /// sort files before conversion.
    /// Valid values are "none", "asc", "desc", "name", "mtime", "ctime", "atime".
    /// "asc" and "desc" sort by file size. Default is "none".
    #[argh(option, short = 's', default = "SortMethod::None")]
    pub sort: SortMethod,

    /// sort direction, either "asc" or "desc". Default is "asc".
    #[argh(option, short = 'd', default = "SortOrder::Asc")]
    pub sort_order: SortOrder,

    /// randomization factor for random sorting, between 0.0 and 1.0,
    /// where 0.0 means no randomization and 1.0 means full randomization
    #[argh(option, default = "0.0")]
    pub randomize: f64,

    /// paths to files or directories to convert.
    #[argh(positional, greedy)]
    pub paths: Vec<PathBuf>,
}

impl Conv2JxlArgs {
    pub fn width(&self) -> RangeInclusive<u32> {
        self.min_width..=self.max_width
    }

    pub fn height(&self) -> RangeInclusive<u32> {
        self.min_height..=self.max_height
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMethod {
    #[default]
    None,
    Size,
    Name,
    /// Modification time
    MTime,
    /// Creation time
    CTime,
    /// Accessed time
    ATime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortOrder {
    #[default]
    Asc,
    Desc,
}

macro_rules! decl_filetypes {
    ($($variant:ident),* $(,)?) => {
        #[allow(clippy::upper_case_acronyms)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum FileType {
            $($variant),*
        }

        #[allow(non_snake_case)]
        #[derive(Debug, Default, Clone, PartialEq, Eq)]
        pub struct PerFileType<T> {
            $(pub $variant: T),*
        }

        impl FileType {
            pub fn all() -> &'static [FileType] {
                &[$(FileType::$variant),*]
            }
        }

        impl<T> PerFileType<T> {
            #[inline]
            pub fn get(&self, ftype: FileType) -> &T {
                match ftype {
                    $(FileType::$variant => &self.$variant),*
                }
            }

            #[inline]
            pub fn get_mut(&mut self, ftype: FileType) -> &mut T {
                match ftype {
                    $(FileType::$variant => &mut self.$variant),*
                }
            }

            #[inline]
            pub fn iter(&self) -> impl Iterator<Item = (FileType, &T)> {
                FileType::all().iter().map(move |&ftype| (ftype, self.get(ftype)))
            }

            pub fn map<F, U>(&self, f: F) -> PerFileType<U>
            where
                F: Fn(&T) -> U,
            {
                PerFileType {
                    $($variant: f(&self.$variant)),*
                }
            }
        }
    };
}

decl_filetypes!(JXL, PPM, PNM, PFM, PAM, PGX, PNG, APNG, GIF, JPEG, TIFF, TGA, QOI, BMP);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTypes(pub HashSet<FileType, foldhash::fast::FixedState>);

impl Default for FileTypes {
    fn default() -> Self {
        let mut set = HashSet::with_capacity_and_hasher(1, foldhash::fast::FixedState::default());
        set.insert(FileType::PNG); // default to only PNG files
        FileTypes(set)
    }
}

impl Deref for FileTypes {
    type Target = HashSet<FileType, foldhash::fast::FixedState>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct InvalidSortMethod;

#[derive(Debug, Clone, Copy)]
pub struct InvalidSortDirection;

#[derive(Debug, Clone, Copy)]
pub struct InvalidFileType;

impl Display for InvalidSortMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid sort method")
    }
}

impl Display for InvalidSortDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid sort direction")
    }
}

impl Display for InvalidFileType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid file type")
    }
}

impl Error for InvalidSortMethod {}
impl Error for InvalidSortDirection {}
impl Error for InvalidFileType {}

impl FromStr for SortMethod {
    type Err = InvalidSortMethod;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const PATTERNS: [(&str, SortMethod); 6] = [
            ("none", SortMethod::None),
            ("size", SortMethod::Size),
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

impl FromStr for SortOrder {
    type Err = InvalidSortDirection;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const PATTERNS: [(&str, SortOrder); 8] = [
            ("asc", SortOrder::Asc),
            ("desc", SortOrder::Desc),
            ("ascending", SortOrder::Asc),
            ("descending", SortOrder::Desc),
            ("up", SortOrder::Asc),
            ("down", SortOrder::Desc),
            ("increasing", SortOrder::Asc),
            ("decreasing", SortOrder::Desc),
        ];

        for (pattern, direction) in PATTERNS {
            if s.eq_ignore_ascii_case(pattern) {
                return Ok(direction);
            }
        }

        Err(InvalidSortDirection)
    }
}

impl FromStr for FileType {
    type Err = InvalidFileType;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const PATTERNS: [(&str, FileType); 15] = [
            ("jxl", FileType::JXL),
            ("ppm", FileType::PPM),
            ("pnm", FileType::PNM),
            ("pfm", FileType::PFM),
            ("pam", FileType::PAM),
            ("pgx", FileType::PGX),
            ("png", FileType::PNG),
            ("apng", FileType::APNG),
            ("gif", FileType::GIF),
            ("jpeg", FileType::JPEG),
            ("jpg", FileType::JPEG),
            ("tiff", FileType::TIFF),
            ("tif", FileType::TIFF),
            ("tga", FileType::TGA),
            ("qoi", FileType::QOI),
        ];

        for (pattern, ftype) in PATTERNS {
            if s.eq_ignore_ascii_case(pattern) {
                return Ok(ftype);
            }
        }

        Err(InvalidFileType)
    }
}

impl Display for FileType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            FileType::JXL => "jxl",
            FileType::PPM => "ppm",
            FileType::PNM => "pnm",
            FileType::PFM => "pfm",
            FileType::PAM => "pam",
            FileType::PGX => "pgx",
            FileType::PNG => "png",
            FileType::APNG => "apng",
            FileType::GIF => "gif",
            FileType::JPEG => "jpeg",
            FileType::TIFF => "tiff",
            FileType::TGA => "tga",
            FileType::QOI => "qoi",
            FileType::BMP => "bmp",
        })
    }
}

impl FileType {
    /// Returns true if the file type needs conversion via the `image` crate,
    /// as these aren't natively supported by `cjxl`.
    pub const fn needs_conversion(self) -> bool {
        matches!(self, FileType::TIFF | FileType::TGA | FileType::QOI | FileType::BMP)
    }
}

impl FromStr for FileTypes {
    type Err = InvalidFileType;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut set = HashSet::with_capacity_and_hasher(4, foldhash::fast::FixedState::default());
        // comma-separated list
        for mut part in s.split(',') {
            part = part.trim();

            if part == "*" {
                let had_jxl = set.contains(&FileType::JXL);

                set.extend(FileType::all());

                if !had_jxl {
                    // if * was specified, don't include JXL itself unless it was explicitly requested
                    set.remove(&FileType::JXL);
                }
            }

            set.insert(part.parse::<FileType>()?);
        }

        Ok(FileTypes(set))
    }
}
