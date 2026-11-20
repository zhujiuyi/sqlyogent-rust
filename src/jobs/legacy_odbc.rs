use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::database::{OdbcColumnDefinition, OdbcConflict, OdbcReset};
use crate::file_source::{self, FileSource, FileSourceFormat};
use crate::odbc;

use super::legacy::{flag, parse_xml, save_definition};
use super::legacy_report::{self, LegacyReport, ReportArgs, ReportInfo};
use super::odbc_import::{ExistingTarget, MissingTarget, TableStrategy};
use super::{Job, JobFile};

pub(super) const USAGE: &str = "import-legacy-odbc <旧导入.xml> <新作业.json> --create-new|--append-existing|--truncate-existing|--recreate-existing|--use-legacy-table-strategies --target-password-env <变量名>|--target-passwordless [--use-legacy-conflict] [--copy-source-indexes] [--copy-source-foreign-keys] [--preserve-source-actions] [--source-user-env <变量名>] [--source-password-env <变量名>] [--source-connection-env <变量名>] [--source-catalog <目录>] [--source-schema <架构>] [--csv-source <CSV 文件或目录>|--excel-source <Excel 工作簿>|--access-source <MDB 或 ACCDB 文件> [--access-password-env <变量名>]]";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OdbcScope {
    MappedTables,
    AllTables,
    MissingMappings,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct Details {
    pub requires_connection_string: bool,
    pub scope: OdbcScope,
    pub file_format: Option<FileSourceFormat>,
    pub mapped_file_path: Option<PathBuf>,
}

#[derive(Default)]
struct Options<'a> {
    create: Option<bool>,
    reset: OdbcReset,
    use_legacy_table_strategies: bool,
    use_legacy_conflict: bool,
    copy_source_indexes: bool,
    copy_source_foreign_keys: bool,
    preserve_source_actions: bool,
    target_password: Option<Option<&'a str>>,
    source_user: Option<&'a str>,
    source_password: Option<&'a str>,
    source_connection: Option<&'a str>,
    source_catalog: Option<&'a str>,
    source_schema: Option<&'a str>,
    file_source: Option<(FileSourceFormat, &'a Path)>,
    access_password_env: Option<&'a str>,
    report: ReportArgs<'a>,
}

impl Options<'_> {
    fn creates_tables(&self) -> bool {
        self.create == Some(true) || self.use_legacy_table_strategies
    }
}

fn options(mut args: &[OsString]) -> Result<Options<'_>, String> {
    let mut options = Options::default();
    while let Some((key, tail)) = args.split_first() {
        if let Some(rest) = options.report.consume(key, tail)? {
            args = rest;
            continue;
        }
        match key.to_str().ok_or(USAGE)? {
            "--create-new" | "--append-existing" if options.create.is_none() => {
                options.create = Some(key == "--create-new");
                args = tail;
            }
            "--truncate-existing" if options.create.is_none() => {
                options.create = Some(false);
                options.reset = OdbcReset::Truncate;
                args = tail;
            }
            "--recreate-existing" if options.create.is_none() => {
                options.create = Some(true);
                options.reset = OdbcReset::Recreate;
                args = tail;
            }
            "--use-legacy-table-strategies" if !options.use_legacy_table_strategies => {
                options.use_legacy_table_strategies = true;
                args = tail;
            }
            "--target-passwordless" if options.target_password.is_none() => {
                options.target_password = Some(None);
                args = tail;
            }
            "--use-legacy-conflict" if !options.use_legacy_conflict => {
                options.use_legacy_conflict = true;
                args = tail;
            }
            "--copy-source-indexes" if !options.copy_source_indexes => {
                options.copy_source_indexes = true;
                args = tail;
            }
            "--copy-source-foreign-keys" if !options.copy_source_foreign_keys => {
                options.copy_source_foreign_keys = true;
                args = tail;
            }
            "--preserve-source-actions" if !options.preserve_source_actions => {
                options.preserve_source_actions = true;
                args = tail;
            }
            key @ ("--csv-source" | "--excel-source" | "--access-source")
                if options.file_source.is_none() =>
            {
                let [path, rest @ ..] = tail else {
                    return Err(USAGE.into());
                };
                let format = match key {
                    "--csv-source" => FileSourceFormat::Csv,
                    "--excel-source" => FileSourceFormat::Excel,
                    _ => FileSourceFormat::Access,
                };
                let path = path
                    .to_str()
                    .filter(|path| {
                        !path.trim().is_empty() && !path.starts_with("--") && !path.contains('\0')
                    })
                    .ok_or_else(|| format!("请为 {key} 指定有效的来源路径。"))?;
                options.file_source = Some((format, Path::new(path)));
                args = rest;
            }
            key @ ("--source-catalog" | "--source-schema") => {
                let [name, rest @ ..] = tail else {
                    return Err(USAGE.into());
                };
                let name = name
                    .to_str()
                    .filter(|name| {
                        !name.trim().is_empty()
                            && !name.starts_with("--")
                            && !name.chars().any(char::is_control)
                    })
                    .ok_or("请填写有效的 ODBC 源目录或架构名称。")?;
                match key {
                    "--source-catalog" if options.source_catalog.is_none() => {
                        options.source_catalog = Some(name)
                    }
                    "--source-schema" if options.source_schema.is_none() => {
                        options.source_schema = Some(name)
                    }
                    _ => return Err(USAGE.into()),
                }
                args = rest;
            }
            key => {
                let [name, rest @ ..] = tail else {
                    return Err(USAGE.into());
                };
                let name = name
                    .to_str()
                    .filter(|name| {
                        !name.trim().is_empty()
                            && !name.starts_with("--")
                            && !name.contains('=')
                            && !name.chars().any(char::is_control)
                    })
                    .ok_or("请填写有效的环境变量名。")?;
                match key {
                    "--target-password-env" if options.target_password.is_none() => {
                        options.target_password = Some(Some(name))
                    }
                    "--source-user-env" if options.source_user.is_none() => {
                        options.source_user = Some(name)
                    }
                    "--source-password-env" if options.source_password.is_none() => {
                        options.source_password = Some(name)
                    }
                    "--source-connection-env" if options.source_connection.is_none() => {
                        options.source_connection = Some(name)
                    }
                    "--access-password-env" if options.access_password_env.is_none() => {
                        options.access_password_env = Some(name)
                    }
                    _ => return Err(USAGE.into()),
                }
                args = rest;
            }
        }
    }
    if options.target_password.is_none()
        || options.create.is_some() == options.use_legacy_table_strategies
        || options.source_connection.is_some()
            && (options.source_user.is_some() || options.source_password.is_some())
    {
        return Err(USAGE.into());
    }
    if (options.copy_source_indexes || options.copy_source_foreign_keys)
        && !options.creates_tables()
    {
        return Err("复制源索引或外键需要允许创建或重建目标表。".into());
    }
    Ok(options)
}

