use std::collections::HashSet;
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use super::{
    GeneratedClientEncoding, RESTORE_MANIFEST_LIMIT, RestoreEncoding, validate_sql_encoding,
};
use crate::database::transfer::DUMP_CLIENT_STATE;
use crate::sql_encoding::{SqlDumpCharset, decode_sql, parse_marker};

const MAX_GENERATED_HEADER: u64 = 4 * 1024;
const DECODED_HTTP_LIMIT: &str = "HTTP 还原要求解码后的 SQL 文件不超过 64 MiB。";
const DECODED_SIZE_CHANGED: &str = "转码后的 SQL 文件长度已变化，请重新选择文件。";
pub(crate) const RESTORE_MANIFEST_NAME: &str = "restore-order.json";

pub(crate) struct Source {
    pub path: PathBuf,
    pub bytes: u64,
    modified: Option<SystemTime>,
}

impl Source {
    fn new(path: PathBuf) -> Result<Self, String> {
        let metadata = fs::metadata(&path)
            .map_err(|error| format!("无法读取「{}」。{error}", path.display()))?;
        if !metadata.is_file() {
            return Err("恢复顺序只能包含普通 SQL 文件。".into());
        }
        Ok(Self {
            path,
            bytes: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }

    pub(crate) fn matches(&self, metadata: &Metadata) -> bool {
        metadata.is_file()
            && self.bytes == metadata.len()
            && self.modified == metadata.modified().ok()
    }

    pub(crate) fn open(&self) -> Result<File, String> {
        let file = File::open(&self.path)
            .map_err(|error| format!("无法打开「{}」。{error}", self.path.display()))?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("无法读取 SQL 文件信息。{error}"))?;
        if !self.matches(&metadata) {
            return Err(format!(
                "「{}」已变化，请重新选择备份。",
                self.path.display()
            ));
        }
        Ok(file)
    }

    pub(crate) fn prepare(
        self,
        selected: Option<SqlDumpCharset>,
        max_decoded_bytes: Option<u64>,
        mut checkpoint: impl FnMut(u64) -> Result<(), String>,
    ) -> Result<PreparedSource, String> {
        checkpoint(0)?;
        let mut file = self.open()?;
        let encoding = detect_encoding(&mut file, selected)?;
        let bytes = if let Some(charset) = encoding.charset() {
            self.decode(
                &mut file,
                charset,
                io::sink(),
                max_decoded_bytes.map(|limit| (limit, DECODED_HTTP_LIMIT)),
                &mut checkpoint,
            )?
        } else {
            validate_sql_encoding(&mut file, &mut checkpoint)?;
            let metadata = file
                .metadata()
                .map_err(|error| format!("无法读取 SQL 文件信息。{error}"))?;
            if !self.matches(&metadata) {
                return Err("检查期间 SQL 文件已变化，请重新选择文件。".into());
            }
            self.bytes
        };
        Ok(PreparedSource {
            source: self,
            bytes,
            encoding,
        })
    }

    fn decode(
        &self,
        file: &mut File,
        encoding: SqlDumpCharset,
        writer: impl Write,
        limit: Option<(u64, &'static str)>,
        checkpoint: &mut impl FnMut(u64) -> Result<(), String>,
    ) -> Result<u64, String> {
        let mut input = DecodingInput {
            file,
            bytes: 0,
            length: self.bytes,
            checkpoint,
            stopped: None,
        };
        let mut output = DecodingOutput {
            writer,
            bytes: 0,
            limit,
        };
        let decoded = decode_sql(encoding, &mut input, &mut output);
        if let Some(reason) = input.stopped {
            return Err(reason);
        }
        if let Err(error) = decoded {
            let reason = error.to_string();
            return Err(if limit.is_some_and(|(_, message)| reason == message) {
                reason
            } else {
                format!(
                    "无法从声明的 {} 编码转为 UTF-8。{reason}",
                    encoding.mysql_name()
                )
            });
        }
        let metadata = input
            .file
            .metadata()
            .map_err(|error| format!("无法读取 SQL 文件信息。{error}"))?;
        if input.bytes != self.bytes || !self.matches(&metadata) {
            return Err("转码期间 SQL 文件已变化，请重新选择文件。".into());
        }
        Ok(output.bytes)
    }
}

pub(crate) struct PreparedSource {
    source: Source,
    pub(crate) bytes: u64,
    pub(crate) encoding: RestoreEncoding,
}

impl PreparedSource {
    pub(crate) fn path(&self) -> &Path {
        &self.source.path
    }

    pub(crate) fn open(
        &self,
        mut checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<File, String> {
        checkpoint()?;
        let mut source_file = self.source.open()?;
        let Some(charset) = self.encoding.charset() else {
            return Ok(source_file);
        };
        let selected = if let RestoreEncoding::Explicit(selected) = self.encoding {
            Some(selected)
        } else {
            None
        };
        if detect_encoding(&mut source_file, selected)? != self.encoding {
            return Err("SQL 转储文件头已变化，请重新选择文件。".into());
        }
        let mut decoded = tempfile::tempfile()
            .or_else(|error| match self.source.path.parent() {
                Some(folder)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
                    ) =>
                {
                    tempfile::tempfile_in(folder)
                }
                _ => Err(error),
            })
            .map_err(|error| format!("无法为 SQL 转码创建临时文件。{error}"))?;
        let bytes = self.source.decode(
            &mut source_file,
            charset,
            &mut decoded,
            Some((self.bytes, DECODED_SIZE_CHANGED)),
            &mut |_| checkpoint(),
        )?;
        if bytes != self.bytes {
            return Err(DECODED_SIZE_CHANGED.into());
        }
        decoded
            .rewind()
            .map_err(|error| format!("无法读取转码后的 SQL 文件。{error}"))?;
        Ok(decoded)
    }
}

fn detect_encoding(
    file: &mut File,
    selected: Option<SqlDumpCharset>,
) -> Result<RestoreEncoding, String> {
    let mut prefix = Vec::new();
    Read::by_ref(file)
        .take(MAX_GENERATED_HEADER)
        .read_to_end(&mut prefix)
        .map_err(|error| format!("无法读取 SQL 转储文件头。{error}"))?;
    file.rewind()
        .map_err(|error| format!("无法从头读取 SQL 文件。{error}"))?;
    let first_line = prefix
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(prefix.len(), |offset| offset + 1);
    let charset = parse_marker(&prefix[..first_line])
        .map_err(|error| format!("SQL 转储编码标记无效。{error}"))?;
    if let Some(charset) = charset {
        if selected.is_some_and(|selected| selected != charset) {
            return Err("所选编码与 SQL 转储编码标记不一致。".into());
        }
        let mut rest = &prefix[first_line..];
        while rest.starts_with(b"-- ")
            && let Some(end) = rest.iter().position(|byte| *byte == b'\n')
        {
            rest = &rest[end + 1..];
        }
        let generated = GeneratedClientEncoding::new(charset);
        let header = rest
            .strip_prefix(DUMP_CLIENT_STATE.as_bytes())
            .is_some_and(|after| after.starts_with(generated.tagged_line().as_bytes()));
        if !header {
            return Err("SQL 转储编码标记后缺少匹配的港湾 SQL 文件头。".into());
        }
        return Ok(RestoreEncoding::Marked(charset));
    }
    if let Some(selected) = selected {
        if prefix.starts_with(b"\xef\xbb\xbf")
            && !matches!(selected, SqlDumpCharset::Utf8Mb3 | SqlDumpCharset::Utf8Mb4)
        {
            return Err("文件的 UTF-8 BOM 与所选编码不一致。".into());
        }
        return Ok(RestoreEncoding::Explicit(selected));
    }
    Ok(RestoreEncoding::Plain)
}

struct DecodingInput<'a, F> {
    file: &'a mut File,
    bytes: u64,
    length: u64,
    checkpoint: &'a mut F,
    stopped: Option<String>,
}

impl<F: FnMut(u64) -> Result<(), String>> Read for DecodingInput<'_, F> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if let Err(reason) = (self.checkpoint)(self.bytes) {
            self.stopped = Some(reason);
            return Err(io::Error::other("SQL 转码已中止。"));
        }
        let count = self.file.read(buffer)?;
        if count as u64 > self.length.saturating_sub(self.bytes) {
            return Err(io::Error::other("转码期间 SQL 文件长度已变化。"));
        }
        self.bytes += count as u64;
        Ok(count)
    }
}

