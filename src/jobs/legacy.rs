use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::num::NonZeroU32;
use std::path::Path;

use quick_xml::{Reader, events::Event};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use tempfile::NamedTempFile;

use crate::APP_NAME;
use crate::database::transfer::{BackupDirectoryMode, DEFAULT_DUMP_CHUNK_ROWS};
use crate::sql_encoding::SqlDumpCharset;

use super::legacy_report::{self, LegacyReport, ReportArgs, ReportInfo};
use super::{
    JOB_FILE_LIMIT, Job, JobFile, SJA_TIMESTAMP_TOKEN, TIMESTAMP_TOKEN, create_parent, paths_match,
};

const LEGACY_DEFAULT_BULK_INSERT_KIB: u32 = 1000;
pub(super) const REPLICATION_LOCK_ARG: &str = "--lock-source-for-legacy-replication";

pub(super) fn import(args: &[OsString]) -> Result<(), String> {
    let [input, destination, options @ ..] = args else {
        return Err(super::USAGE.into());
    };
    let input = Path::new(input);
    let destination = std::path::absolute(Path::new(destination))
        .map_err(|error| format!("无法定位新作业文件：{error}"))?;
    let (definition, notes) = prepare(input, &destination, options)?;
    save_definition(input, &destination, &definition)?;
    println!("已转换备份作业。{}", destination.display());
    for note in notes {
        eprintln!("{note}");
    }
    Ok(())
}

pub(super) fn prepare(
    input: &Path,
    destination: &Path,
    args: &[OsString],
) -> Result<(Value, Vec<String>), String> {
    let (output, password_env, report_args) = match args {
        [output, flag, name, report_args @ ..] if flag == "--password-env" => {
            let name = name
                .to_str()
                .filter(|name| !name.is_empty() && !name.contains(['=', '\0']))
                .ok_or("请输入有效的密码环境变量名。")?;
            (output, Some(name), report_args)
        }
        [output, flag, report_args @ ..] if flag == "--passwordless" => (output, None, report_args),
        _ => return Err(super::USAGE.into()),
    };
    let output = output.to_str().ok_or("备份输出路径应为 Unicode 路径。")?;
    let directory = destination.parent().ok_or("请设置新作业文件路径。")?;
    let output_path = directory.join(output);
    if destination.exists() || paths_match(destination, input) {
        return Err("新作业文件已经存在。请选择新路径。".into());
    }
    if paths_match(&output_path, destination)
        || paths_match(&output_path, input)
        || paths_match(
            &output_path,
            &super::append_suffix(destination, ".log.jsonl"),
        )
        || paths_match(&output_path, &super::append_suffix(destination, ".lock"))
    {
        return Err("请为旧作业、新作业、日志、锁文件和备份选择不同路径。".into());
    }
    let (copy_global_lock, report_args) = match report_args {
        [flag, rest @ ..] if flag == REPLICATION_LOCK_ARG => (true, rest),
        _ => (false, report_args),
    };
    let legacy = read(input)?;
    let replication = legacy.replication_flags()?;
    let (mut definition, mut notes) = legacy.to_native(output, password_env)?;
    if copy_global_lock {
        if !replication.0 && !replication.1 {
            return Err("旧作业没有启用主库或从库日志选项。".into());
        }
        definition["job"]["options"]["lock_source_server"] = json!(true);
        definition["job"]["options"]["lock_source_tables"] = json!(false);
        notes.push(
            "已迁移旧选项的全局读锁。备份读取时执行 FLUSH TABLES WITH READ LOCK，读取结束后释放。"
                .into(),
        );
    }
    for (selected, option) in [(replication.0, "主库日志"), (replication.1, "从库日志")] {
        if selected {
            notes.push(format!(
                "旧作业启用了{option}选项。{APP_NAME} 支持从该选项迁移全局读锁。"
            ));
        }
    }
    legacy_report::migrate(
        legacy.sendreport.as_ref(),
        ReportArgs::parse(report_args)?,
        &[password_env],
        &mut definition,
        &mut notes,
    )?;
    let parsed: JobFile = serde_json::from_value(definition.clone())
        .map_err(|_| format!("无法生成有效的 {APP_NAME} 作业。"))?;
    parsed.connection.validate()?;
    match &parsed.job {
        Job::Backup(backup) => backup.validate()?,
        Job::BackupDatabases(backup) => {
            backup.validate()?;
            if backup.output_file.is_some() {
                let started = super::unix_ms();
                let output_path = backup
                    .run_path(directory, started)
                    .ok_or("请设置多库备份输出文件。")?;
                let manifest = backup.manifest_path(&output_path, started);
                let log = super::append_suffix(destination, ".log.jsonl");
                let lock = super::append_suffix(destination, ".lock");
                if [&output_path, &manifest].into_iter().any(|output| {
                    [input, destination, &log, &lock]
                        .into_iter()
                        .any(|path| paths_match(output, path))
                }) {
                    return Err("请为旧作业、新作业、日志、锁文件和备份清单选择不同路径。".into());
                }
                backup.validate_append_path(&output_path)?;
            }
        }
        _ => return Err("旧版备份转换生成了错误的作业类型。".into()),
    }
    Ok((definition, notes))
}