pub(super) fn import(args: &[OsString]) -> Result<(), String> {
    let [input, output, args @ ..] = args else {
        return Err(USAGE.into());
    };
    let input = Path::new(input);
    let output =
        std::path::absolute(output).map_err(|error| format!("无法定位新作业文件：{error}"))?;
    let (definition, notes) = prepare(input, args)?;
    save_definition(input, &output, &definition)?;
    if definition["job"]["discovery"].is_object() {
        println!("已转换为运行时发现源表的 ODBC 作业。{}", output.display());
    } else {
        println!(
            "已转换 {} 张表的 ODBC 作业。{}",
            job_count(&definition),
            output.display()
        );
    }
    for note in notes {
        eprintln!("{note}");
    }
    Ok(())
}

pub(super) fn prepare(input: &Path, args: &[OsString]) -> Result<(Value, Vec<String>), String> {
    let (definition, notes) = read(input)?.convert(options(args)?, input)?;
    let parsed: JobFile =
        serde_json::from_value(definition.clone()).map_err(|_| "无法生成有效的 ODBC 作业。")?;
    parsed.connection.validate()?;
    let Job::OdbcImport(job) = &parsed.job else {
        return Err("旧版 ODBC 转换生成了错误的作业类型。".into());
    };
    job.validate()?;
    Ok((definition, notes))
}

pub(super) fn details(input: &Path) -> Result<Details, String> {
    let job = read(input)?;
    let scope = job.tables.scope()?;
    let file_format = if scope != OdbcScope::MissingMappings {
        job.dsn.file_format()
    } else {
        None
    };
    let mapped_file_path = if scope == OdbcScope::MappedTables && file_format.is_some() {
        job.dsn.file_path(input)?
    } else {
        None
    };
    Ok(Details {
        requires_connection_string: !job.dsn.is_named() && !job.dsn.is_file_dsn(),
        scope,
        file_format,
        mapped_file_path,
    })
}

pub(super) fn report_info(input: &Path) -> Result<Option<ReportInfo>, String> {
    legacy_report::info(read(input)?.sendreport.as_ref())
}

fn read(input: &Path) -> Result<LegacyJob, String> {
    let file: LegacyFile = parse_xml(input)?;
    if file.exportjob.is_some() || file.syncjob.is_some() || file.notifyjob.is_some() {
        return Err("请选择只包含 ODBC 导入的旧作业。".into());
    }
    let Ok([job]) = <[_; 1]>::try_from(file.jobs) else {
        return Err("请选择包含一个 odbcimport 的旧版 ODBC 作业。".into());
    };
    Ok(job)
}

fn job_count(definition: &Value) -> usize {
    definition["job"]["tables"].as_array().map_or(0, Vec::len)
}

