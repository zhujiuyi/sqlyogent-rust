use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::IgnoredAny;

use crate::database::OdbcReset;
use crate::file_source::FileSourceFormat;

pub use super::legacy_odbc::OdbcScope;
pub use super::legacy_report::ReportInfo;
use super::{
    JOB_FILE_LIMIT, Job, LoadedJob, legacy, legacy_notify, legacy_odbc, legacy_sync,
    parse_definition, paths_match, unix_ms,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Backup {
        directory: bool,
    },
    DataSync {
        source: Transport,
        target: Transport,
        continues_on_error: bool,
    },
    Notification,
    Odbc {
        requires_connection_string: bool,
        scope: OdbcScope,
        source_file_format: Option<FileSourceFormat>,
    },
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Backup { .. } => "计划备份",
            Self::DataSync { .. } => "数据同步",
            Self::Notification => "SQL 通知",
            Self::Odbc { .. } => "ODBC 导入",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Direct,
    Http,
    Tunnel,
}

impl Transport {
    fn from_legacy(value: &str) -> Self {
        match value {
            "native" => Self::Direct,
            "http" => Self::Http,
            _ => Self::Tunnel,
        }
    }
}

#[derive(Deserialize)]
struct Contents {
    #[serde(default)]
    exportjob: Vec<IgnoredAny>,
    #[serde(default)]
    syncjob: Vec<IgnoredAny>,
    #[serde(default)]
    notifyjob: Vec<IgnoredAny>,
    #[serde(default)]
    odbcimport: Vec<IgnoredAny>,
}

pub struct Source {
    path: PathBuf,
    kind: Kind,
    mapped_file_path: Option<PathBuf>,
    requires_smtp_auth: bool,
    uses_replication_flags: bool,
    report: Option<ReportInfo>,
}