pub(super) fn directory_output(input: &Path) -> Result<bool, String> {
    let legacy = read(input)?;
    let all = flag(&legacy.export.databases.all)?.unwrap_or(false);
    let schemas = &legacy.export.databases.items;
    if all != schemas.is_empty() {
        return Err("旧版备份须选择全部数据库或列出具体数据库。".into());
    }
    let multiple = all || schemas.len() > 1;
    let layout = legacy
        .script(multiple)
        .and_then(|script| script.layout.as_deref())
        .map(str::trim);
    Ok(
        layout.is_some_and(|layout| layout.eq_ignore_ascii_case("dir"))
            || (multiple && layout.is_none()),
    )
}

pub(super) fn report_info(input: &Path) -> Result<Option<ReportInfo>, String> {
    legacy_report::info(read(input)?.sendreport.as_ref())
}

pub(super) fn uses_replication_flags(input: &Path) -> Result<bool, String> {
    let (master, slave) = read(input)?.replication_flags()?;
    Ok(master || slave)
}

pub(super) fn save_definition(
    input: &Path,
    destination: &Path,
    definition: &Value,
) -> Result<(), String> {
    if destination.exists() || paths_match(destination, input) {
        return Err("新作业文件已经存在。请选择新路径。".into());
    }
    let directory = destination.parent().ok_or("请设置新作业文件路径。")?;
    create_parent(destination)?;
    let mut temporary =
        NamedTempFile::new_in(directory).map_err(|error| format!("无法创建新作业文件：{error}"))?;
    serde_json::to_writer_pretty(&mut temporary, definition)
        .map_err(|error| format!("无法保存新作业定义：{error}"))?;
    temporary
        .write_all(b"\n")
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| format!("无法保存新作业文件：{error}"))?;
    temporary
        .persist_noclobber(destination)
        .map_err(|error| format!("无法写入新作业文件：{}", error.error))?;
    Ok(())
}

#[derive(Deserialize)]
struct LegacyFile {
    #[serde(rename = "exportjob", default)]
    backups: Vec<LegacyBackup>,
    notifyjob: Option<serde::de::IgnoredAny>,
    syncjob: Option<serde::de::IgnoredAny>,
    odbcimport: Option<serde::de::IgnoredAny>,
}