#[derive(Deserialize)]
struct LegacyFile {
    #[serde(rename = "odbcimport", default)]
    jobs: Vec<LegacyJob>,
    exportjob: Option<serde::de::IgnoredAny>,
    syncjob: Option<serde::de::IgnoredAny>,
    notifyjob: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct LegacyJob {
    // SJA names the destination MySQL connection "source".
    source: LegacyTarget,
    dsn: LegacyDsn,
    import: LegacyImport,
    export: LegacyExport,
    tables: LegacyTables,
    abortonerror: Option<String>,
    sendreport: Option<LegacyReport>,
}

#[derive(Deserialize)]
struct LegacyTarget {
    host: String,
    user: String,
    port: Option<u16>,
    database: String,
    ssl: Option<String>,
    sslauth: Option<String>,
    clientkey: Option<String>,
    clientcert: Option<String>,
    cacert: Option<String>,
    cipher: Option<String>,
    ssh: Option<serde::de::IgnoredAny>,
    http: Option<serde::de::IgnoredAny>,
    tunnel: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct LegacyDsn {
    #[serde(rename = "@type")]
    kind: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct LegacyImport {
    #[serde(rename = "@source")]
    mode: String,
}

#[derive(Deserialize)]
struct LegacyExport {
    #[serde(rename = "@target")]
    mode: String,
}

#[derive(Deserialize)]
struct LegacyTables {
    #[serde(rename = "@all")]
    all: Option<String>,
    #[serde(rename = "table", default)]
    items: Vec<LegacyTable>,
}

impl LegacyTables {
    fn scope(&self) -> Result<OdbcScope, String> {
        match (flag(&self.all)?.unwrap_or(false), self.items.is_empty()) {
            (true, true) => Ok(OdbcScope::AllTables),
            (true, false) => {
                Err("旧作业同时选择了全部表和逐表映射。请检查 XML 中的导入范围。".into())
            }
            (false, false) => Ok(OdbcScope::MappedTables),
            (false, true) => Ok(OdbcScope::MissingMappings),
        }
    }
}

#[derive(Deserialize)]
struct LegacyTable {
    #[serde(rename = "@source")]
    source: String,
    #[serde(rename = "@target")]
    target: String,
    #[serde(rename = "@type")]
    engine: Option<String>,
    #[serde(rename = "@index")]
    index: Option<String>,
    #[serde(rename = "@foreignkey")]
    foreign_key: Option<String>,
    ddl: Option<LegacyDdl>,
    dml: Option<LegacyDml>,
    triggers: Option<LegacyTriggers>,
    whereclause: Option<String>,
    columns: LegacyColumns,
}

#[derive(Deserialize)]
struct LegacyDdl {
    #[serde(rename = "@onconflict")]
    conflict: Option<String>,
}

#[derive(Deserialize)]
struct LegacyDml {
    #[serde(rename = "@import")]
    import: Option<String>,
    #[serde(rename = "@onconflict")]
    conflict: Option<String>,
}

#[derive(Deserialize)]
struct LegacyTriggers {
    delete: Option<String>,
    updates: Option<LegacyUpdates>,
}

#[derive(Deserialize)]
struct LegacyUpdates {
    #[serde(rename = "update", default)]
    items: Vec<LegacyUpdate>,
}

#[derive(Deserialize)]
struct LegacyUpdate {
    #[serde(rename = "@srccol")]
    column: Option<String>,
    #[serde(rename = "@value")]
    expression: Option<String>,
}

#[derive(Deserialize)]
struct LegacyColumns {
    #[serde(rename = "column", default)]
    items: Vec<LegacyColumn>,
}

#[derive(Deserialize)]
struct LegacyColumn {
    source: String,
    target: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    length: Option<String>,
    default: Option<String>,
    primary: Option<String>,
    binary: Option<String>,
    notnull: Option<String>,
    unsigned: Option<String>,
    autoincr: Option<String>,
    zerofill: Option<String>,
}

#[derive(Default)]
struct Notices {
    recreate: bool,
    duplicate_action: bool,
    indexes: bool,
    foreign_keys: bool,
    source_action: bool,
    automatic_default: bool,
}

impl LegacyJob {
    fn convert(&self, options: Options<'_>, input: &Path) -> Result<(Value, Vec<String>), String> {
        let create = options.create.unwrap_or(false);
        let source = self.target_connection(options.target_password.flatten())?;
        if !self.import.mode.eq_ignore_ascii_case("data")
            || !self.export.mode.eq_ignore_ascii_case("data")
        {
            return Err(
                "这个旧作业的源或目标模式需要手工迁移。可在原生 ODBC 作业中填写源查询和目标列。"
                    .into(),
            );
        }
        let scope = self.tables.scope()?;
        if scope == OdbcScope::MissingMappings {
            return Err(
                "旧作业未指定逐表列映射。请用原生 ODBC 作业填写要导入的表、查询和列映射。".into(),
            );
        }
        if scope == OdbcScope::AllTables && options.use_legacy_table_strategies {
            return Err("这个作业在运行时发现源表。请为所有目标表选择统一写入方式。".into());
        }
        if scope != OdbcScope::AllTables
            && (options.source_catalog.is_some() || options.source_schema.is_some())
        {
            return Err("源目录和源架构筛选仅用于运行时发现全部表的作业。".into());
        }

        let file = self.dsn.direct_file(input, scope, &options)?;
        let source_definition = if let Some(file) = &file {
            json!({"file_source": file})
        } else {
            self.dsn.connection(&options, input)?
        };
        let mut notices = Notices::default();
        let mut notes = match file.as_ref().map(|file| file.format) {
            Some(FileSourceFormat::Csv) => {
                vec!["CSV 来源默认按 UTF-8 读取，以逗号分隔，首行作为表头。".into()]
            }
            Some(FileSourceFormat::Excel) => vec!["Excel 来源默认将首行作为表头。".into()],
            Some(FileSourceFormat::Access) => vec![],
            None => vec!["连接密码和 ODBC 凭据由运行时环境变量提供。".into()],
        };
        if let Some(path) = source_definition["file_dsn"].as_str() {
            notes.push(format!("沿用文件 DSN {path}。"));
        }
        let mut job = json!({
            "type": "odbc_import",
            "schema": self.source.database,
            "create_table": create,
            "reset": options.reset,
        });
        if scope == OdbcScope::AllTables {
            if options.preserve_source_actions {
                return Err("保留旧版源表更新或删除需要逐表映射。".into());
            }
            let mut discovery = json!({
                "source": source_definition,
                "copy_indexes": options.copy_source_indexes || options.copy_source_foreign_keys,
                "copy_foreign_keys": options.copy_source_foreign_keys,
            });
            if let Some(catalog) = options.source_catalog {
                discovery["catalog"] = json!(catalog);
                notes.push(format!("源目录 {catalog}。"));
            }
            if let Some(schema) = options.source_schema {
                discovery["schema"] = json!(schema);
                notes.push(format!("源架构 {schema}。"));
            }
            notes.push("作业每次运行时会读取所选范围内的源表，并按表名和列名建立映射。".into());
            job["discovery"] = discovery;
        } else {
            let tables = self
                .tables
                .items
                .iter()
                .map(|table| {
                    table.convert(&source_definition, file.as_ref(), &options, &mut notices)
                })
                .collect::<Result<Vec<_>, _>>()?;
            if options.use_legacy_table_strategies {
                for table in &tables {
                    let structure_only = table["strategy"]["structure_only"] == true;
                    let action = match (table["strategy"]["if_exists"].as_str(), structure_only) {
                        (Some("recreate"), true) => "重建表结构",
                        (Some("recreate"), false) => "重建并导入数据",
                        (Some("append"), false) => "保留表结构并追加数据",
                        _ => "保留原表并跳过",
                    };
                    let missing = if structure_only {
                        "只创建表结构"
                    } else {
                        "创建并导入数据"
                    };
                    let name = table["table"].as_str().unwrap_or_default();
                    notes.push(format!(
                        "目标表「{name}」不存在时{missing}，已存在时{action}。"
                    ));
                }
            }
            job["tables"] = json!(tables);
        }
        if notices.recreate
            && !options.use_legacy_table_strategies
            && options.reset != OdbcReset::Recreate
        {
            notes.push(
                match (create, options.reset) {
                    (true, _) => "旧版删除重建已改为创建新表。目标表存在时，本次导入结束。",
                    (false, OdbcReset::Truncate) => {
                        "旧版删除重建已改为清空后导入，保留现有表结构。"
                    }
                    (false, _) => "旧版删除重建已改为向已有表追加数据。",
                }
                .into(),
            );
        }
        if notices.duplicate_action {
            notes.push("键冲突会结束本次导入。可在转换时选择保留旧版逐表重复键策略。".into());
        }
        if notices.indexes {
            notes.push("选择创建新表并复制源索引，可读取旧作业勾选的源索引。".into());
        }
        if notices.foreign_keys {
            notes.push(
                "可在转换时选择复制旧作业勾选的外键。请在同一作业中导入被引用表及其索引。".into(),
            );
        }
        if notices.source_action {
            notes.push(if file.is_some() {
                "旧作业包含源表更新或删除。通过 ODBC 连接字符串迁移时可选择保留这些操作。".into()
            } else {
                "可以在转换时选择保留旧作业的源表更新或删除。".into()
            });
        }
        if job["tables"].as_array().is_some_and(|tables| {
            tables
                .iter()
                .any(|table| table["source"]["after_import"].is_object())
        }) {
            notes.push("新作业运行时会在目标写入确认后更新或删除对应的源记录。".into());
        }
        if notices.automatic_default {
            notes.push("自增列的默认值由目标库的自增规则处理。".into());
        }
        if flag(&self.abortonerror)? == Some(false) {
            job["continue_on_error"] = json!(true);
            notes.push(
                "保留旧作业遇错继续的设置。有关联外键的表一起提交，各表结果记录在作业日志中。"
                    .into(),
            );
        }
        let mut definition = json!({
            "version": 1,
            "connection": source,
            "job": job,
        });
        legacy_report::migrate(
            self.sendreport.as_ref(),
            options.report,
            &[
                options.target_password.flatten(),
                options.source_user,
                options.source_password,
                options.source_connection,
                options.access_password_env,
            ],
            &mut definition,
            &mut notes,
        )?;
        Ok((definition, notes))
    }

    fn target_connection(&self, password_env: Option<&str>) -> Result<Value, String> {
        let source = &self.source;
        if source.ssh.is_some()
            || source.http.is_some()
            || source.tunnel.is_some()
            || flag(&source.sslauth)?.unwrap_or(false)
            || [
                &source.clientkey,
                &source.clientcert,
                &source.cacert,
                &source.cipher,
            ]
            .into_iter()
            .flatten()
            .any(|setting| !setting.trim().is_empty())
        {
            return Err("旧目标连接配置了隧道或指定了 TLS 证书或加密参数。请在原生作业中填写对应的连接配置。".into());
        }
        let mut connection = json!({
            "host": source.host,
            "port": source.port.unwrap_or(3306),
            "user": source.user,
            "password_env": password_env,
        });
        if flag(&source.ssl)?.unwrap_or(false) {
            connection["tls"] = json!({});
        }
        Ok(connection)
    }
}

impl LegacyDsn {
    fn file_format(&self) -> Option<FileSourceFormat> {
        let kind = self.kind.as_deref()?;
        if kind.eq_ignore_ascii_case("csv") {
            Some(FileSourceFormat::Csv)
        } else if kind.eq_ignore_ascii_case("excel") {
            Some(FileSourceFormat::Excel)
        } else if kind.eq_ignore_ascii_case("access") {
            Some(FileSourceFormat::Access)
        } else {
            None
        }
    }

    fn is_named(&self) -> bool {
        self.kind.as_deref().is_none_or(|kind| {
            kind.eq_ignore_ascii_case("system") || kind.eq_ignore_ascii_case("user")
        })
    }

    fn is_file_dsn(&self) -> bool {
        self.kind
            .as_deref()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("file"))
    }

    fn file_path(&self, input: &Path) -> Result<Option<PathBuf>, String> {
        let Some(name) = self
            .name
            .as_deref()
            .filter(|name| !name.trim().is_empty() && !name.chars().any(char::is_control))
        else {
            return Ok(None);
        };
        let xml =
            std::path::absolute(input).map_err(|error| format!("无法定位旧 ODBC 作业。{error}"))?;
        let directory = xml.parent().ok_or("无法定位旧作业的目录。")?;
        Ok(Some(directory.join(name)))
    }

    fn direct_file(
        &self,
        input: &Path,
        scope: OdbcScope,
        options: &Options<'_>,
    ) -> Result<Option<FileSource>, String> {
        let original_format = self.file_format();
        let chosen = if let Some((format, path)) = options.file_source {
            if original_format != Some(format) {
                return Err("请指定与旧作业来源相同的文件类型。".into());
            }
            let path =
                std::path::absolute(path).map_err(|error| format!("无法定位来源文件。{error}"))?;
            Some((format, path))
        } else if let Some(format) = original_format.filter(|_| options.source_connection.is_none())
        {
            if scope == OdbcScope::MappedTables
                && let Some(path) = self.file_path(input)?
            {
                Some((format, path))
            } else {
                let (option, source) = match format {
                    FileSourceFormat::Csv => ("--csv-source", "CSV 文件或目录"),
                    FileSourceFormat::Excel => ("--excel-source", "Excel 工作簿"),
                    FileSourceFormat::Access => ("--access-source", "Access 文件"),
                };
                return Err(format!(
                    "请用 {option} 指定{source}，或用 --source-connection-env 提供 ODBC 连接字符串。"
                ));
            }
        } else {
            None
        };
        if options.access_password_env.is_some()
            && !matches!(chosen, Some((FileSourceFormat::Access, _)))
        {
            return Err("--access-password-env 需要直接读取 Access 文件。".into());
        }
        let Some((format, path)) = chosen else {
            return Ok(None);
        };
        if options.source_connection.is_some()
            || options.source_user.is_some()
            || options.source_password.is_some()
            || options.source_catalog.is_some()
            || options.source_schema.is_some()
        {
            return Err("指定文件来源时，请清除 ODBC 连接变量、目录和架构筛选。".into());
        }
        let file = FileSource {
            format,
            path,
            access_password_env: options.access_password_env.map(str::to_owned),
            ..FileSource::default()
        };
        if !file.supports_metadata()
            && (options.copy_source_indexes || options.copy_source_foreign_keys)
        {
            return Err("CSV 和 Excel 文件没有可复制的数据库索引或外键。".into());
        }
        Ok(Some(file))
    }

    fn connection(&self, options: &Options<'_>, input: &Path) -> Result<Value, String> {
        if let Some(name) = options.source_connection {
            return Ok(json!({"connection_env": name}));
        }
        if self.is_file_dsn() {
            let path = self
                .name
                .as_deref()
                .map(Path::new)
                .ok_or("旧作业缺少文件 DSN 路径。")?;
            odbc::validate_file_dsn_path(path)?;
            return Ok(json!({
                "file_dsn": self.file_path(input)?.ok_or("旧作业缺少文件 DSN 路径。")?,
                "user_env": options.source_user,
                "password_env": options.source_password
            }));
        }
        if !self.is_named() {
            return Err(
                "此旧作业使用文件数据源。请用 --source-connection-env 提供运行时连接字符串。"
                    .into(),
            );
        }
        let dsn = self.name.as_deref().ok_or("旧作业缺少 ODBC DSN 名称。")?;
        odbc::validate_dsn(dsn)?;
        Ok(
            json!({"dsn": dsn, "user_env": options.source_user, "password_env": options.source_password}),
        )
    }
}

fn legacy_csv_name<'a>(
    old_name: &str,
    candidates: impl IntoIterator<Item = &'a str>,
    kind: &str,
) -> Result<&'a str, String> {
    // The Microsoft Text driver encodes embedded periods as '#' in table and column names.
    let mut matches = candidates
        .into_iter()
        .filter(|candidate| *candidate == old_name || candidate.replace('.', "#") == old_name);
    let name = matches
        .next()
        .ok_or_else(|| format!("在所选 CSV 来源中找不到旧作业的{kind}「{old_name}」。"))?;
    if matches.next().is_some() {
        return Err(format!("旧 CSV {kind}名「{old_name}」对应多个名称。"));
    }
    Ok(name)
}

