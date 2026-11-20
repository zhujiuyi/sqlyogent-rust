use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use tempfile::{NamedTempFile, TempDir, TempPath};

use crate::restore::{
    RESTORE_MANIFEST_LIMIT, RESTORE_MANIFEST_NAME, directory_sources, ordered_part_name,
};
use crate::sql_encoding::{SqlDumpCharset, encode_sql};

use super::{
    BackupDirectoryMode, DumpSettings, ExistingOutput, TransferSummary, atomic_output,
    lock_output_directory, output_directory, output_error, validate_output_filename,
};

pub(super) enum OutputMark {
    File(u64),
    Parts(usize),
}

pub(super) trait DumpOutput: Write {
    fn checkpoint(&mut self) -> io::Result<OutputMark>;

    fn rewind(&mut self, mark: OutputMark) -> io::Result<()>;

    fn per_object(&self) -> bool {
        false
    }

    fn open_part(&mut self, _name: &str) -> io::Result<()> {
        Ok(())
    }

    fn close_part(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl DumpOutput for BufWriter<&mut File> {
    fn checkpoint(&mut self) -> io::Result<OutputMark> {
        self.stream_position().map(OutputMark::File)
    }

    fn rewind(&mut self, mark: OutputMark) -> io::Result<()> {
        let OutputMark::File(offset) = mark else {
            return Err(io::Error::other("SQL 文件的备份检查点无效。"));
        };
        self.seek(SeekFrom::Start(offset))?;
        self.get_mut().set_len(offset)
    }
}

struct ObjectFiles<'a> {
    directory: &'a Path,
    timestamp_prefix: String,
    generation_suffix: &'a str,
    current: Option<BufWriter<File>>,
    files: Vec<String>,
}

impl ObjectFiles<'_> {
    fn current(&mut self) -> io::Result<&mut BufWriter<File>> {
        self.current
            .as_mut()
            .ok_or_else(|| io::Error::other("分文件备份没有打开的 SQL 文件。"))
    }

    fn finish(self, charset: SqlDumpCharset) -> io::Result<(Vec<String>, u64)> {
        if self.current.is_some() {
            return Err(io::Error::other("分文件备份仍有打开的 SQL 文件。"));
        }
        let mut bytes = 0;
        for name in &self.files {
            let path = self.directory.join(name);
            let file = if charset == SqlDumpCharset::Utf8Mb4 {
                File::options().write(true).open(&path)?
            } else {
                let mut encoded = NamedTempFile::new_in(self.directory)?;
                encode_sql(
                    charset,
                    BufReader::new(File::open(&path)?),
                    encoded.as_file_mut(),
                )?;
                encoded.persist(&path).map_err(|error| error.error)?
            };
            file.sync_all()?;
            bytes += file.metadata()?.len();
        }
        Ok((self.files, bytes))
    }
}

impl Write for ObjectFiles<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.current()?.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.current()?.flush()
    }
}