struct DecodingOutput<W> {
    writer: W,
    bytes: u64,
    limit: Option<(u64, &'static str)>,
}

impl<W: Write> Write for DecodingOutput<W> {
    fn write(&mut self, value: &[u8]) -> io::Result<usize> {
        if let Some((limit, reason)) = self.limit
            && value.len() as u64 > limit.saturating_sub(self.bytes)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, reason));
        }
        let count = self.writer.write(value)?;
        self.bytes = self
            .bytes
            .checked_add(count as u64)
            .ok_or_else(|| io::Error::other("转码后的 SQL 文件超过支持范围。"))?;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    files: Vec<String>,
}

pub(crate) fn sources(path: &Path) -> Result<Vec<Source>, String> {
    let absolute = path
        .canonicalize()
        .map_err(|error| format!("无法定位 SQL 文件或备份目录。{error}"))?;
    if !absolute.is_dir() {
        return Ok(vec![Source::new(absolute)?]);
    }
    directory_sources(&absolute)?.ok_or_else(|| "备份目录中缺少恢复顺序文件。".into())
}

pub(crate) fn directory_sources(directory: &Path) -> Result<Option<Vec<Source>>, String> {
    match fs::symlink_metadata(directory.join(RESTORE_MANIFEST_NAME)) {
        Ok(metadata) if metadata.is_file() => (),
        Ok(_) => return Err("恢复顺序文件须为备份目录内的普通文件。".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("无法读取恢复顺序文件。{error}")),
    }
    let index = child(directory, RESTORE_MANIFEST_NAME)?;
    let (file, bytes) = super::open_sql_file(&index)?;
    if bytes > RESTORE_MANIFEST_LIMIT {
        return Err("恢复顺序文件超过 8 MiB。".into());
    }
    let mut bytes = Vec::new();
    file.take(RESTORE_MANIFEST_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("无法读取恢复顺序文件。{error}"))?;
    if bytes.len() as u64 > RESTORE_MANIFEST_LIMIT {
        return Err("恢复顺序文件超过 8 MiB。".into());
    }
    let index: Manifest =
        serde_json::from_slice(&bytes).map_err(|error| format!("恢复顺序文件无效。{error}"))?;
    if !matches!(index.version, 1 | 2) || index.files.is_empty() {
        return Err("需要包含 SQL 文件清单的版本 1 或版本 2 恢复顺序文件。".into());
    }
    let mut seen = HashSet::new();
    index
        .files
        .iter()
        .enumerate()
        .map(|(position, name)| {
            let valid = if index.version == 1 {
                ordered_part_name(position, name)
            } else {
                name.strip_suffix(".sql")
                    .and_then(|stem| stem.split_once('-'))
                    .is_some_and(|(number, label)| {
                        number.len() >= 4
                            && number.bytes().all(|byte| byte.is_ascii_digit())
                            && !label.is_empty()
                    })
            };
            if !valid || !seen.insert(name) {
                return Err(format!("恢复顺序中的第 {} 个文件名无效。", position + 1));
            }
            Source::new(child(directory, name)?)
        })
        .collect::<Result<_, _>>()
        .map(Some)
}

pub(crate) fn ordered_part_name(position: usize, name: &str) -> bool {
    let prefix = format!("{:08}-", position + 1);
    name.strip_prefix(&prefix)
        .and_then(|label| label.strip_suffix(".sql"))
        .is_some_and(|label| !label.is_empty())
}

fn child(directory: &Path, name: &str) -> Result<PathBuf, String> {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err("恢复顺序只能引用当前备份目录中的文件名。".into());
    }
    let child = directory
        .join(name)
        .canonicalize()
        .map_err(|error| format!("无法定位备份目录中的「{name}」。{error}"))?;
    if child.parent() != Some(directory) || child.file_name().is_none_or(|actual| actual != name) {
        return Err(format!("「{name}」未指向备份目录中的同名文件。"));
    }
    Ok(child)
}