impl Source {
    pub fn open(path: &Path) -> Result<Self, String> {
        let path =
            fs::canonicalize(path).map_err(|error| format!("无法定位旧作业文件：{error}"))?;
        let contents: Contents = legacy::parse_xml(&path)?;
        let mut mapped_file_path = None;
        let kind = match (
            contents.exportjob.len(),
            contents.syncjob.len(),
            contents.notifyjob.len(),
            contents.odbcimport.len(),
        ) {
            (1, 0, 0, 0) => Kind::Backup {
                directory: legacy::directory_output(&path)?,
            },
            (0, 1, 0, 0) => {
                let (source, target, continues_on_error) = legacy_sync::details(&path)?;
                Kind::DataSync {
                    source: Transport::from_legacy(source),
                    target: Transport::from_legacy(target),
                    continues_on_error,
                }
            }
            (0, 0, 1, 0) => Kind::Notification,
            (0, 0, 0, 1) => {
                let details = legacy_odbc::details(&path)?;
                mapped_file_path = details.mapped_file_path;
                Kind::Odbc {
                    requires_connection_string: details.requires_connection_string,
                    scope: details.scope,
                    source_file_format: details.file_format,
                }
            }
            _ => {
                return Err(
                    "请选择只包含一个备份、同步、通知或 ODBC 导入作业的 SQLyog XML。".into(),
                );
            }
        };
        let requires_smtp_auth =
            kind == Kind::Notification && legacy_notify::requires_smtp_auth(&path)?;
        let uses_replication_flags =
            matches!(kind, Kind::Backup { .. }) && legacy::uses_replication_flags(&path)?;
        let report = match kind {
            Kind::Backup { .. } => legacy::report_info(&path)?,
            Kind::Odbc { .. } => legacy_odbc::report_info(&path)?,
            _ => None,
        };
        Ok(Self {
            path,
            kind,
            mapped_file_path,
            requires_smtp_auth,
            uses_replication_flags,
            report,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn mapped_file_path(&self) -> Option<&Path> {
        self.mapped_file_path.as_deref()
    }

    pub fn requires_smtp_auth(&self) -> bool {
        self.requires_smtp_auth
    }

    pub fn uses_replication_flags(&self) -> bool {
        self.uses_replication_flags
    }

    pub fn report_info(&self) -> Option<ReportInfo> {
        self.report
    }

    pub fn prepare(&self, destination: &Path, request: &Request) -> Result<Prepared, String> {
        self.prepare_with_report(destination, request, None)
    }

    pub fn prepare_with_report(
        &self,
        destination: &Path,
        request: &Request,
        report: Option<&ReportSettings>,
    ) -> Result<Prepared, String> {
        if !destination
            .extension()
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case("json"))
        {
            return Err("请使用 .json 作为新作业文件扩展名。".into());
        }
        let destination = std::path::absolute(destination)
            .map_err(|error| format!("无法定位新的作业文件：{error}"))?;
        check_destination(&self.path, &destination)?;
        let mut args = Vec::new();
        let (definition, mut notes) = match (self.kind, request) {
            (
                Kind::Backup { .. },
                Request::Backup {
                    output,
                    password_env,
                    copy_replication_lock,
                },
            ) => {
                args.push(required(output, "备份输出路径")?.into());
                password(
                    &mut args,
                    "--password-env",
                    "--passwordless",
                    password_env.as_deref(),
                )?;
                if *copy_replication_lock {
                    args.push(legacy::REPLICATION_LOCK_ARG.into());
                }
                if let Some(report) = report {
                    report.append_args(self.report, &mut args)?;
                }
                legacy::prepare(&self.path, &destination, &args)?
            }
            (
                Kind::DataSync { .. },
                Request::DataSync {
                    source,
                    target,
                    use_native_rules,
                },
            ) => {
                source.append_args(&mut args, "source")?;
                target.append_args(&mut args, "target")?;
                if *use_native_rules {
                    args.push("--use-native-rules".into());
                }
                let (definition, mut notes) = legacy_sync::prepare(&self.path, &args)?;
                notes.insert(
                    0,
                    "同步作业默认只比较和记录差异。可在作业编辑器中启用写入。".into(),
                );
                (definition, notes)
            }
            (
                Kind::Notification,
                Request::Notification {
                    report,
                    password_env,
                    smtp_user_env,
                    smtp_password_env,
                    smtp_tls,
                    smtp_port,
                },
            ) => {
                args.push(required(report, "HTML 报告文件")?.into());
                append_environment(&mut args, "--password-env", password_env)?;
                if self.requires_smtp_auth {
                    append_environment(&mut args, "--smtp-username-env", smtp_user_env)?;
                    append_environment(&mut args, "--smtp-password-env", smtp_password_env)?;
                }
                args.push(
                    if *smtp_tls {
                        "--smtp-tls-port"
                    } else {
                        "--smtp-starttls-port"
                    }
                    .into(),
                );
                args.push(smtp_port.to_string().into());
                legacy_notify::prepare(&self.path, &destination, &args)?
            }
            (
                Kind::Odbc {
                    scope: OdbcScope::MissingMappings,
                    ..
                },
                Request::Odbc { .. },
            ) => {
                return Err(
                    "旧作业缺少逐表列映射。请新建 ODBC 导入作业并设置源查询和目标列。".into(),
                );
            }
            (
                Kind::Odbc {
                    requires_connection_string,
                    source_file_format,
                    ..
                },
                Request::Odbc {
                    create,
                    reset,
                    use_legacy_table_strategies,
                    target_password_env,
                    source,
                    source_catalog,
                    source_schema,
                    use_legacy_conflict,
                    copy_indexes,
                    copy_foreign_keys,
                    preserve_source_actions,
                },
            ) => {
                args.push(
                    if *use_legacy_table_strategies {
                        "--use-legacy-table-strategies"
                    } else {
                        match (*create, *reset) {
                            (true, OdbcReset::None) => "--create-new",
                            (false, OdbcReset::None) => "--append-existing",
                            (false, OdbcReset::Truncate) => "--truncate-existing",
                            (true, OdbcReset::Recreate) => "--recreate-existing",
                            _ => return Err("请重新选择有效的目标表写入方式。".into()),
                        }
                    }
                    .into(),
                );
                password(
                    &mut args,
                    "--target-password-env",
                    "--target-passwordless",
                    target_password_env.as_deref(),
                )?;
                let file_source = matches!(
                    source,
                    OdbcSource::CsvPath(_)
                        | OdbcSource::ExcelPath(_)
                        | OdbcSource::AccessPath { .. }
                );
                let supports_metadata =
                    !matches!(source, OdbcSource::CsvPath(_) | OdbcSource::ExcelPath(_));
                match source {
                    OdbcSource::Dsn { .. } if requires_connection_string => {
                        return Err("请填写来源连接字符串的环境变量名。".into());
                    }
                    OdbcSource::Dsn {
                        user_env,
                        password_env,
                    } => {
                        if let Some(name) = user_env {
                            append_environment(&mut args, "--source-user-env", name)?;
                        }
                        if let Some(name) = password_env {
                            append_environment(&mut args, "--source-password-env", name)?;
                        }
                    }
                    OdbcSource::ConnectionString(environment) => {
                        append_environment(&mut args, "--source-connection-env", environment)?
                    }
                    OdbcSource::CsvPath(path) => {
                        if source_file_format != Some(FileSourceFormat::Csv)
                            || path.as_os_str().is_empty()
                        {
                            return Err("请选择这个旧作业要导入的 CSV 文件或文件夹。".into());
                        }
                        args.extend(["--csv-source".into(), path.as_os_str().to_owned()]);
                    }
                    OdbcSource::ExcelPath(path) => {
                        if source_file_format != Some(FileSourceFormat::Excel)
                            || path.as_os_str().is_empty()
                        {
                            return Err("请选择这个旧作业要导入的 Excel 工作簿。".into());
                        }
                        args.extend(["--excel-source".into(), path.as_os_str().to_owned()]);
                    }
                    OdbcSource::AccessPath { path, password_env } => {
                        if source_file_format != Some(FileSourceFormat::Access)
                            || path.as_os_str().is_empty()
                        {
                            return Err("请选择这个旧作业要导入的 Access 数据库文件。".into());
                        }
                        args.extend(["--access-source".into(), path.as_os_str().to_owned()]);
                        if let Some(name) = password_env {
                            append_environment(&mut args, "--access-password-env", name)?;
                        }
                    }
                }
                if !file_source {
                    for (flag, value) in [
                        ("--source-catalog", source_catalog),
                        ("--source-schema", source_schema),
                    ] {
                        if let Some(value) = value {
                            args.extend([flag.into(), value.as_str().into()]);
                        }
                    }
                }
                if *use_legacy_conflict {
                    args.push("--use-legacy-conflict".into());
                }
                let allows_creation = *create || *use_legacy_table_strategies;
                if supports_metadata && allows_creation && (*copy_indexes || *copy_foreign_keys) {
                    args.push("--copy-source-indexes".into());
                }
                if supports_metadata && allows_creation && *copy_foreign_keys {
                    args.push("--copy-source-foreign-keys".into());
                }
                if !file_source && *preserve_source_actions {
                    args.push("--preserve-source-actions".into());
                }
                if let Some(report) = report {
                    report.append_args(self.report, &mut args)?;
                }
                legacy_odbc::prepare(&self.path, &args)?
            }
            _ => return Err("迁移设置与旧作业类型不符。".into()),
        };
        let mut bytes =
            serde_json::to_vec_pretty(&definition).map_err(|_| "无法生成新的作业 JSON。")?;
        bytes.push(b'\n');
        notes.insert(0, "旧作业的登录密码不会复制到新作业。".into());
        let prepared = Prepared {
            input: self.path.clone(),
            destination,
            bytes,
            notes,
        };
        prepared.validated_bytes()?;
        Ok(prepared)
    }
}

pub struct ReportSettings {
    pub smtp_user_env: String,
    pub smtp_password_env: String,
    pub smtp_tls: bool,
    pub smtp_port: u16,
}

impl Default for ReportSettings {
    fn default() -> Self {
        Self {
            smtp_user_env: String::new(),
            smtp_password_env: String::new(),
            smtp_tls: false,
            smtp_port: 587,
        }
    }
}

impl ReportSettings {
    fn append_args(
        &self,
        info: Option<ReportInfo>,
        args: &mut Vec<OsString>,
    ) -> Result<(), String> {
        if let Some(info) = info {
            if info.needs_auth {
                append_environment(args, "--smtp-username-env", &self.smtp_user_env)?;
                append_environment(args, "--smtp-password-env", &self.smtp_password_env)?;
            }
            if info.needs_transport {
                args.push(
                    if self.smtp_tls {
                        "--smtp-tls-port"
                    } else {
                        "--smtp-starttls-port"
                    }
                    .into(),
                );
                args.push(self.smtp_port.to_string().into());
            }
        }
        Ok(())
    }
}

pub enum Request {
    Backup {
        output: String,
        password_env: Option<String>,
        copy_replication_lock: bool,
    },
    DataSync {
        source: Endpoint,
        target: Endpoint,
        use_native_rules: bool,
    },
    Notification {
        report: String,
        password_env: String,
        smtp_user_env: String,
        smtp_password_env: String,
        smtp_tls: bool,
        smtp_port: u16,
    },
    Odbc {
        create: bool,
        reset: OdbcReset,
        use_legacy_table_strategies: bool,
        target_password_env: Option<String>,
        source: OdbcSource,
        source_catalog: Option<String>,
        source_schema: Option<String>,
        use_legacy_conflict: bool,
        copy_indexes: bool,
        copy_foreign_keys: bool,
        preserve_source_actions: bool,
    },
}

#[derive(Default)]
pub struct Endpoint {
    pub password_env: String,
    pub address: Address,
}

#[derive(Default)]
pub enum Address {
    #[default]
    Original,
    Direct {
        host: String,
        port: u16,
    },
    Http(String),
}

impl Endpoint {
    fn append_args(&self, args: &mut Vec<OsString>, side: &str) -> Result<(), String> {
        append_environment(args, &format!("--{side}-password-env"), &self.password_env)?;
        match &self.address {
            Address::Original => {}
            Address::Direct { host, port } => args.extend([
                format!("--{side}-direct").into(),
                required(host, "直连主机")?.into(),
                port.to_string().into(),
            ]),
            Address::Http(url) => args.extend([
                format!("--{side}-http").into(),
                required(url, "PHP 隧道地址")?.into(),
            ]),
        }
        Ok(())
    }
}

pub enum OdbcSource {
    Dsn {
        user_env: Option<String>,
        password_env: Option<String>,
    },
    ConnectionString(String),
    CsvPath(PathBuf),
    ExcelPath(PathBuf),
    AccessPath {
        path: PathBuf,
        password_env: Option<String>,
    },
}

pub struct Prepared {
    input: PathBuf,
    destination: PathBuf,
    bytes: Vec<u8>,
    notes: Vec<String>,
}

impl Prepared {
    pub fn destination(&self) -> &Path {
        &self.destination
    }

    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    pub fn validated_bytes(&self) -> Result<&[u8], String> {
        check_destination(&self.input, &self.destination)?;
        if self.bytes.len() as u64 > JOB_FILE_LIMIT {
            return Err("作业定义最多支持 1 MiB。".into());
        }
        let directory = self.destination.parent().ok_or("无法定位新作业目录。")?;
        let directory = directory
            .canonicalize()
            .map_err(|error| format!("无法定位新作业目录：{error}"))?;
        let filename = self
            .destination
            .file_name()
            .ok_or("请选择作业 JSON 文件。")?;
        let loaded =
            LoadedJob::from_definition(parse_definition(&self.bytes)?, directory.join(filename))?;

        let started = unix_ms();
        let output = loaded
            .definition
            .job
            .output_path(&loaded.directory, started);
        let manifest = match (&loaded.definition.job, &output) {
            (Job::BackupDatabases(job), Some(output)) => Some(job.manifest_path(output, started)),
            _ => None,
        };
        if [&loaded.log, &loaded.lock]
            .into_iter()
            .chain(output.iter())
            .chain(manifest.iter())
            .any(|path| paths_match(path, &self.input))
        {
            return Err("新作业的日志、锁文件和输出必须使用与旧 XML 不同的路径。".into());
        }
        Ok(&self.bytes)
    }
}

fn check_destination(input: &Path, destination: &Path) -> Result<(), String> {
    if destination.exists() || paths_match(input, destination) {
        return Err("请选择尚未使用的新作业路径。".into());
    }
    Ok(())
}

fn required<'a>(value: &'a str, label: &str) -> Result<&'a str, String> {
    let value = value.trim();
    if value.is_empty() {
        Err(format!("请填写{label}。"))
    } else {
        Ok(value)
    }
}