pub(super) fn parse_xml<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let metadata = fs::metadata(path).map_err(|error| format!("无法读取旧作业属性：{error}"))?;
    if !metadata.is_file() || metadata.len() > JOB_FILE_LIMIT {
        return Err("请选择不超过 1 MiB 的普通 XML 作业文件。".into());
    }
    let mut raw = Vec::new();
    File::open(path)
        .and_then(|file| file.take(JOB_FILE_LIMIT + 1).read_to_end(&mut raw))
        .map_err(|error| format!("无法读取旧作业：{error}"))?;
    if raw.len() as u64 > JOB_FILE_LIMIT {
        return Err("旧版作业文件最多支持 1 MiB。".into());
    }
    let raw = raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&raw);
    let mut reader = Reader::from_reader(raw);
    let mut found_root = false;
    loop {
        match reader.read_event().map_err(|_| "旧作业 XML 无效。")? {
            Event::Start(root) | Event::Empty(root) if !found_root => {
                if root.name().as_ref() != b"job" {
                    return Err("旧版作业 XML 应从 job 元素开始。".into());
                }
                found_root = true;
            }
            Event::DocType(_) => return Err("旧版作业 XML 不支持 DTD。".into()),
            Event::Eof => break,
            _ => {}
        }
    }
    // Types only name nonsecret fields. Password elements are ignored by the XML deserializer.
    quick_xml::de::from_reader(raw).map_err(|_| "无法解析此类型的旧版 XML 作业。".into())
}

fn read(path: &Path) -> Result<LegacyBackup, String> {
    let parsed: LegacyFile = parse_xml(path)?;
    if parsed.odbcimport.is_some() {
        return Err("请使用 import-legacy-odbc 转换旧版 ODBC 导入作业。".into());
    }
    if parsed.notifyjob.is_some() || parsed.syncjob.is_some() || parsed.backups.len() != 1 {
        return Err("此转换入口接受一个旧版 SQL 备份 exportjob。".into());
    }
    parsed
        .backups
        .into_iter()
        .next()
        .ok_or_else(|| "没有可转换的旧版备份作业。".into())
}

#[derive(Deserialize)]
struct LegacyBackup {
    source: LegacySource,
    export: LegacyExport,
    #[serde(default)]
    options: LegacyOptions,
    abortonerror: Option<String>,
    sendreport: Option<LegacyReport>,
}

#[derive(Deserialize)]
struct LegacySource {
    host: String,
    user: String,
    port: Option<u16>,
    ssl: Option<String>,
    sslauth: Option<String>,
    ssh: Option<serde::de::IgnoredAny>,
    http: Option<serde::de::IgnoredAny>,
    tunnel: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct LegacyExport {
    databases: LegacyDatabases,
    script: Option<LegacyScript>,
}

#[derive(Deserialize)]
struct LegacyDatabases {
    #[serde(rename = "@all")]
    all: Option<String>,
    #[serde(rename = "@mysqldb")]
    mysqldb: Option<String>,
    #[serde(rename = "database", default)]
    items: Vec<LegacySchema>,
}

#[derive(Deserialize)]
struct LegacySchema {
    #[serde(rename = "@name")]
    name: String,
    objects: LegacyObjects,
    script: Option<LegacyScript>,
}

#[derive(Deserialize)]
struct LegacyScript {
    #[serde(rename = "@fileordir")]
    layout: Option<String>,
    #[serde(rename = "@timestampdir")]
    timestamp_directory: Option<String>,
    #[serde(rename = "@timestamp")]
    timestamp: Option<String>,
}

#[derive(Deserialize)]
struct LegacyObjects {
    #[serde(rename = "@all")]
    all: Option<String>,
    tables: Option<LegacyGroup>,
    views: Option<LegacyGroup>,
    procedures: Option<LegacyGroup>,
    functions: Option<LegacyGroup>,
    triggers: Option<LegacyGroup>,
    events: Option<LegacyGroup>,
    script: Option<LegacyScript>,
}

#[derive(Deserialize)]
struct LegacyGroup {
    #[serde(rename = "@all")]
    all: Option<String>,
    #[serde(rename = "$value", default)]
    objects: Vec<LegacyObject>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum LegacyObject {
    Table(LegacyName),
    View(LegacyName),
    Procedure(LegacyName),
    Function(LegacyName),
    Trigger(LegacyName),
    Event(LegacyName),
}

#[derive(Deserialize)]
struct LegacyName {
    #[serde(rename = "@name")]
    attribute: Option<String>,
    #[serde(rename = "$text")]
    text: Option<String>,
}

impl LegacyObject {
    fn parts(&self) -> (&'static str, Option<&str>) {
        let (kind, name) = match self {
            Self::Table(name) => ("table", name),
            Self::View(name) => ("view", name),
            Self::Procedure(name) => ("procedure", name),
            Self::Function(name) => ("function", name),
            Self::Trigger(name) => ("trigger", name),
            Self::Event(name) => ("event", name),
        };
        (kind, name.attribute.as_deref().or(name.text.as_deref()))
    }
}

#[derive(Default, Deserialize)]
struct LegacyOptions {
    createdb: Option<String>,
    usedb: Option<String>,
    dropobjects: Option<String>,
    bulkinsert: Option<String>,
    flushlogs: Option<String>,
    locktableread: Option<String>,
    locktablewrite: Option<String>,
    fkchecks: Option<String>,
    singletransaction: Option<String>,
    completeinsert: Option<String>,
    insertdelayed: Option<String>,
    flushmaster: Option<String>,
    flushslave: Option<String>,
    disablekeys: Option<String>,
    autocommit: Option<String>,
    chunkinsert: Option<String>,
    chunklimit: Option<String>,
    bulksize: Option<String>,
    appendtofile: Option<String>,
    usecharset: Option<String>,
}

impl LegacyBackup {
    fn replication_flags(&self) -> Result<(bool, bool), String> {
        Ok((
            flag(&self.options.flushmaster)?.unwrap_or(false),
            flag(&self.options.flushslave)?.unwrap_or(false),
        ))
    }