fn resolve_legacy_csv_columns(
    file: &FileSource,
    table: &str,
    columns: &mut [Value],
) -> Result<(), String> {
    if !columns.iter().any(|column| {
        column["source"]
            .as_str()
            .is_some_and(|name| name.contains('#'))
    }) {
        return Ok(());
    }
    let headers = file_source::read(file, table, true)?.columns;
    for column in columns {
        let Some(name) = column["source"].as_str().filter(|name| name.contains('#')) else {
            continue;
        };
        let name = legacy_csv_name(
            name,
            headers.iter().map(|header| header.name.as_str()),
            "字段",
        )?;
        column["source"] = json!(name);
    }
    Ok(())
}

impl LegacyTable {
    fn existing_target_strategy(&self) -> Result<ExistingTarget, String> {
        let value = self
            .ddl
            .as_ref()
            .and_then(|ddl| ddl.conflict.as_deref())
            .map(str::trim);
        match value.map(str::to_ascii_lowercase).as_deref() {
            Some("recreate") => Ok(ExistingTarget::Recreate),
            Some("import") => Ok(ExistingTarget::Append),
            Some("abort") => Ok(ExistingTarget::Skip),
            _ => Err(format!(
                "旧目标表「{}」缺少已确认的已有表策略。请指定统一写入方式。",
                self.target
            )),
        }
    }

