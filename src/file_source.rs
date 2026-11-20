use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::database::{Cell, IMPORT_MEMORY_LIMIT, check_import_cancelled};
use crate::odbc::{SourceColumn, SourceData};
use crate::sql_tools::delimited::csv_records;

mod access;
mod csv;
mod excel;

const MAX_SOURCE_FILES: usize = 16_384;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileSourceFormat {
    #[default]
    Csv,
    Excel,
    Access,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileSourceEncoding {
    #[default]
    Utf8,
    Gbk,
    Gb18030,
    Big5,
    #[serde(alias = "utf16le")]
    Utf16Le,
    #[serde(alias = "utf16be")]
    Utf16Be,
}

impl FileSourceEncoding {
    pub const ALL: [Self; 6] = [
        Self::Utf8,
        Self::Gbk,
        Self::Gb18030,
        Self::Big5,
        Self::Utf16Le,
        Self::Utf16Be,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Gbk => "GBK",
            Self::Gb18030 => "GB18030",
            Self::Big5 => "Big5",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileSource {
    pub format: FileSourceFormat,
    pub path: PathBuf,
    pub delimiter: String,
    pub encoding: FileSourceEncoding,
    pub headers: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_password_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_link_folder: Option<PathBuf>,
    #[serde(skip_serializing_if = "is_false")]
    pub excel_named_ranges: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Default for FileSource {
    fn default() -> Self {
        Self {
            format: FileSourceFormat::Csv,
            path: PathBuf::new(),
            delimiter: ",".into(),
            encoding: FileSourceEncoding::Utf8,
            headers: true,
            access_password_env: None,
            access_link_folder: None,
            excel_named_ranges: false,
        }
    }
}

impl FileSource {
    pub const fn supports_metadata(&self) -> bool {
        matches!(self.format, FileSourceFormat::Access)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.path.as_os_str().is_empty() {
            return Err("请选择源文件或 CSV 文件夹。".into());
        }
        if let Some(name) = &self.access_password_env {
            if self.format != FileSourceFormat::Access {
                return Err("只有 Access 文件可设置数据库密码变量名。".into());
            }
            if name.trim().is_empty() || name.contains('=') || name.chars().any(char::is_control) {
                return Err("请填写有效的 Access 密码环境变量名。".into());
            }
        }
        if let Some(folder) = &self.access_link_folder {
            if self.format != FileSourceFormat::Access {
                return Err("链接文件夹只用于 Access 数据库。".into());
            }
            if folder.as_os_str().is_empty() {
                return Err("请选择 Access 链接文件所在的文件夹。".into());
            }
        }
        match self.format {
            FileSourceFormat::Csv => csv_records("", &self.delimiter).map(drop),
            FileSourceFormat::Excel => excel::format(&self.path, self.excel_named_ranges).map(drop),
            FileSourceFormat::Access => access::validate(&self.path),
        }
    }

    fn with_access_password<T>(
        &self,
        explicit: Option<&str>,
        operation: impl FnOnce(Option<&str>) -> Result<T, String>,
    ) -> Result<T, String> {
        if let Some(password) = explicit {
            return operation(Some(password));
        }
        let Some(name) = &self.access_password_env else {
            return operation(None);
        };
        let password = Zeroizing::new(
            std::env::var(name).map_err(|_| format!("无法读取 Access 密码环境变量「{name}」。"))?,
        );
        operation(Some(password.as_str()))
    }
}

pub fn discover(source: &FileSource) -> Result<Vec<String>, String> {
    discover_file(source, None)
}

pub fn discover_with_access_password(
    source: &FileSource,
    password: &str,
) -> Result<Vec<String>, String> {
    discover_file(source, Some(password))
}

fn discover_file(
    source: &FileSource,
    access_password: Option<&str>,
) -> Result<Vec<String>, String> {
    source.validate()?;
    if access_password.is_some() && source.format != FileSourceFormat::Access {
        return Err("文件密码只用于 Access 数据库。".into());
    }
    match source.format {
        FileSourceFormat::Csv => Ok(csv_files(&source.path, None)?.into_keys().collect()),
        FileSourceFormat::Excel => excel::discover(&source.path, source.excel_named_ranges),
        FileSourceFormat::Access => source.with_access_password(access_password, |password| {
            access::discover(source, password)
        }),
    }
}

pub fn read(source: &FileSource, table: &str, columns_only: bool) -> Result<SourceData, String> {
    read_with_metadata(source, table, columns_only, false, false)
}

pub fn read_with_metadata(
    source: &FileSource,
    table: &str,
    columns_only: bool,
    copy_indexes: bool,
    copy_foreign_keys: bool,
) -> Result<SourceData, String> {
    read_file(
        source,
        table,
        columns_only,
        copy_indexes,
        copy_foreign_keys,
        None,
        None,
    )
}

pub fn read_with_access_password(
    source: &FileSource,
    table: &str,
    columns_only: bool,
    copy_indexes: bool,
    copy_foreign_keys: bool,
    password: &str,
) -> Result<SourceData, String> {
    read_file(
        source,
        table,
        columns_only,
        copy_indexes,
        copy_foreign_keys,
        Some(password),
        None,
    )
}

pub(crate) fn read_with_cancel(
    source: &FileSource,
    table: &str,
    columns_only: bool,
    cancel: &AtomicBool,
) -> Result<SourceData, String> {
    read_with_metadata_and_cancel(source, table, columns_only, false, false, cancel)
}

pub(crate) fn read_with_metadata_and_cancel(
    source: &FileSource,
    table: &str,
    columns_only: bool,
    copy_indexes: bool,
    copy_foreign_keys: bool,
    cancel: &AtomicBool,
) -> Result<SourceData, String> {
    read_file(
        source,
        table,
        columns_only,
        copy_indexes,
        copy_foreign_keys,
        None,
        Some(cancel),
    )
}

fn read_file(
    source: &FileSource,
    table: &str,
    columns_only: bool,
    copy_indexes: bool,
    copy_foreign_keys: bool,
    access_password: Option<&str>,
    cancel: Option<&AtomicBool>,
) -> Result<SourceData, String> {
    check_import_cancelled(cancel)?;
    source.validate()?;
    if access_password.is_some() && source.format != FileSourceFormat::Access {
        return Err("文件密码只用于 Access 数据库。".into());
    }
    if (copy_indexes || copy_foreign_keys) && !source.supports_metadata() {
        return Err("文件来源中，只有 Access 数据库支持复制索引和外键。".into());
    }
    match source.format {
        FileSourceFormat::Csv => {
            let files = csv_files(&source.path, cancel)?;
            let path = files.get(table).ok_or("所选 CSV 表已不在源目录中。")?;
            csv::read(path, source, columns_only, cancel)
        }
        FileSourceFormat::Excel => excel::read(
            &source.path,
            table,
            source.headers,
            columns_only,
            source.excel_named_ranges,
            cancel,
        ),
        FileSourceFormat::Access => source.with_access_password(access_password, |password| {
            access::read(
                source,
                table,
                columns_only,
                copy_indexes || copy_foreign_keys,
                copy_foreign_keys,
                password,
                cancel,
            )
        }),
    }
}

fn csv_files(
    path: &Path,
    cancel: Option<&AtomicBool>,
) -> Result<BTreeMap<String, PathBuf>, String> {
    let root = fs::canonicalize(path).map_err(file_error)?;
    let metadata = fs::metadata(&root).map_err(file_error)?;
    if metadata.is_file() {
        return Ok(BTreeMap::from([(file_stem(path)?, root)]));
    }
    if !metadata.is_dir() {
        return Err("请选择普通 CSV 文件或文件夹。".into());
    }
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(&root).map_err(file_error)? {
        check_import_cancelled(cancel)?;
        let entry = entry.map_err(file_error)?;
        let path = entry.path();
        if !path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("csv"))
            || !entry.file_type().map_err(file_error)?.is_file()
        {
            continue;
        }
        let name = file_stem(&path)?;
        let resolved = fs::canonicalize(path).map_err(file_error)?;
        if resolved.parent() != Some(root.as_path()) {
            return Err(format!("CSV 表「{name}」不在所选文件夹内。"));
        }
        if files.insert(name.clone(), resolved).is_some() {
            return Err(format!("所选文件夹里有多个 CSV 文件对应「{name}」。"));
        }
        if files.len() > MAX_SOURCE_FILES {
            return Err(format!("CSV 文件夹最多支持 {MAX_SOURCE_FILES} 张表。"));
        }
    }
    Ok(files)
}

fn file_stem(path: &Path) -> Result<String, String> {
    path.file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "CSV 文件名须可用作表名。".into())
}

fn file_error(error: io::Error) -> String {
    format!("无法读取源文件。{error}")
}

fn read_bytes(path: &Path, cancel: Option<&AtomicBool>) -> Result<Vec<u8>, String> {
    let mut file = File::open(path).map_err(file_error)?;
    let metadata = file.metadata().map_err(file_error)?;
    if !metadata.is_file() {
        return Err("请选择普通文件作为数据源。".into());
    }
    if metadata.len() > IMPORT_MEMORY_LIMIT as u64 {
        return Err("源文件大小超过 32 MiB。".into());
    }
    let mut contents = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        check_import_cancelled(cancel)?;
        let read = match file.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result.map_err(file_error)?,
        };
        if read == 0 {
            return Ok(contents);
        }
        if contents.len() + read > IMPORT_MEMORY_LIMIT {
            return Err("源文件大小超过 32 MiB。".into());
        }
        contents.extend_from_slice(&buffer[..read]);
    }
}

fn column_names(headers: Vec<String>) -> Result<Vec<String>, String> {
    let mut used = HashSet::new();
    for header in headers.iter().filter(|header| !header.trim().is_empty()) {
        if !used.insert(header.to_lowercase()) {
            return Err(format!("文件中有重复的列名「{header}」，请修改表头。"));
        }
    }
    Ok(headers
        .into_iter()
        .enumerate()
        .map(|(index, header)| {
            if !header.trim().is_empty() {
                return header;
            }
            let base = format!("列{}", index + 1);
            let mut candidate = base.clone();
            let mut suffix = 2;
            while !used.insert(candidate.clone()) {
                candidate = format!("{base}_{suffix}");
                suffix += 1;
            }
            candidate
        })
        .collect())
}

fn imported(columns: Vec<SourceColumn>, rows: Vec<Vec<Cell>>) -> SourceData {
    SourceData {
        columns,
        rows,
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        source_table: None,
    }
}