    fn script(&self, multiple: bool) -> Option<&LegacyScript> {
        self.export.script.as_ref().or_else(|| {
            let schema = self.export.databases.items.first().filter(|_| !multiple)?;
            schema.script.as_ref().or(schema.objects.script.as_ref())
        })
    }

    fn to_native(
        &self,
        output: &str,
        password_env: Option<&str>,
    ) -> Result<(Value, Vec<String>), String> {
        if self.source.ssh.is_some()
            || self.source.http.is_some()
            || self.source.tunnel.is_some()
            || flag(&self.source.sslauth)?.unwrap_or(false)
        {
            return Err("旧作业使用隧道或 TLS 客户端证书，请单独编写新的连接配置。".into());
        }
        let databases = &self.export.databases;
        let all = flag(&databases.all)?.unwrap_or(false);
        if all != databases.items.is_empty() {
            return Err("旧版备份须选择全部数据库或列出具体数据库。".into());
        }
        let multi = all || databases.items.len() > 1;
        let ssl = flag(&self.source.ssl)?.unwrap_or(false);
        let mut connection = json!({
            "host":self.source.host,"user":self.source.user,"port":self.source.port.unwrap_or(3306),
            "password_env":password_env,
        });
        if ssl {
            connection["tls"] = json!({});
        }
        let options = &self.options;
        let mut mapped = Map::new();
        for (key, value) in [
            ("include_create_database", &options.createdb),
            ("include_use", &options.usedb),
            ("drop_existing", &options.dropobjects),
            ("bulk_insert", &options.bulkinsert),
            ("flush_logs", &options.flushlogs),
            ("lock_source_tables", &options.locktableread),
            ("lock_insert_tables", &options.locktablewrite),
            ("disable_foreign_key_checks", &options.fkchecks),
            ("complete_insert", &options.completeinsert),
            ("insert_delayed", &options.insertdelayed),
            ("disable_keys", &options.disablekeys),
            ("disable_autocommit", &options.autocommit),
        ] {
            if let Some(value) = flag(value)? {
                mapped.insert(key.into(), json!(value));
            }
        }
        mapped.insert(
            "single_transaction".into(),
            json!(flag(&options.singletransaction)?.unwrap_or(false)),
        );
        // An omitted or empty value keeps the native backup default.
        if let Some(name) = options
            .usecharset
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            // SJA before 6.1 used a switch for UTF-8 instead of a character set name.
            let charset = if name.eq_ignore_ascii_case("yes") {
                SqlDumpCharset::Utf8Mb3
            } else if name.eq_ignore_ascii_case("no") {
                return Err(
                    "旧备份作业的 usecharset=no 未指定文件字符集。请填写具体的字符集名称后再转换。"
                        .into(),
                );
            } else {
                SqlDumpCharset::parse(name)
                    .ok_or_else(|| format!("暂不支持旧备份作业指定的文件字符集 {name:?}。"))?
            };
            mapped.insert("file_charset".into(), json!(charset));
        }
        if flag(&options.chunkinsert)?.unwrap_or(false) {
            let rows = options
                .chunklimit
                .as_deref()
                .map(str::trim)
                .filter(|rows| !rows.is_empty())
                .map(|rows| {
                    rows.parse::<NonZeroU32>()
                        .map(NonZeroU32::get)
                        .map_err(|_| "旧作业启用了分批读取，chunklimit 应为正整数。")
                })
                .transpose()?
                .unwrap_or(DEFAULT_DUMP_CHUNK_ROWS);
            mapped.insert("chunk_rows".into(), json!(rows));
        }
        let mut notes = Vec::new();
        let bulk_size = options
            .bulksize
            .as_deref()
            .map(str::trim)
            .filter(|size| !size.is_empty())
            .map(str::parse::<u32>)
            .unwrap_or(Ok(LEGACY_DEFAULT_BULK_INSERT_KIB));
        match bulk_size {
            Ok(size) => {
                mapped.insert(
                    "bulk_insert_limit_kib".into(),
                    json!((size != 0).then_some(size)),
                );
            }
            Err(_) => notes.push(
                "旧作业的批量 INSERT 大小无法识别。可在新作业中设置 bulk_insert_limit_kib。".into(),
            ),
        }
        let abort = flag(&self.abortonerror)?;
        if let Some(abort) = abort {
            mapped.insert("continue_on_error".into(), json!(!abort));
        }
        let (mut job, script) = if multi {
            let script = self.script(multi);
            let layout = script
                .and_then(|script| script.layout.as_deref())
                .map(str::trim);
            let output_key = match layout {
                Some(layout) if layout.eq_ignore_ascii_case("file") => "output_file",
                None => "output_directory",
                Some(layout) if layout.eq_ignore_ascii_case("dir") => "output_directory",
                _ => return Err("旧版多库备份的输出布局无法转换。".into()),
            };
            let include_mysql = if all {
                flag(&databases.mysqldb)?.unwrap_or(false)
            } else {
                databases
                    .items
                    .iter()
                    .any(|schema| schema.name.eq_ignore_ascii_case("mysql"))
            };
            let mut job = json!({
                "type":"backup_databases", "options":mapped,
                "include_mysql":include_mysql, "continue_on_error":abort == Some(false),
            });
            job[output_key] = json!(output);
            if all {
                job["all_user_schemas"] = json!(true);
            } else {
                job["schemas"] = json!(
                    databases
                        .items
                        .iter()
                        .map(LegacySchema::native_selection)
                        .collect::<Result<Vec<_>, String>>()?
                );
            }
            if include_mysql {
                notes.push("已保留旧作业对 mysql 系统库的选择。转储会包含账户认证和权限表，请检查备份目录的访问权限。".into());
            }
            (job, script)
        } else {
            let schema = &databases.items[0];
            let mut job = schema.native_selection()?;
            job["type"] = json!("backup");
            job["output"] = json!(output);
            let script = self.script(multi);
            if script
                .and_then(|script| script.layout.as_deref())
                .is_some_and(|layout| layout.trim().eq_ignore_ascii_case("dir"))
            {
                mapped.insert("file_per_object".into(), json!(true));
            }
            job["options"] = json!(mapped);
            (job, script)
        };
        if ssl {
            notes.push("新作业使用系统信任链校验 TLS。原版证书路径需要按新环境配置。".into());
        }
        let single_file_key = if job.get("output_file").is_some() {
            Some("output_file")
        } else if !multi && job["options"]["file_per_object"] != true {
            Some("output")
        } else {
            None
        };
        let timestamp_files = script
            .map(|script| script_flag(&script.timestamp))
            .transpose()?
            .unwrap_or(false);
        let append = flag(&options.appendtofile)?;
        if single_file_key.is_none() {
            let timestamp_directory = script
                .map(|script| script_flag(&script.timestamp_directory))
                .transpose()?
                .unwrap_or(false);
            let mode = if timestamp_directory {
                notes.push("分文件备份将在所选目录下创建本地时间命名的子目录。".into());
                if timestamp_files {
                    notes.push(
                        "原版启用目录时间戳时会关闭文件名时间前缀，转换时采用目录时间戳。".into(),
                    );
                }
                BackupDirectoryMode::Timestamped
            } else {
                if timestamp_files {
                    job["options"]["split_filename_timestamp"] = json!(true);
                    job["options"]["append"] = json!(true);
                    notes.push("分文件备份将在所选目录中使用本地时间文件名前缀。每次运行生成独立文件，并将它们依次加入恢复顺序。".into());
                } else {
                    match append {
                        Some(true) => {
                            job["options"]["append"] = json!(true);
                            notes.push(
                                "分文件备份将使用所选路径，并把本次文件依次加入恢复顺序。".into(),
                            );
                        }
                        Some(false) => {
                            job["options"]["overwrite"] = json!(true);
                            notes.push(
                                "分文件备份将使用所选路径，并替换原恢复顺序中的备份文件。".into(),
                            );
                        }
                        None => notes.push(
                            "分文件备份将使用所选路径。请选择空目录或尚未创建的目录。".into(),
                        ),
                    }
                }
                BackupDirectoryMode::Selected
            };
            job["options"]["directory_mode"] = json!(mode);
        }
        if timestamp_files
            && let Some(key) = single_file_key
            && !output.contains(TIMESTAMP_TOKEN)
            && !output.contains(SJA_TIMESTAMP_TOKEN)
        {
            let path = Path::new(output);
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or("请指定有效的备份输出文件名。")?;
            let path = path.with_file_name(format!("{SJA_TIMESTAMP_TOKEN}_{name}"));
            job[key] = json!(path);
        }
        if append == Some(true) && single_file_key.is_some() {
            job["options"]["append"] = json!(true);
        }
        Ok((
            json!({"version":1,"connection":connection,"job":job}),
            notes,
        ))
    }
}

fn script_flag(value: &Option<String>) -> Result<bool, String> {
    if value.as_deref().is_none_or(|value| value.trim().is_empty()) {
        return Ok(false);
    }
    Ok(flag(value)?.unwrap_or(false))
}

#[cfg(test)]
mod replication_tests {
    use super::*;