    fn file_table(&self, file: &FileSource) -> Result<(FileSource, String), String> {
        let mut file = file.clone();
        let table = match file.format {
            FileSourceFormat::Csv => {
                let name = self
                    .source
                    .rsplit_once('.')
                    .filter(|(_, extension)| extension.eq_ignore_ascii_case("csv"))
                    .map_or(self.source.as_str(), |(name, _)| name);
                if name.contains('#') {
                    let names = file_source::discover(&file)?;
                    legacy_csv_name(name, names.iter().map(String::as_str), "表")?.to_owned()
                } else {
                    name.to_owned()
                }
            }
            FileSourceFormat::Excel => {
                let quoted = self
                    .source
                    .strip_prefix('\'')
                    .and_then(|name| name.strip_suffix('\''))
                    .map(|name| name.replace("''", "'"));
                let name = quoted.as_deref().unwrap_or(&self.source);
                if let Some(sheet) = name.strip_suffix('$') {
                    sheet.to_owned()
                } else {
                    file.excel_named_ranges = true;
                    name.rsplit_once('$').map_or_else(
                        || name.to_owned(),
                        |(sheet, range)| format!("{sheet}!{range}"),
                    )
                }
            }
            FileSourceFormat::Access => self.source.clone(),
        };
        Ok((file, table))
    }

    fn convert(
        &self,
        credentials: &Value,
        file: Option<&FileSource>,
        options: &Options<'_>,
        notices: &mut Notices,
    ) -> Result<Value, String> {
        let create = options.creates_tables();
        let strategy = options
            .use_legacy_table_strategies
            .then(|| self.existing_target_strategy())
            .transpose()?;
        if file.is_none() && !self.source.split('.').all(plain_identifier) {
            return Err("旧版源表名需要按 ODBC 驱动的规则引用。请在原生作业中填写 SELECT。".into());
        }
        let mut data = true;
        let mut conflict = OdbcConflict::Abort;
        if let Some(ddl) = &self.ddl {
            notices.recreate |= ddl
                .conflict
                .as_deref()
                .is_some_and(|action| action.eq_ignore_ascii_case("recreate"));
        }
        if let Some(dml) = &self.dml {
            data = flag(&dml.import)?.unwrap_or(true);
            if let Some(action) = dml
                .conflict
                .as_deref()
                .map(str::trim)
                .filter(|action| !action.is_empty())
            {
                if options.use_legacy_conflict {
                    conflict = match action.to_ascii_lowercase().as_str() {
                        "abort" => OdbcConflict::Abort,
                        "noupdate" => OdbcConflict::KeepTarget,
                        "update" => OdbcConflict::Replace,
                        _ => return Err("旧版 ODBC 作业使用了未知的重复键策略。".into()),
                    };
                } else if !action.eq_ignore_ascii_case("abort") {
                    notices.duplicate_action = true;
                }
            }
        }
        if !data && !create {
            return Err(
                "旧作业包含仅导入表结构的表。请选择 --create-new 或 --recreate-existing。".into(),
            );
        }
        let foreign_keys = flag(&self.foreign_key)?.unwrap_or(false);
        let indexes = flag(&self.index)?.unwrap_or(false) || foreign_keys;
        let copy_foreign_keys = foreign_keys && options.copy_source_foreign_keys;
        let copy_indexes =
            indexes && (options.copy_source_indexes || options.copy_source_foreign_keys);
        notices.foreign_keys |= foreign_keys && !copy_foreign_keys;
        notices.indexes |= indexes && !copy_indexes;

        let mut fields = Vec::new();
        let mut columns = Vec::new();
        for column in &self.columns.items {
            if file.is_none() && !plain_identifier(&column.source) {
                return Err(
                    "旧版源列名需要按 ODBC 驱动的规则引用。请在原生作业中填写 SELECT 和列映射。"
                        .into(),
                );
            }
            if file.is_none() {
                fields.push(column.source.as_str());
            }
            let mut mapping = json!({"source": column.source, "target": column.target});
            if create && let Some(definition) = column.definition(notices)? {
                mapping["definition"] = json!(definition);
            }
            columns.push(mapping);
        }

        let condition = self
            .whereclause
            .as_deref()
            .map(str::trim)
            .filter(|part| !part.is_empty());
        let (mut source, table_name) = if let Some(file) = file {
            if condition.is_some() {
                return Err(
                    "此旧文件作业包含 WHERE 筛选。请使用 ODBC 连接字符串迁移该设置。".into(),
                );
            }
            let (file, table) = self.file_table(file)?;
            if file.format == FileSourceFormat::Csv {
                resolve_legacy_csv_columns(&file, &table, &mut columns)?;
            }
            (json!({"file_source": file, "file_table": table}), table)
        } else {
            let mut query = format!("SELECT {} FROM {}", fields.join(", "), self.source);
            if !data {
                query.push_str(" WHERE 1 = 0");
            } else if let Some(condition) = condition {
                query.push_str(" WHERE ");
                query.push_str(condition);
            }
            odbc::checked_select(&query)?;
            let mut source = credentials.clone();
            source["sql"] = json!(query);
            (source, self.source.clone())
        };
        if copy_indexes {
            if file.is_none() && self.source.contains('.') {
                return Err("旧作业使用带限定符的源表。请在原生作业的索引和外键来源设置中分别填写表名、目录及架构。".into());
            }
            source["indexes_from"] = json!({"table": table_name});
        }
        if copy_foreign_keys {
            source["copy_foreign_keys"] = json!(true);
        }
        if let Some(triggers) = &self.triggers {
            let delete = flag(&triggers.delete)?.unwrap_or(false);
            let updates = triggers
                .updates
                .as_ref()
                .map_or(&[][..], |updates| updates.items.as_slice());
            if delete || !updates.is_empty() {
                if options.preserve_source_actions {
                    if file.is_some() {
                        return Err("旧作业的源表更新或删除需要使用 ODBC 连接字符串迁移。".into());
                    }
                    if !data {
                        return Err("旧作业的仅结构导入不能设置源表更新或删除。".into());
                    }
                    source["after_import"] = source_action(&self.source, delete, updates)?;
                } else {
                    notices.source_action = true;
                }
            }
        }
        let mut definition = json!({"source": source, "table": self.target, "columns": columns});
        if create
            && let Some(engine) = self
                .engine
                .as_deref()
                .map(str::trim)
                .filter(|engine| !engine.is_empty())
        {
            definition["engine"] = json!(engine);
        }
        if conflict != OdbcConflict::Abort {
            definition["conflict"] = json!(conflict);
        }
        if let Some(existing) = strategy {
            definition["strategy"] = json!(TableStrategy {
                if_exists: Some(existing),
                if_missing: Some(MissingTarget::Create),
                structure_only: Some(!data),
            });
        } else if !data {
            definition["strategy"] = json!(TableStrategy {
                structure_only: Some(true),
                ..TableStrategy::default()
            });
        }
        Ok(definition)
    }
}