impl DumpOutput for ObjectFiles<'_> {
    fn checkpoint(&mut self) -> io::Result<OutputMark> {
        if self.current.is_some() {
            return Err(io::Error::other("分文件备份仍有打开的 SQL 文件。"));
        }
        Ok(OutputMark::Parts(self.files.len()))
    }

    fn rewind(&mut self, mark: OutputMark) -> io::Result<()> {
        let OutputMark::Parts(count) = mark else {
            return Err(io::Error::other("分文件备份的检查点无效。"));
        };
        if let Some(writer) = self.current.take() {
            let (file, _) = writer.into_parts();
            drop(file);
        }
        for name in self.files.drain(count..) {
            fs::remove_file(self.directory.join(name))?;
        }
        Ok(())
    }

    fn per_object(&self) -> bool {
        true
    }

    fn open_part(&mut self, name: &str) -> io::Result<()> {
        let label: String = name
            .chars()
            .take(40)
            .map(|ch| {
                if ch.is_alphanumeric() || matches!(ch, '-' | '_') {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        let name = format!(
            "{}{:08}-{label}{}.sql",
            self.timestamp_prefix,
            self.files.len() + 1,
            self.generation_suffix
        );
        let path = self.directory.join(&name);
        self.current = Some(BufWriter::new(
            File::options().write(true).create_new(true).open(path)?,
        ));
        self.files.push(name);
        Ok(())
    }

    fn close_part(&mut self) -> io::Result<()> {
        let mut writer = self
            .current
            .take()
            .ok_or_else(|| io::Error::other("分文件备份没有打开的 SQL 文件。"))?;
        writer.flush()
    }
}

pub(crate) fn validate_backup_directory(
    path: &Path,
    mode: BackupDirectoryMode,
    existing: ExistingOutput,
) -> Result<bool, String> {
    if path.file_name().is_none() {
        return Err("请指定有效的备份目录。".into());
    }
    validate_output_filename(path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if mode == BackupDirectoryMode::Selected && metadata.is_dir() => {
            if existing == ExistingOutput::Reject
                && fs::read_dir(path)
                    .map_err(output_error)?
                    .next()
                    .transpose()
                    .map_err(output_error)?
                    .is_some()
            {
                return Err(
                    "所选备份目录中已有内容。请选择空目录、路径尚不存在的目录或时间戳子目录。"
                        .into(),
                );
            }
            Ok(true)
        }
        Ok(_) => Err("本次备份的输出路径已经存在。".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(output_error(error)),
    }
}

pub(super) fn write_dump_output(
    path: &Path,
    settings: &DumpSettings,
    write: impl FnOnce(&mut dyn DumpOutput) -> Result<TransferSummary, String>,
) -> Result<TransferSummary, String> {
    let existing = ExistingOutput::from_flags(settings.overwrite, settings.append)?;
    if !settings.file_per_object {
        let charset = settings.file_charset;
        return atomic_output(path, existing, |out| {
            if charset == SqlDumpCharset::Utf8Mb4 {
                return write(out);
            }
            let mut scratch =
                tempfile::tempfile_in(output_directory(path)).map_err(output_error)?;
            let summary = {
                let mut writer = BufWriter::new(&mut scratch);
                let result = write(&mut writer)?;
                writer.flush().map_err(output_error)?;
                result
            };
            scratch.rewind().map_err(output_error)?;
            encode_sql(charset, BufReader::new(&mut scratch), out).map_err(output_error)?;
            Ok(summary)
        });
    }
    let mode = settings.directory_mode;
    if existing != ExistingOutput::Reject && mode != BackupDirectoryMode::Selected {
        return Err("分文件备份须使用所选目录才能覆盖或追加。".into());
    }
    let parent = if mode.is_per_run() {
        path
    } else {
        validate_backup_directory(path, mode, existing)?;
        output_directory(path)
    };
    fs::create_dir_all(parent).map_err(output_error)?;
    let staging = tempfile::Builder::new()
        .prefix(".harbor-backup-")
        .tempdir_in(parent)
        .map_err(output_error)?;
    let mut out = ObjectFiles {
        directory: staging.path(),
        timestamp_prefix: split_timestamp_prefix(settings.split_filename_timestamp),
        generation_suffix: split_generation_suffix(&staging, existing)?,
        current: None,
        files: Vec::new(),
    };
    let mut summary = write(&mut out)?;
    // Source locks and snapshots are released when the writer returns.
    let (files, bytes) = out.finish(settings.file_charset).map_err(output_error)?;
    summary.bytes = bytes;

    let output = if mode.is_per_run() {
        let name = staging
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix('.'))
            .ok_or("无法确定分文件备份目录名称。")?;
        path.join(name)
    } else {
        path.to_owned()
    };
    summary.cleanup_warning =
        publish_split_directory(staging, &output, mode, existing, &files, || Ok(()))?;
    summary.output_directory = Some(output);
    Ok(summary)
}

pub(crate) fn backup_timestamp(timestamp: u64) -> String {
    chrono::DateTime::from_timestamp_millis(timestamp as i64)
        .unwrap_or_default()
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H-%M-%S")
        .to_string()
}

pub(crate) fn split_timestamp_prefix(timestamp: Option<u64>) -> String {
    timestamp.map_or_else(String::new, |time| format!("{}_", backup_timestamp(time)))
}

pub(crate) fn split_generation_suffix(
    staging: &TempDir,
    existing: ExistingOutput,
) -> Result<&str, String> {
    if existing == ExistingOutput::Reject {
        return Ok("");
    }
    staging
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(".harbor-backup"))
        .ok_or_else(|| "无法确定本次分文件备份的名称。".into())
}

pub(crate) fn write_restore_order(directory: &Path, files: &[String]) -> Result<(), String> {
    let manifest = NamedTempFile::new_in(directory).map_err(output_error)?;
    record_restore_order(manifest.as_file(), files)?;
    manifest
        .persist_noclobber(directory.join(RESTORE_MANIFEST_NAME))
        .map_err(|error| output_error(error.error))?;
    Ok(())
}

fn record_restore_order(mut manifest: &File, files: &[String]) -> Result<(), String> {
    if files.is_empty() {
        return Err("分文件备份没有可还原的 SQL 文件。".into());
    }
    let version = if files
        .iter()
        .enumerate()
        .all(|(index, file)| ordered_part_name(index, file))
    {
        1
    } else {
        2
    };
    serde_json::to_writer_pretty(
        &mut manifest,
        &serde_json::json!({"version":version,"files":files}),
    )
    .map_err(|error| format!("无法记录恢复顺序：{error}"))?;
    manifest.write_all(b"\n").map_err(output_error)?;
    if manifest.metadata().map_err(output_error)?.len() > RESTORE_MANIFEST_LIMIT {
        return Err("备份对象过多，还原顺序文件超过大小上限。".into());
    }
    manifest.sync_all().map_err(output_error)
}

pub(crate) fn publish_split_directory(
    staging: TempDir,
    output: &Path,
    mode: BackupDirectoryMode,
    existing: ExistingOutput,
    files: &[String],
    check_cancelled: impl Fn() -> Result<(), String>,
) -> Result<Option<String>, String> {
    let _lock = (mode == BackupDirectoryMode::Selected)
        .then(|| lock_output_directory(output_directory(output)))
        .transpose()
        .map_err(output_error)?;
    let directory_exists = validate_backup_directory(output, mode, existing)?;
    check_cancelled()?;
    if directory_exists && existing != ExistingOutput::Reject {
        return merge_split_directory(staging.path(), output, existing, files, &check_cancelled);
    }
    write_restore_order(staging.path(), files)?;
    check_cancelled()?;
    rename_split_directory(staging.path(), output, directory_exists)?;
    let _ = staging.keep();
    Ok(None)
}

fn merge_split_directory(
    staging: &Path,
    output: &Path,
    existing: ExistingOutput,
    files: &[String],
    check_cancelled: &impl Fn() -> Result<(), String>,
) -> Result<Option<String>, String> {
    let output = output.canonicalize().map_err(output_error)?;
    let prior = directory_sources(&output)?;
    let replace_manifest = prior.is_some();
    let prior = prior.unwrap_or_default();
    let mut order = Vec::new();
    if existing == ExistingOutput::Append {
        for source in &prior {
            let name = source
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or("已有分文件备份的文件名无效。")?;
            order.push(name.to_owned());
        }
    }
    order.extend_from_slice(files);
    let manifest = NamedTempFile::new_in(staging).map_err(output_error)?;
    record_restore_order(manifest.as_file(), &order)?;

    // Keep the previous index usable until all parts from this run are in place.
    // These guards remove only the new files if a move or index publication fails.
    let mut installed = Vec::with_capacity(files.len());
    for name in files {
        check_cancelled()?;
        let destination = output.join(name);
        let part = TempPath::try_from_path(staging.join(name)).map_err(output_error)?;
        part.persist_noclobber(&destination)
            .map_err(|error| output_error(error.error))?;
        installed.push(TempPath::try_from_path(destination).map_err(output_error)?);
    }
    check_cancelled()?;
    let index = output.join(RESTORE_MANIFEST_NAME);
    let saved = if replace_manifest {
        manifest.persist(index)
    } else {
        manifest.persist_noclobber(index)
    };
    saved.map_err(|error| output_error(error.error))?;
    for part in &mut installed {
        part.disable_cleanup(true);
    }

    let mut cleanup_warning = None;
    if existing == ExistingOutput::Replace {
        for source in prior {
            let removed = match fs::symlink_metadata(&source.path) {
                Ok(metadata) if source.matches(&metadata) => fs::remove_file(&source.path)
                    .or_else(|error| {
                        if error.kind() == io::ErrorKind::NotFound {
                            Ok(())
                        } else {
                            Err(error)
                        }
                    })
                    .map_err(output_error),
                Ok(_) => Err(format!("「{}」已变化。", source.path.display())),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(output_error(error)),
            };
            if let Err(error) = removed {
                cleanup_warning
                    .get_or_insert_with(|| format!("新备份已发布，有旧备份文件无法移除。{error}"));
            }
        }
    }
    Ok(cleanup_warning)
}

fn rename_split_directory(
    staging: &Path,
    output: &Path,
    empty_directory: bool,
) -> Result<(), String> {
    let Err(error) = fs::rename(staging, output) else {
        return Ok(());
    };
    if !cfg!(windows) || !empty_directory {
        return Err(output_error(error));
    }
    // Filesystems without Windows' replacement support require an absent destination.
    fs::remove_dir(output).map_err(output_error)?;
    if let Err(error) = fs::rename(staging, output) {
        if let Err(restore) = fs::create_dir(output)
            && restore.kind() != io::ErrorKind::AlreadyExists
        {
            return Err(format!(
                "{}无法重新创建所选空目录。{restore}",
                output_error(error)
            ));
        }
        return Err(output_error(error));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_named_split_backups_with_restore_order() {
        let executable = std::env::current_exe().unwrap();
        let root = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let selected = root.path().join("selected");
        fs::create_dir(&selected).unwrap();
        for (directory_mode, output) in [
            (BackupDirectoryMode::Selected, selected),
            (
                BackupDirectoryMode::Timestamped,
                root.path().join("2026-11-20 08-13-00"),
            ),
        ] {
            let settings = DumpSettings {
                file_per_object: true,
                directory_mode,
                ..DumpSettings::default()
            };
            let parts = [
                ("table", "CREATE TABLE sample (id INT);"),
                ("data", "INSERT INTO sample VALUES (1);"),
            ];
            let summary = write_dump_output(&output, &settings, |out| {
                for (name, sql) in parts {
                    out.open_part(name).map_err(output_error)?;
                    out.write_all(sql.as_bytes()).map_err(output_error)?;
                    out.close_part().map_err(output_error)?;
                }
                Ok(TransferSummary::default())
            })
            .unwrap();
            assert_eq!(summary.output_directory.as_deref(), Some(output.as_path()));
            let manifest: serde_json::Value =
                serde_json::from_slice(&fs::read(output.join("restore-order.json")).unwrap())
                    .unwrap();
            assert_eq!(
                manifest,
                serde_json::json!({"version":1,"files":["00000001-table.sql","00000002-data.sql"]})
            );
            for (file, (_, sql)) in manifest["files"].as_array().unwrap().iter().zip(parts) {
                assert_eq!(
                    fs::read_to_string(output.join(file.as_str().unwrap())).unwrap(),
                    sql
                );
            }
        }
    }

    #[test]
    fn appends_and_replaces_selected_split_backups_in_restore_order() {
        let executable = std::env::current_exe().unwrap();
        let root = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let output = root.path().join("selected");
        let mut settings = DumpSettings {
            file_per_object: true,
            directory_mode: BackupDirectoryMode::Selected,
            ..DumpSettings::default()
        };
        let publish = |settings: &DumpSettings, sql: [&str; 2]| {
            write_dump_output(&output, settings, |out| {
                for (name, sql) in ["table", "data"].into_iter().zip(sql) {
                    out.open_part(name).map_err(output_error)?;
                    out.write_all(sql.as_bytes()).map_err(output_error)?;
                    out.close_part().map_err(output_error)?;
                }
                Ok(TransferSummary::default())
            })
            .unwrap()
        };
        let sources = || {
            directory_sources(&output.canonicalize().unwrap())
                .unwrap()
                .unwrap()
        };
        let first = [
            "CREATE TABLE example (id INT);",
            "INSERT INTO example VALUES (1);",
        ];
        let second = [
            "CREATE TABLE extra (id INT);",
            "INSERT INTO extra VALUES (2);",
        ];
        publish(&settings, first);
        let unrelated = output.join("00000003-unrelated.sql");
        fs::write(&unrelated, "SELECT 3;").unwrap();

        settings.append = true;
        let timestamp = 1_780_000_000_000;
        settings.split_filename_timestamp = Some(timestamp);
        publish(&settings, second);
        let appended = sources();
        let expected_prefix = format!("{}_", backup_timestamp(timestamp));
        assert!(appended[2..].iter().all(|source| {
            source
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&expected_prefix))
        }));
        let statements: Vec<_> = appended
            .iter()
            .map(|source| fs::read_to_string(&source.path).unwrap())
            .collect();
        assert_eq!(statements, [first, second].concat());

        settings.append = false;
        settings.overwrite = true;
        assert!(publish(&settings, second).cleanup_warning.is_none());
        let statements: Vec<_> = sources()
            .iter()
            .map(|source| fs::read_to_string(&source.path).unwrap())
            .collect();
        assert_eq!(statements, second);
        assert!(appended.iter().all(|source| !source.path.exists()));
        assert_eq!(fs::read_to_string(unrelated).unwrap(), "SELECT 3;");
    }
}