    #[test]
    fn migrates_the_observed_replication_lock_on_request() {
        let executable = std::env::current_exe().unwrap();
        let dir = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let source = dir.path().join("old.xml");
        let target = dir.path().join("backup.json");
        let xml = r#"<job><exportjob>
            <source><host>127.0.0.1</host><user>backup_user</user></source>
            <export><databases all="yes"/></export>
            <options><locktableread>yes</locktableread><flushslave>yes</flushslave></options>
        </exportjob></job>"#;
        fs::write(&source, xml).unwrap();
        let args = ["dump", "--passwordless", REPLICATION_LOCK_ARG].map(OsString::from);
        let (native, notes) = prepare(&source, &target, &args).unwrap();
        let options = &native["job"]["options"];
        assert_eq!(options["lock_source_server"], true);
        assert_eq!(options["lock_source_tables"], false);
        assert!(
            notes
                .iter()
                .any(|note| note.contains("FLUSH TABLES WITH READ LOCK"))
        );
        assert_eq!(fs::read_to_string(source).unwrap(), xml);
    }
}

impl LegacySchema {
    fn native_selection(&self) -> Result<Value, String> {
        let mut selection = json!({"schema":self.name});
        if !flag(&self.objects.all)?.unwrap_or(false) {
            let (kinds, objects) = self.objects.selection()?;
            selection["object_kinds"] = json!(kinds);
            selection["objects"] = json!(objects);
        }
        Ok(selection)
    }
}

impl LegacyObjects {
    fn selection(&self) -> Result<(Vec<&'static str>, Vec<Value>), String> {
        let mut kinds = Vec::new();
        let mut objects = Vec::new();
        for (kind, group) in [
            ("table", &self.tables),
            ("view", &self.views),
            ("procedure", &self.procedures),
            ("function", &self.functions),
            ("trigger", &self.triggers),
            ("event", &self.events),
        ] {
            let Some(group) = group else {
                continue;
            };
            if flag(&group.all)?.unwrap_or(false) {
                kinds.push(kind);
            } else {
                for object in &group.objects {
                    let (member_kind, value) = object.parts();
                    let name = value
                        .filter(|name| !name.is_empty())
                        .ok_or("旧版对象名称不能为空。")?;
                    if member_kind != kind {
                        return Err("旧版对象类型与所在分组不匹配。".into());
                    }
                    objects.push(json!({"kind":kind,"name":name}));
                }
            }
        }
        Ok((kinds, objects))
    }
}

pub(super) fn flag(value: &Option<String>) -> Result<Option<bool>, String> {
    value
        .as_ref()
        .map(|value| match value.trim().to_ascii_lowercase().as_str() {
            "yes" | "true" | "1" => Ok(true),
            "no" | "false" | "0" => Ok(false),
            _ => Err("旧版开关应使用 yes、no、true、false、1 或 0。".into()),
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_legacy_split_backup_directory_modes() {
        let executable = std::env::current_exe().unwrap();
        let directory = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let output = directory.path().join("dump");
        let timestamp = 1_780_000_000_000;
        for attribute in [r#" timestampdir="yes""#, r#" timestampdir="no""#, ""] {
            let script =
                format!("<script fileordir=\"dir\" timestamp=\"\"{attribute}>old</script>");
            let single = format!(
                "<databases all=\"no\"><database name=\"demo\">\
                 <objects all=\"yes\">{script}</objects></database></databases>"
            );
            let multi = format!("<databases all=\"yes\"/>{script}");
            for export in [single, multi] {
                for (append, expected) in [
                    ("", (false, false)),
                    ("<appendtofile>yes</appendtofile>", (false, true)),
                    ("<appendtofile>no</appendtofile>", (true, false)),
                ] {
                    let xml = format!(
                        "<exportjob><source><host>localhost</host><user>reader</user></source>\
                         <export>{export}</export><options>{append}</options></exportjob>"
                    );
                    let legacy: LegacyBackup = quick_xml::de::from_str(&xml).unwrap();
                    let (definition, _) = legacy.to_native("dump", None).unwrap();
                    let definition: JobFile = serde_json::from_value(definition).unwrap();
                    let resolved = definition
                        .job
                        .output_path(directory.path(), timestamp)
                        .unwrap();
                    let options = match &definition.job {
                        Job::Backup(job) => {
                            job.validate().unwrap();
                            &job.options
                        }
                        Job::BackupDatabases(job) => {
                            job.validate().unwrap();
                            &job.options
                        }
                        _ => panic!("expected a backup job"),
                    };
                    if attribute.contains("yes") {
                        assert_eq!(
                            resolved,
                            output.join(super::super::sja_timestamp(timestamp))
                        );
                        assert_ne!(
                            definition
                                .job
                                .output_path(directory.path(), timestamp + 1000),
                            Some(resolved)
                        );
                        assert_eq!((options.overwrite, options.append), (false, false));
                    } else {
                        assert_eq!(resolved, output);
                        assert_eq!((options.overwrite, options.append), expected);
                    }
                }
            }
        }
    }

    #[test]
    fn preserves_legacy_filename_timestamps_for_new_parts_in_the_selected_directory() {
        let xml = r#"<exportjob><source><host>localhost</host><user>reader</user></source>
            <export><databases all="yes"/>
            <script fileordir="dir" timestampdir="no" timestamp="yes">old</script>
            </export><options><appendtofile>no</appendtofile></options></exportjob>"#;
        let legacy: LegacyBackup = quick_xml::de::from_str(xml).unwrap();
        let (native, _) = legacy.to_native("dump", None).unwrap();
        let parsed: JobFile = serde_json::from_value(native).unwrap();
        let Job::BackupDatabases(job) = parsed.job else {
            panic!("expected a multi-database backup job");
        };
        job.validate().unwrap();
        assert_eq!(job.options.directory_mode, BackupDirectoryMode::Selected);
        assert!(job.options.split_filename_timestamp);
        assert!(job.options.append);
        assert!(!job.options.overwrite);
    }

    #[test]
    fn converts_legacy_backup_file_charsets_to_explicit_job_settings() {
        for (setting, expected) in [
            ("", SqlDumpCharset::Utf8Mb4),
            ("<usecharset>utf8</usecharset>", SqlDumpCharset::Utf8Mb3),
            ("<usecharset>yes</usecharset>", SqlDumpCharset::Utf8Mb3),
            ("<usecharset>latin2</usecharset>", SqlDumpCharset::Latin2),
            (
                "<usecharset> GB18030 </usecharset>",
                SqlDumpCharset::Gb18030,
            ),
        ] {
            let xml = format!(
                "<exportjob><source><host>localhost</host><user>reader</user></source>\
                 <export><databases all=\"yes\"/></export><options>{setting}</options></exportjob>"
            );
            let legacy: LegacyBackup = quick_xml::de::from_str(&xml).unwrap();
            let (definition, _) = legacy.to_native("backup", None).unwrap();
            let definition: JobFile = serde_json::from_value(definition).unwrap();
            let Job::BackupDatabases(job) = definition.job else {
                panic!("expected a multi-database backup job");
            };
            assert_eq!(job.options.file_charset, expected);
        }
    }

    #[test]
    fn converts_legacy_backup_error_handling_for_objects_and_databases() {
        let selected = r#"<databases all="no"><database name="demo"><objects all="yes"/>
                          </database></databases>"#;
        let continue_on_error = "<abortonerror>no</abortonerror>";
        let stop_on_error = "<abortonerror>yes</abortonerror>";
        for (scope, policy, expected, multiple) in [
            (selected, continue_on_error, true, false),
            (selected, stop_on_error, false, false),
            (selected, "", false, false),
            (r#"<databases all="yes"/>"#, continue_on_error, true, true),
        ] {
            let xml = format!(
                "<exportjob><source><host>localhost</host><user>reader</user></source>\
                 <export>{scope}</export>{policy}</exportjob>"
            );
            let legacy: LegacyBackup = quick_xml::de::from_str(&xml).unwrap();
            let (definition, _) = legacy.to_native("backup", None).unwrap();
            let definition: JobFile = serde_json::from_value(definition).unwrap();
            let policy = match definition.job {
                Job::Backup(job) => (job.options.continue_on_error, None),
                Job::BackupDatabases(job) => {
                    (job.options.continue_on_error, Some(job.continue_on_error))
                }
                _ => panic!("expected a backup job"),
            };
            assert_eq!(policy, (expected, multiple.then_some(expected)));
        }
    }

    #[test]
    fn preserves_legacy_backup_batch_defaults_and_explicit_sizes() {
        let cases = [
            ("", Some(1000), None),
            (
                "<bulksize/><chunkinsert>yes</chunkinsert><chunklimit/>",
                Some(1000),
                Some(1000),
            ),
            (
                "<bulksize>0</bulksize><chunkinsert>yes</chunkinsert>",
                None,
                Some(1000),
            ),
            (
                "<bulksize>1</bulksize><chunkinsert>yes</chunkinsert><chunklimit>2000</chunklimit>",
                Some(1),
                Some(2000),
            ),
            (
                "<bulksize>32</bulksize><chunkinsert>no</chunkinsert><chunklimit>2000</chunklimit>",
                Some(32),
                None,
            ),
        ];
        for (settings, bulk, rows) in cases {
            let xml = format!(
                "<exportjob><source><host>localhost</host><user>reader</user></source>\
                 <export><databases all=\"no\"><database name=\"demo\"><objects all=\"yes\"/>\
                 </database></databases></export><options>{settings}</options></exportjob>"
            );
            let job: LegacyBackup = quick_xml::de::from_str(&xml).unwrap();
            let (definition, notes) = job.to_native("backup.sql", None).unwrap();
            let options = &definition["job"]["options"];
            assert_eq!(options["bulk_insert_limit_kib"], json!(bulk));
            assert_eq!(options["chunk_rows"], json!(rows));
            assert!(notes.is_empty());
        }
    }
}