fn source_action(source: &str, delete: bool, updates: &[LegacyUpdate]) -> Result<Value, String> {
    let (catalog, table) = match source.rsplit_once('.') {
        Some((catalog, table)) if !catalog.contains('.') => (Some(catalog), table),
        Some(_) => return Err("旧版源表名称包含多个限定符，请在新作业中填写源表位置。".into()),
        None => (None, source),
    };
    let change = if delete {
        if !updates.is_empty() {
            return Err("同一张源表同时设置了更新和删除。请在新作业中选择一项。".into());
        }
        json!({"kind": "delete"})
    } else {
        let assignments = updates
            .iter()
            .map(|update| {
                let column = update.column.as_deref().ok_or("旧作业源表更新缺少列名。")?;
                let expression = update
                    .expression
                    .as_deref()
                    .ok_or("旧作业源表更新缺少赋值。")?;
                Ok::<_, String>(json!({"column": column, "expression": expression}))
            })
            .collect::<Result<Vec<_>, _>>()?;
        json!({"kind": "update", "assignments": assignments})
    };
    Ok(json!({"table": {"table": table, "catalog": catalog}, "change": change}))
}

impl LegacyColumn {
    fn definition(&self, notices: &mut Notices) -> Result<Option<OdbcColumnDefinition>, String> {
        let Some(kind) = self
            .kind
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            return Ok(None);
        };
        let sql_type = if let Some(length) = self
            .length
            .as_deref()
            .map(str::trim)
            .filter(|length| !length.is_empty())
        {
            format!("{kind}({length})")
        } else {
            kind.to_owned()
        };
        let auto_increment = flag(&self.autoincr)?.unwrap_or(false);
        let mut default = self.default.as_deref().filter(|value| !value.is_empty());
        if auto_increment {
            notices.automatic_default |= default.take().is_some();
        }
        let default = match default {
            Some(value) if value.trim().eq_ignore_ascii_case("NULL") => None,
            Some(value) if value.to_ascii_lowercase().contains("current_timestamp") => {
                return Err(
                    "旧作业使用了时间默认值表达式。请预建目标表并使用 --append-existing。".into(),
                );
            }
            Some(value) => {
                let unquoted = value
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''));
                Some(unquoted.map_or_else(|| value.to_owned(), |value| value.replace("''", "'")))
            }
            None => None,
        };
        let definition = OdbcColumnDefinition {
            sql_type,
            not_null: flag(&self.notnull)?.unwrap_or(false),
            primary: flag(&self.primary)?.unwrap_or(false),
            unsigned: flag(&self.unsigned)?.unwrap_or(false),
            binary: flag(&self.binary)?.unwrap_or(false),
            auto_increment,
            zerofill: flag(&self.zerofill)?.unwrap_or(false),
            default,
        };
        definition
            .validate()
            .map_err(|error| format!("旧版列定义无法用于自动建表。 {error}"))?;
        Ok(Some(definition))
    }
}

