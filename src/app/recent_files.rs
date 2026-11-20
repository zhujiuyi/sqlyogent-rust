use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::state::{profile_path, write_atomic};

const MAX_FILES: usize = 12;
const STORE_LIMIT: u64 = 2 * 1024 * 1024;
const KINDS: [Kind; 2] = [Kind::Sql, Kind::SchemaDesign];

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Sql,
    SchemaDesign,
}

#[derive(Default)]
pub(super) struct RecentFiles {
    current: FileLists,
    pending: FileLists,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct FileLists {
    sql: Vec<PathBuf>,
    schema_designs: Vec<PathBuf>,
}

impl RecentFiles {
    pub(super) fn load() -> Result<Self, String> {
        let current = FileLists::read(&store_path()?)?;
        Ok(Self {
            current,
            pending: FileLists::default(),
        })
    }

    pub(super) fn files(&self, kind: Kind) -> &[PathBuf] {
        self.current.files(kind)
    }

    pub(super) fn record(&mut self, kind: Kind, path: &Path) -> Result<(), String> {
        self.remember(kind, path)?;
        self.persist(&store_path()?)
    }

    fn remember(&mut self, kind: Kind, path: &Path) -> Result<(), String> {
        if path.as_os_str().is_empty() {
            return Err("最近文件路径为空。".into());
        }
        let path = fs::canonicalize(path).map_err(|error| format!("无法解析文件路径：{error}"))?;
        if !path.is_file() {
            return Err("最近文件必须是普通文件。".into());
        }
        self.current.touch(kind, path.clone());
        self.pending.touch(kind, path);
        Ok(())
    }

    fn persist(&mut self, store: &Path) -> Result<(), String> {
        let directory = store.parent().ok_or("无法找到最近文件列表所在目录。")?;
        fs::create_dir_all(directory).map_err(|error| error.to_string())?;
        let lock = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(store.with_extension("lock"))
            .map_err(|error| error.to_string())?;
        lock.lock().map_err(|error| error.to_string())?;

        let mut merged = FileLists::read(store)?;
        for kind in KINDS {
            for path in self.pending.files(kind).iter().rev() {
                merged.touch(kind, path.clone());
            }
        }
        let bytes = serde_json::to_vec_pretty(&merged).map_err(|error| error.to_string())?;
        if bytes.len() as u64 > STORE_LIMIT {
            return Err("最近文件列表超过保存上限。".into());
        }
        write_atomic(store, &bytes).map_err(|error| error.to_string())?;
        self.current = merged;
        self.pending = FileLists::default();
        Ok(())
    }
}

impl FileLists {
    fn read(store: &Path) -> Result<Self, String> {
        let file = match fs::File::open(store) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error.to_string()),
        };
        let mut bytes = Vec::new();
        file.take(STORE_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() as u64 > STORE_LIMIT {
            return Err("最近文件列表超过读取上限。".into());
        }
        let mut lists: Self = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        for kind in KINDS {
            let paths = std::mem::take(lists.files_mut(kind));
            for path in paths.into_iter().rev().filter(|path| path.is_absolute()) {
                lists.touch(kind, path);
            }
        }
        Ok(lists)
    }

    fn files(&self, kind: Kind) -> &[PathBuf] {
        match kind {
            Kind::Sql => &self.sql,
            Kind::SchemaDesign => &self.schema_designs,
        }
    }

    fn files_mut(&mut self, kind: Kind) -> &mut Vec<PathBuf> {
        match kind {
            Kind::Sql => &mut self.sql,
            Kind::SchemaDesign => &mut self.schema_designs,
        }
    }

    fn touch(&mut self, kind: Kind, path: PathBuf) {
        let files = self.files_mut(kind);
        files.retain(|entry| !same_path(entry, &path));
        files.insert(0, path);
        files.truncate(MAX_FILES);
    }
}

fn store_path() -> Result<PathBuf, String> {
    profile_path()
        .map(|path| path.with_file_name("recent-files.json"))
        .ok_or_else(|| "无法找到本机的配置目录。".into())
}

fn same_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    return display_path(left).to_lowercase() == display_path(right).to_lowercase();
    #[cfg(not(windows))]
    return left == right;
}

pub(super) fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    #[cfg(windows)]
    {
        if let Some(server_path) = text.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{server_path}");
        }
        if let Some(drive_path) = text.strip_prefix(r"\\?\")
            && drive_path.as_bytes().get(1) == Some(&b':')
        {
            return drive_path.to_owned();
        }
    }
    text.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_files_merge_and_reload_per_kind() {
        let fixture =
            tempfile::tempdir_in(std::env::current_exe().unwrap().parent().unwrap()).unwrap();
        let store = fixture.path().join("recent-files.json");
        let first = fixture.path().join("脚本.sql");
        let design = fixture.path().join("旧布局.xml");
        fs::write(&first, "SELECT 1;").unwrap();
        fs::write(&design, "<schema/>").unwrap();

        let mut sql = RecentFiles::default();
        let mut layouts = RecentFiles::default();
        sql.remember(Kind::Sql, &first).unwrap();
        layouts.remember(Kind::SchemaDesign, &design).unwrap();
        layouts.persist(&store).unwrap();

        let mut later_scripts = Vec::new();
        for index in 0..MAX_FILES {
            let path = fixture.path().join(format!("script-{index}.sql"));
            fs::write(&path, "").unwrap();
            sql.remember(Kind::Sql, &path).unwrap();
            later_scripts.push(path.canonicalize().unwrap());
        }
        sql.remember(Kind::Sql, &first).unwrap();
        sql.remember(Kind::Sql, &first).unwrap();
        sql.persist(&store).unwrap();

        let loaded = FileLists::read(&store).unwrap();
        let script_files = loaded.files(Kind::Sql);
        assert_eq!(MAX_FILES, script_files.len());
        assert_eq!(first.canonicalize().unwrap(), script_files[0]);
        assert_eq!(later_scripts.last().unwrap(), &script_files[1]);
        assert_eq!(
            [design.canonicalize().unwrap()],
            loaded.files(Kind::SchemaDesign)
        );
        assert_eq!("SELECT 1;", fs::read_to_string(first).unwrap());
        assert_eq!("<schema/>", fs::read_to_string(design).unwrap());
    }
}