fn password(
    args: &mut Vec<OsString>,
    flag: &str,
    passwordless: &str,
    environment: Option<&str>,
) -> Result<(), String> {
    if let Some(environment) = environment {
        append_environment(args, flag, environment)
    } else {
        args.push(passwordless.into());
        Ok(())
    }
}

fn append_environment(
    args: &mut Vec<OsString>,
    flag: &str,
    environment: &str,
) -> Result<(), String> {
    let name = required(environment, "凭据的环境变量名")?;
    if name.starts_with("--") || name.contains('=') || name.chars().any(char::is_control) {
        return Err("请填写有效的环境变量名。".into());
    }
    args.extend([flag.into(), name.into()]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_enabled_backup_and_odbc_reports_with_safe_smtp_settings() {
        let executable = std::env::current_exe().unwrap();
        let fixture = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let csv = fixture.path().join("fixture.csv");
        fs::write(&csv, "id\n1\n").unwrap();
        let smtp_fields = "<displayname>港湾备份</displayname><fromemail>sender@example.test</fromemail>\
            <toemail>first@example.test;second@example.test</toemail><cc>copy@example.test</cc>\
            <bcc>blind@example.test</bcc><replyemail>reply@example.test</replyemail>\
            <subject>yyyy-mm-dd hh:mm:ss</subject>";
        let backup = format!(
            "<exportjob><source><host>localhost</host><user>reader</user></source>\
             <export><databases all=\"yes\"/></export><sendreport send=\"yes\" when=\"onerror\">\
             <smtp><host>smtp.example.test</host><port>2465</port><encryption>ssl</encryption>\
             <auth required=\"yes\"><user>old-smtp-user</user><pwd>old-smtp-secret</pwd></auth>\
             {smtp_fields}</smtp></sendreport></exportjob>"
        );
        let odbc = format!(
            "<odbcimport><source><host>localhost</host><user>writer</user><database>fixture</database></source>\
             <dsn type=\"csv\"><name>legacy</name></dsn><import source=\"data\"/><export target=\"data\"/>\
             <tables all=\"yes\"/><sendreport send=\"yes\" when=\"always\">\
             <smtp><host>127.0.0.1</host><port>2525</port><encryption>no</encryption>\
             {smtp_fields}</smtp></sendreport></odbcimport>"
        );
        let report = ReportSettings {
            smtp_user_env: "FIXTURE_SMTP_USER".into(),
            smtp_password_env: "FIXTURE_SMTP_PASSWORD".into(),
            ..ReportSettings::default()
        };
        for (name, body, request, trigger, security, port) in [
            (
                "backup",
                backup,
                Request::Backup {
                    output: "export".into(),
                    password_env: None,
                    copy_replication_lock: false,
                },
                "failure",
                "tls",
                2465,
            ),
            (
                "odbc",
                odbc,
                Request::Odbc {
                    create: true,
                    reset: OdbcReset::None,
                    use_legacy_table_strategies: false,
                    target_password_env: None,
                    source: OdbcSource::CsvPath(csv.clone()),
                    source_catalog: None,
                    source_schema: None,
                    use_legacy_conflict: false,
                    copy_indexes: false,
                    copy_foreign_keys: false,
                    preserve_source_actions: false,
                },
                "always",
                "local_relay",
                2525,
            ),
        ] {
            let input = fixture.path().join(format!("{name}.xml"));
            fs::write(&input, format!("<job>{body}</job>")).unwrap();
            let source = Source::open(&input).unwrap();
            assert_eq!(source.report_info().unwrap().needs_auth, name == "backup");
            let prepared = source
                .prepare_with_report(
                    &fixture.path().join(format!("{name}.json")),
                    &request,
                    Some(&report),
                )
                .unwrap();
            let bytes = prepared.validated_bytes().unwrap();
            let converted: serde_json::Value = serde_json::from_slice(bytes).unwrap();
            let mail = &converted["notification"];
            assert_eq!(mail["when"], trigger);
            assert_eq!(mail["security"], security);
            assert_eq!(mail["port"], port);
            assert_eq!(
                mail["to"],
                serde_json::json!(["first@example.test", "second@example.test"])
            );
            assert_eq!(mail["cc"], serde_json::json!(["copy@example.test"]));
            assert_eq!(mail["bcc"], serde_json::json!(["blind@example.test"]));
            assert_eq!(mail["reply_to"], "reply@example.test");
            assert_eq!(mail["subject_date"], "date_time");
            assert_eq!(
                mail["username_env"].as_str(),
                (name == "backup").then_some("FIXTURE_SMTP_USER")
            );
            assert_eq!(
                mail["password_env"].as_str(),
                (name == "backup").then_some("FIXTURE_SMTP_PASSWORD")
            );
            let text = std::str::from_utf8(bytes).unwrap();
            assert!(!text.contains("old-smtp-user") && !text.contains("old-smtp-secret"));
        }
    }

    #[test]
    fn prepares_excel_file_and_access_password_reference_from_desktop_migration() {
        let executable = std::env::current_exe().unwrap();
        let fixture = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let excel = fixture.path().join("工作簿.xlsx");
        let access = fixture.path().join("密码库.accdb");
        for path in [&excel, &access] {
            fs::write(path, "路径校验样例").unwrap();
        }
        for (kind, format, input) in [
            (
                "excel",
                FileSourceFormat::Excel,
                OdbcSource::ExcelPath(excel),
            ),
            (
                "access",
                FileSourceFormat::Access,
                OdbcSource::AccessPath {
                    path: access,
                    password_env: Some("ACCESS_FILE_PASSWORD".into()),
                },
            ),
        ] {
            let xml = fixture.path().join(format!("{kind}.xml"));
            fs::write(&xml, format!(
                "<job><odbcimport><source><host>127.0.0.1</host><user>root</user><database>fixture</database></source><dsn type=\"{kind}\"><name>legacy</name></dsn><import source=\"data\"/><export target=\"data\"/><tables all=\"yes\"/></odbcimport></job>"
            )).unwrap();
            let source = Source::open(&xml).unwrap();
            assert!(
                matches!(source.kind(), Kind::Odbc { source_file_format: Some(active), .. } if active == format)
            );
            let destination = fixture.path().join(format!("{kind}.json"));
            let prepared = source
                .prepare(
                    &destination,
                    &Request::Odbc {
                        create: true,
                        reset: OdbcReset::None,
                        use_legacy_table_strategies: false,
                        target_password_env: None,
                        source: input,
                        source_catalog: None,
                        source_schema: None,
                        use_legacy_conflict: false,
                        copy_indexes: false,
                        copy_foreign_keys: false,
                        preserve_source_actions: false,
                    },
                )
                .unwrap();
            let job: serde_json::Value =
                serde_json::from_slice(prepared.validated_bytes().unwrap()).unwrap();
            let file = &job["job"]["discovery"]["source"]["file_source"];
            assert_eq!(file["format"], kind);
            if format == FileSourceFormat::Access {
                assert_eq!(file["access_password_env"], "ACCESS_FILE_PASSWORD");
            }
        }
    }
}