fn plain_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn converts_the_original_per_table_target_strategies_and_copy_settings() {
        let options = Options {
            use_legacy_table_strategies: true,
            ..Options::default()
        };
        let converted = [
            ("orders", "recreate", "yes", "recreate", false),
            ("customers", "import", "yes", "append", false),
            ("warehouses", "abort", "no", "skip", true),
        ]
        .into_iter()
        .map(|(name, conflict, data, expected, structure_only)| {
            let xml = format!(
                r#"<table source="{name}" target="{name}">
                <ddl onconflict="{conflict}"/><dml import="{data}" onconflict="abort"/>
                <columns><column><source>id</source><target>id</target></column></columns>
                </table>"#
            );
            let table: LegacyTable = quick_xml::de::from_str(&xml).unwrap();
            let table = table
                .convert(
                    &json!({"dsn": "fixture"}),
                    None,
                    &options,
                    &mut Notices::default(),
                )
                .unwrap();
            assert_eq!(
                table["strategy"],
                json!({
                    "if_missing": "create", "if_exists": expected, "structure_only": structure_only
                })
            );
            table
        })
        .collect::<Vec<_>>();
        let job: super::super::odbc_import::OdbcJob =
            serde_json::from_value(json!({"schema":"target","tables":converted})).unwrap();
        job.validate().unwrap();
        assert!(job.has_table_strategies());
    }

    #[test]
    fn converts_and_reopens_a_legacy_file_dsn_job() {
        let executable = std::env::current_exe().unwrap();
        let directory = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let input = directory.path().join("旧导入.xml");
        let output_directory = directory.path().join("迁移后");
        fs::create_dir(&output_directory).unwrap();
        let xml = r#"<job><odbcimport>
            <source><host>localhost</host><user>fixture</user><database>target</database></source>
            <dsn type="file"><name>库存来源.dsn</name><pwd>fixture-old-password-marker</pwd></dsn>
            <import source="data"/><export target="data"/><tables all="yes"/>
        </odbcimport></job>"#;
        fs::write(&input, xml).unwrap();
        assert_eq!(
            details(&input).unwrap(),
            Details {
                requires_connection_string: false,
                scope: OdbcScope::AllTables,
                file_format: None,
                mapped_file_path: None,
            }
        );
        let args = [
            "--create-new",
            "--target-passwordless",
            "--source-password-env",
            "HARBOR_TEST_SOURCE_PASSWORD",
        ]
        .map(OsString::from);
        let (definition, _) = prepare(&input, &args).unwrap();
        assert!(
            !definition
                .to_string()
                .contains("fixture-old-password-marker")
        );
        let output = output_directory.join("作业.json");
        fs::write(&output, serde_json::to_vec(&definition).unwrap()).unwrap();

        let reopened = super::super::odbc_editor::Definition::read(&output)
            .unwrap()
            .unwrap();
        let source = &reopened.job.discovery.as_ref().unwrap().source;
        assert_eq!(
            source.file_dsn.as_ref().unwrap(),
            &directory.path().join("库存来源.dsn")
        );
        assert_eq!(
            source.password_env.as_deref(),
            Some("HARBOR_TEST_SOURCE_PASSWORD")
        );
        assert_eq!(fs::read_to_string(input).unwrap(), xml);
    }

    #[test]
    fn converts_two_mapped_csv_tables_and_keeps_the_source_bytes() {
        let executable = std::env::current_exe().unwrap();
        let folder = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let sources = folder.path().join("来源");
        let output_folder = folder.path().join("迁移后");
        fs::create_dir(&sources).unwrap();
        fs::create_dir(&output_folder).unwrap();
        let stock = "编号,单位.单价,备注\n0007,1.25,不导入\n";
        let returns = "编号,原因,备注\n0014,缺件,不导入\n";
        fs::write(sources.join("archived.stock.csv"), stock).unwrap();
        fs::write(sources.join("order returns.csv"), returns).unwrap();
        let input = folder.path().join("旧导入.xml");
        let xml = r#"<job><odbcimport>
            <source><host>localhost</host><user>fixture</user><database>target</database></source>
            <dsn type="csv"><name>来源</name></dsn>
            <import source="data"/><export target="data"/>
            <tables all="no">
              <table source="archived#stock.csv" target="inventory" type="InnoDB">
                <columns>
                  <column><source>编号</source><target>item_id</target><type>VARCHAR</type><length>8</length></column>
                  <column><source>单位#单价</source><target>unit_price</target><type>DECIMAL</type><length>6,2</length></column>
                </columns>
              </table>
              <table source="order returns.csv" target="returns">
                <columns>
                  <column><source>编号</source><target>returned_item_id</target></column>
                  <column><source>原因</source><target>reason</target></column>
                </columns>
              </table>
            </tables>
        </odbcimport></job>"#;
        fs::write(&input, xml).unwrap();
        assert_eq!(
            details(&input).unwrap(),
            Details {
                requires_connection_string: true,
                scope: OdbcScope::MappedTables,
                file_format: Some(FileSourceFormat::Csv),
                mapped_file_path: Some(sources.clone()),
            }
        );
        let args = ["--create-new", "--target-passwordless"].map(OsString::from);
        let (definition, _) = prepare(&input, &args).unwrap();
        let output = output_folder.join("作业.json");
        fs::write(&output, serde_json::to_vec(&definition).unwrap()).unwrap();
        let reopened = super::super::odbc_editor::Definition::read(&output)
            .unwrap()
            .unwrap();
        let tables = reopened.job.tables.as_ref().unwrap();
        assert_eq!(tables.len(), 2);
        for (table, source_name, target_name, id, targets) in [
            (
                &tables[0],
                "archived.stock",
                "inventory",
                "0007",
                ["item_id", "unit_price"],
            ),
            (
                &tables[1],
                "order returns",
                "returns",
                "0014",
                ["returned_item_id", "reason"],
            ),
        ] {
            assert_eq!(table.table, target_name);
            assert_eq!(table.source.file_table.as_deref(), Some(source_name));
            assert!(table.source.sql.is_empty());
            assert_eq!(
                table
                    .columns
                    .iter()
                    .map(|column| column.target.as_str())
                    .collect::<Vec<_>>(),
                targets
            );
            let file = table.source.file_source.as_ref().unwrap();
            assert_eq!(file.path, sources);
            let data = file_source::read(file, source_name, false).unwrap();
            assert_eq!(data.columns[2].name, "备注");
            assert_eq!(data.rows[0][0], crate::database::Cell::Text(id.into()));
        }
        assert_eq!(tables[0].engine.as_deref(), Some("InnoDB"));
        assert_eq!(tables[0].columns[1].source, "单位.单价");
        assert_eq!(
            tables[0].columns[1].definition.as_ref().unwrap().sql_type,
            "DECIMAL(6,2)"
        );
        assert_eq!(
            fs::read_to_string(sources.join("archived.stock.csv")).unwrap(),
            stock
        );
        assert_eq!(
            fs::read_to_string(sources.join("order returns.csv")).unwrap(),
            returns
        );
        assert_eq!(fs::read_to_string(input).unwrap(), xml);
    }

    #[test]
    fn converts_all_tables_to_discovery_with_credentials_references_and_namespace() {
        let executable = std::env::current_exe().unwrap();
        let directory = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let input = directory.path().join("all-tables.xml");
        let xml = r#"<job><odbcimport>
            <source><host>localhost</host><user>fixture</user><pwd>fixture-target-marker</pwd><database>target</database></source>
            <dsn type="csv"><name>fixture.csv</name><pwd>fixture-source-marker</pwd></dsn>
            <import source="data"/><export target="data"/><tables all="yes"/>
            <abortonerror>no</abortonerror><sendreport send="no"/>
        </odbcimport></job>"#;
        fs::write(&input, xml).unwrap();
        assert_eq!(
            details(&input).unwrap(),
            Details {
                requires_connection_string: true,
                scope: OdbcScope::AllTables,
                file_format: Some(FileSourceFormat::Csv),
                mapped_file_path: None,
            }
        );
        for (mode, create, reset) in [
            ("--create-new", true, "none"),
            ("--append-existing", false, "none"),
            ("--truncate-existing", false, "truncate"),
            ("--recreate-existing", true, "recreate"),
        ] {
            let mut args = [
                mode,
                "--target-password-env",
                "HARBOR_TEST_TARGET_PASSWORD",
                "--source-connection-env",
                "HARBOR_TEST_SOURCE_CONNECTION",
                "--source-catalog",
                "catalog=fixture",
                "--source-schema",
                "warehouse",
            ]
            .map(OsString::from)
            .to_vec();
            if create {
                args.push("--copy-source-foreign-keys".into());
            }
            let (definition, notes) = prepare(&input, &args).unwrap();
            let job = &definition["job"];
            assert_eq!(job["create_table"], create);
            assert_eq!(job["reset"], reset);
            assert_eq!(job["continue_on_error"], true);
            assert!(job.get("tables").is_none());
            assert_eq!(
                job["discovery"],
                json!({
                    "source": {"connection_env":"HARBOR_TEST_SOURCE_CONNECTION"},
                    "catalog": "catalog=fixture", "schema": "warehouse",
                    "copy_indexes": create, "copy_foreign_keys": create,
                })
            );
            assert_eq!(
                definition["connection"]["password_env"],
                "HARBOR_TEST_TARGET_PASSWORD"
            );
            assert!(!definition.to_string().contains("marker"));
            assert!(notes.iter().any(|note| note.contains("遇错继续")));
        }

        let input_path = Path::new("fixtures/warehouse.csv");
        let args = [
            OsString::from("--create-new"),
            OsString::from("--target-passwordless"),
            OsString::from("--csv-source"),
            input_path.as_os_str().into(),
        ];
        let (definition, _) = prepare(&input, &args).unwrap();
        let source = &definition["job"]["discovery"]["source"];
        let file: FileSource = serde_json::from_value(source["file_source"].clone()).unwrap();
        assert_eq!(file.path, std::path::absolute(input_path).unwrap());
        assert!(source.get("connection_env").is_none());
        assert!(source.get("file_table").is_none());
        assert_eq!(fs::read_to_string(input).unwrap(), xml);
    }

    #[test]
    fn converts_legacy_access_to_discovery_with_keys_and_password_reference() {
        let executable = std::env::current_exe().unwrap();
        let directory = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let input = directory.path().join("access.xml");
        let xml = r#"<job><odbcimport>
            <source><host>localhost</host><user>fixture</user><database>target</database></source>
            <dsn type="access"><name>old.accdb</name><pwd>fixture-old-password-marker</pwd></dsn>
            <import source="data"/><export target="data"/><tables all="yes"/>
        </odbcimport></job>"#;
        fs::write(&input, xml).unwrap();
        assert_eq!(
            details(&input).unwrap(),
            Details {
                requires_connection_string: true,
                scope: OdbcScope::AllTables,
                file_format: Some(FileSourceFormat::Access),
                mapped_file_path: None,
            }
        );
        let file_path = Path::new("fixtures/inventory.accdb");
        let args = [
            "--recreate-existing".into(),
            "--target-passwordless".into(),
            "--copy-source-foreign-keys".into(),
            "--access-source".into(),
            file_path.as_os_str().to_owned(),
            "--access-password-env".into(),
            "HARBOR_TEST_ACCESS_PASSWORD".into(),
        ];
        let (definition, _) = prepare(&input, &args).unwrap();
        let job = &definition["job"];
        assert_eq!(job["create_table"], true);
        assert_eq!(job["reset"], "recreate");
        let discovery = &job["discovery"];
        assert_eq!(discovery["copy_indexes"], true);
        assert_eq!(discovery["copy_foreign_keys"], true);
        let source = discovery["source"].as_object().unwrap();
        assert_eq!(source.len(), 1);
        let file: FileSource = serde_json::from_value(source["file_source"].clone()).unwrap();
        assert_eq!(file.format, FileSourceFormat::Access);
        assert_eq!(file.path, std::path::absolute(file_path).unwrap());
        assert_eq!(
            file.access_password_env.as_deref(),
            Some("HARBOR_TEST_ACCESS_PASSWORD")
        );
        assert!(
            !definition
                .to_string()
                .contains("fixture-old-password-marker")
        );
        assert_eq!(fs::read_to_string(input).unwrap(), xml);
    }

    #[test]
    fn converts_legacy_excel_to_discovery_with_workbook_path() {
        let executable = std::env::current_exe().unwrap();
        let directory = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let input = directory.path().join("excel.xml");
        let xml = r#"<job><odbcimport>
            <source><host>localhost</host><user>fixture</user><database>target</database></source>
            <dsn type="excel"><name>old.xls</name></dsn>
            <import source="data"/><export target="data"/><tables all="yes"/>
        </odbcimport></job>"#;
        fs::write(&input, xml).unwrap();
        assert_eq!(
            details(&input).unwrap(),
            Details {
                requires_connection_string: true,
                scope: OdbcScope::AllTables,
                file_format: Some(FileSourceFormat::Excel),
                mapped_file_path: None,
            }
        );
        let workbook = Path::new("fixtures/stock lists.xlsx");
        let args = [
            "--append-existing".into(),
            "--target-passwordless".into(),
            "--excel-source".into(),
            workbook.as_os_str().to_owned(),
        ];
        let (definition, notes) = prepare(&input, &args).unwrap();
        let job = &definition["job"];
        assert_eq!(job["create_table"], false);
        let discovery = &job["discovery"];
        assert_eq!(discovery["copy_indexes"], false);
        assert_eq!(discovery["copy_foreign_keys"], false);
        let source = discovery["source"].as_object().unwrap();
        assert_eq!(source.len(), 1);
        let file: FileSource = serde_json::from_value(source["file_source"].clone()).unwrap();
        assert_eq!(file.format, FileSourceFormat::Excel);
        assert_eq!(file.path, std::path::absolute(workbook).unwrap());
        assert!(file.headers);
        assert!(notes.iter().any(|note| note.contains("首行作为表头")));
        assert_eq!(fs::read_to_string(input).unwrap(), xml);
    }
}
