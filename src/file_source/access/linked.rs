use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use jetdb::data::{TableRows, Value};
use jetdb::format::{CATALOG_PAGE, ObjectType};
use jetdb::read_table_def;

use crate::database::{MAX_IMPORT_ROWS, check_import_cancelled};

use super::{Access, access_error, next_row, user_table};

pub(super) struct Link {
    pub(super) alias: String,
    pub(super) filename: String,
    pub(super) table: String,
}

pub(super) fn catalog(
    access: &mut Access,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<Link>, String> {
    let mut expected = access
        .catalog
        .iter()
        .filter(|entry| entry.object_type == ObjectType::LinkedTable && user_table(entry))
        .map(|entry| entry.name.as_str())
        .collect::<HashSet<_>>();
    if expected.is_empty() {
        return Ok(Vec::new());
    }
    let table =
        read_table_def(&mut access.reader, "MSysObjects", CATALOG_PAGE).map_err(access_error)?;
    let positions = ["Type", "Name", "Database", "ForeignName"].map(|name| {
        table
            .columns
            .iter()
            .position(|column| column.name.eq_ignore_ascii_case(name))
    });
    let [Some(kind), Some(name), Some(database), Some(foreign_name)] = positions else {
        return Err("Access 系统目录缺少链接文件或后端表信息。".into());
    };
    let calculated = HashMap::new();
    let mut scanner =
        TableRows::new(&table, &calculated).with_columns(&[kind, name, database, foreign_name]);
    let mut links = Vec::new();
    for number in 1..=MAX_IMPORT_ROWS + 1 {
        check_import_cancelled(cancel)?;
        let Some(row) = next_row(&mut access.reader, &mut scanner, &table.name, number)? else {
            break;
        };
        if number > MAX_IMPORT_ROWS {
            return Err("Access 链接表系统目录超过记录数上限。".into());
        }
        let local_link = match row.values.get(kind) {
            Some(Value::Int(kind)) => i32::from(*kind) == ObjectType::LinkedTable as i32,
            Some(Value::Long(kind)) => *kind == ObjectType::LinkedTable as i32,
            _ => false,
        };
        if !local_link {
            continue;
        }
        let Some(Value::Text(alias)) = row.values.get(name) else {
            continue;
        };
        if !expected.remove(alias.as_str()) {
            continue;
        }
        let (Some(Value::Text(path)), Some(Value::Text(table))) =
            (row.values.get(database), row.values.get(foreign_name))
        else {
            continue;
        };
        let Some(filename) = recorded_filename(path) else {
            continue;
        };
        if table.trim().is_empty() || table.contains('\0') {
            continue;
        }
        links.push(Link {
            alias: alias.clone(),
            filename: filename.to_owned(),
            table: table.clone(),
        });
    }
    if !expected.is_empty() {
        return Err("无法核对 Access 系统目录中的全部链接表。".into());
    }
    Ok(links)
}

fn recorded_filename(path: &str) -> Option<&str> {
    let head = path.trim_start();
    if matches!(head.as_bytes(), [b'/' | b'\\', b'/' | b'\\', ..])
        || head.contains("://")
        || path.chars().any(char::is_control)
        || head.split(';').any(connection_part)
    {
        return None;
    }
    let basename = path.rsplit(['/', '\\']).next()?;
    if basename.contains(':') || super::validate(Path::new(basename)).is_err() {
        return None;
    }
    Some(basename)
}

fn connection_part(part: &str) -> bool {
    part.trim().eq_ignore_ascii_case("ODBC")
        || part.split_once('=').is_some_and(|(name, _)| {
            matches!(
                name.trim().to_ascii_uppercase().as_str(),
                "DATABASE"
                    | "DBQ"
                    | "DSN"
                    | "DRIVER"
                    | "FILEDSN"
                    | "PROVIDER"
                    | "DATA SOURCE"
                    | "SERVER"
                    | "UID"
                    | "USER ID"
                    | "PWD"
                    | "PASSWORD"
            )
        })
}

pub(super) fn open_folder(path: &Path) -> Result<PathBuf, String> {
    let error = |error| format!("无法访问选定的 Access 链接文件夹。{error}");
    let folder = fs::canonicalize(path).map_err(error)?;
    if !fs::metadata(&folder).map_err(error)?.is_dir() {
        return Err("请为 Access 链接表选择文件夹。".into());
    }
    Ok(folder)
}

pub(super) fn resolve(
    folder: &Path,
    link: &Link,
    cancel: Option<&AtomicBool>,
) -> Result<PathBuf, String> {
    let folder = open_folder(folder)?;
    let error = |error| {
        format!(
            "无法访问 Access 链接表「{}」的后端文件。{error}",
            link.alias
        )
    };
    for entry in fs::read_dir(&folder).map_err(error)? {
        check_import_cancelled(cancel)?;
        let entry = entry.map_err(error)?;
        // Use the directory entry itself rather than interpreting the stored path.
        // A symbolic link is not a regular direct child.
        if entry.file_name() != OsStr::new(&link.filename) {
            continue;
        }
        if !entry.file_type().map_err(error)?.is_file() {
            break;
        }
        let file = fs::canonicalize(entry.path()).map_err(error)?;
        if file.parent() == Some(folder.as_path()) {
            return Ok(file);
        }
        break;
    }
    Err(format!(
        "选定的链接文件夹中缺少链接表「{}」需要的本地文件「{}」。",
        link.alias, link.filename
    ))
}

pub(super) fn parent_alias<'a>(
    links: &'a [Link],
    selected: &Link,
    referenced: &str,
    key: &str,
) -> Result<&'a str, String> {
    let name = referenced.to_lowercase();
    let mut matches = links.iter().filter(|candidate| {
        candidate.filename == selected.filename && candidate.table.to_lowercase() == name
    });
    let Some(found) = matches.next() else {
        return Err(format!(
            "Access 链接表「{}」的外键「{key}」引用后端表「{referenced}」，该表在前端缺少对应的链接表名。",
            selected.alias,
        ));
    };
    if matches.next().is_some() {
        return Err(format!(
            "Access 链接表「{}」的外键「{key}」引用后端表「{referenced}」，该表在前端有多个链接表名。",
            selected.alias,
        ));
    }
    Ok(&found.alias)
}
