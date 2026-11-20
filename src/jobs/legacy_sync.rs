use std::ffi::OsString;
use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

use super::legacy::{flag, parse_xml, save_definition};
use super::{Job, JobFile, validate_identifier};

pub(super) const USAGE: &str = "import-legacy-sync <旧同步.xml> <新作业.json> --source-password-env <变量名> --target-password-env <变量名> [--source-direct <主机> <端口> | --source-http <地址>] [--target-direct <主机> <端口> | --target-http <地址>] [--use-native-rules]";

#[derive(Default)]
struct Options<'a> {
    source_password: Option<&'a str>,
    target_password: Option<&'a str>,
    source_direct: Option<(&'a str, u16)>,
    target_direct: Option<(&'a str, u16)>,
    source_http: Option<&'a str>,
    target_http: Option<&'a str>,
    native_rules: bool,
}

fn options(mut args: &[OsString]) -> Result<Options<'_>, String> {
    let mut options = Options::default();
    while let Some((key, tail)) = args.split_first() {
        let option = key.to_str().ok_or(USAGE)?;
        match option {
            "--source-password-env" | "--target-password-env" => {
                let [name, rest @ ..] = tail else {
                    return Err(USAGE.into());
                };
                let name = name
                    .to_str()
                    .filter(|name| {
                        !name.trim().is_empty()
                            && !name.contains('=')
                            && !name.chars().any(char::is_control)
                            && !name.starts_with("--")
                    })
                    .ok_or("请提供有效的密码环境变量名。")?;
                let assigned = if option == "--source-password-env" {
                    &mut options.source_password
                } else {
                    &mut options.target_password
                };
                if assigned.replace(name).is_some() {
                    return Err("重复设置了密码环境变量。".into());
                }
                args = rest;
            }
            "--source-direct" | "--target-direct" => {
                let [host, port, rest @ ..] = tail else {
                    return Err(USAGE.into());
                };
                let host = host
                    .to_str()
                    .filter(|host| valid_host(host))
                    .ok_or("请填写有效的直连主机名或 IP 地址。")?;
                let port = port
                    .to_str()
                    .and_then(|port| port.parse::<u16>().ok())
                    .filter(|port| *port != 0)
                    .ok_or("请填写有效的直连端口。")?;
                let assigned = if option == "--source-direct" {
                    &mut options.source_direct
                } else {
                    &mut options.target_direct
                };
                if assigned.replace((host, port)).is_some() {
                    return Err("重复设置了直连地址。".into());
                }
                args = rest;
            }
            "--source-http" | "--target-http" => {
                let [value, rest @ ..] = tail else {
                    return Err(USAGE.into());
                };
                let url = value
                    .to_str()
                    .filter(|value| !value.is_empty())
                    .ok_or("请填写 SQLyog PHP 隧道地址。")?;
                let assigned = if option == "--source-http" {
                    &mut options.source_http
                } else {
                    &mut options.target_http
                };
                if assigned.replace(url).is_some() {
                    return Err("重复设置了 HTTP 隧道地址。".into());
                }
                args = rest;
            }
            "--use-native-rules" if !options.native_rules => {
                options.native_rules = true;
                args = tail;
            }
            _ => return Err(USAGE.into()),
        }
    }
    if (options.source_http.is_some() && options.source_direct.is_some())
        || (options.target_http.is_some() && options.target_direct.is_some())
    {
        return Err("每一端只能选择直连地址或 HTTP 隧道地址。".into());
    }
    match (options.source_password, options.target_password) {
        (Some(source), Some(target)) if !source.eq_ignore_ascii_case(target) => Ok(options),
        _ => Err("请为源端和目标端分别指定不同的密码环境变量名。".into()),
    }
}

pub(super) fn valid_host(host: &str) -> bool {
    let address = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    !host.is_empty()
        && !host.starts_with('-')
        && (address.parse::<std::net::Ipv6Addr>().is_ok()
            || host
                .chars()
                .all(|ch| ch.is_alphanumeric() || matches!(ch, '.' | '-' | '_')))
}

pub(super) fn import(args: &[OsString]) -> Result<(), String> {
    let [input, output, args @ ..] = args else {
        return Err(USAGE.into());
    };
    let input = Path::new(input);
    let output =
        std::path::absolute(output).map_err(|error| format!("无法定位新作业文件：{error}"))?;
    let (definition, notices) = prepare(input, args)?;
    save_definition(input, &output, &definition)?;
    println!("已转换数据同步作业，默认预览。{}", output.display());
    for notice in notices {
        eprintln!("{notice}");
    }
    Ok(())
}

pub(super) fn prepare(input: &Path, args: &[OsString]) -> Result<(Value, Vec<String>), String> {
    let (definition, notices) = read(input)?.convert(options(args)?)?;
    let parsed: JobFile =
        serde_json::from_value(definition.clone()).map_err(|_| "无法生成有效的数据同步作业。")?;
    parsed.connection.validate()?;
    let Job::DataSync(job) = parsed.job else {
        return Err("旧同步转换生成了错误的作业类型。".into());
    };
    job.validate()?;
    Ok((definition, notices))
}

pub(super) fn details(input: &Path) -> Result<(&'static str, &'static str, bool), String> {
    let job = read(input)?;
    let (abort_on_error, _) = job.rules()?;
    Ok((
        job.source.transport(),
        job.target.transport(),
        !abort_on_error,
    ))
}

fn read(input: &Path) -> Result<LegacySync, String> {
    let file: File = parse_xml(input)?;
    let Ok([job]) = <[_; 1]>::try_from(file.jobs) else {
        return Err("请选择一个旧版 syncjob。".into());
    };
    Ok(job)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(rename = "@version")]
    _version: Option<String>,
    #[serde(rename = "syncjob", default)]
    jobs: Vec<LegacySync>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySync {
    abortonerror: Abort,
    fkcheck: ForeignKeys,
    #[serde(default)]
    twowaysync: TwoWay,
    #[serde(rename = "dontdelfromtarget", default)]
    keep_target_rows: KeepTargetRows,
    source: Endpoint,
    target: Endpoint,
    tables: Tables,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Abort {
    #[serde(rename = "@abort")]
    enabled: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForeignKeys {
    #[serde(rename = "@check")]
    option: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TwoWay {
    #[serde(rename = "@twoway")]
    enabled: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeepTargetRows {
    #[serde(rename = "@dontdel")]
    enabled: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Endpoint {
    host: String,
    port: Option<u16>,
    user: String,
    #[serde(rename = "pwd")]
    _password: Option<serde::de::IgnoredAny>,
    pub(super) database: String,
    ssl: Option<String>,
    sslauth: Option<String>,
    #[serde(rename = "clientkey")]
    _client_key: Option<serde::de::IgnoredAny>,
    #[serde(rename = "clientcert")]
    _client_cert: Option<serde::de::IgnoredAny>,
    cacert: Option<String>,
    cipher: Option<String>,
    charset: Option<String>,
    tunnelinfo: Option<Tunnel>,
    ssh: Option<serde::de::IgnoredAny>,
    http: Option<serde::de::IgnoredAny>,
    tunnel: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct Tunnel {
    #[serde(rename = "@type")]
    kind: Option<String>,
}

impl Endpoint {
    fn transport(&self) -> &'static str {
        match self
            .tunnelinfo
            .as_ref()
            .and_then(|tunnel| tunnel.kind.as_deref())
        {
            Some("http" | "https") => "http",
            Some("ssh") => "ssh",
            Some(_) => "tunnel",
            None if self.ssh.is_some() => "ssh",
            None if self.http.is_some() => "http",
            None if self.tunnel.is_some() || self.tunnelinfo.is_some() => "tunnel",
            None => "native",
        }
    }

    pub(super) fn to_native(
        &self,
        flag_name: &str,
        password_env: Option<&str>,
        direct: Option<(&str, u16)>,
    ) -> Result<Value, String> {
        if self.transport() != "native" && direct.is_none() {
            return Err(format!(
                "旧作业的同步连接使用隧道。请用 {flag_name} 指定经过确认的 MySQL 直连主机和端口。"
            ));
        }
        if flag(&self.sslauth)?.unwrap_or(false)
            || [&self.cacert, &self.cipher, &self.charset]
                .into_iter()
                .any(|field| {
                    field
                        .as_deref()
                        .is_some_and(|value| !value.trim().is_empty())
                })
        {
            return Err(
                "旧作业配置了客户端证书、CA、密码套件或字符集，请单独填写新连接配置。".into(),
            );
        }
        let (host, port) = direct.unwrap_or((self.host.trim(), self.port.unwrap_or(3306)));
        if !valid_host(host) || port == 0 {
            return Err(format!(
                "请使用 {flag_name} 填写此端有效的 MySQL 主机和端口。"
            ));
        }
        let mut connection =
            json!({"host":host,"port":port,"user":self.user,"password_env":password_env});
        if flag(&self.ssl)?.unwrap_or(false) {
            connection["tls"] = json!({});
        }
        Ok(connection)
    }

    fn for_sync(
        &self,
        direct_flag: &str,
        http_flag: &str,
        password: Option<&str>,
        direct: Option<(&str, u16)>,
        http: Option<&str>,
    ) -> Result<Value, String> {
        if let Some(url) = http {
            if self.transport() != "http" {
                return Err(format!("只有旧作业使用 HTTP 隧道时才需要 {http_flag}。"));
            }
            let host = self.host.trim();
            let port = self.port.unwrap_or(3306);
            if !valid_host(host) || port == 0 {
                return Err("旧作业需要有效的隧道端数据库地址和端口。".into());
            }
            return Ok(
                json!({"host":host,"port":port,"user":self.user,"password_env":password,"http":{"url":url}}),
            );
        }
        if self.transport() == "http" && direct.is_none() {
            return Err(format!(
                "请用 {http_flag} 指定 PHP 隧道地址，或用 {direct_flag} 迁移为直连。"
            ));
        }
        self.to_native(direct_flag, password, direct)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tables {
    #[serde(rename = "@all")]
    all: Option<String>,
    #[serde(rename = "table", default)]
    members: Vec<Table>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Table {
    name: String,
    columns: Columns,
    sqlwhere: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Columns {
    #[serde(rename = "@all")]
    all: Option<String>,
    #[serde(rename = "column", default)]
    members: Vec<String>,
}

impl Table {
    fn to_native(&self) -> Result<Value, String> {
        let name = identifier(&self.name, "数据表名称")?;
        let all = flag(&self.columns.all)?.ok_or("旧同步表缺少比较列选择。")?;
        if all != self.columns.members.is_empty() {
            return Err("旧同步表的比较列选择互相冲突。".into());
        }
        let mut table = json!({"source":name,"target":name});
        if !all {
            table["compare_columns"] = json!(
                self.columns
                    .members
                    .iter()
                    .map(|column| identifier(column, "比较列名称"))
                    .collect::<Result<Vec<_>, _>>()?
            );
        }
        if let Some(predicate) = self
            .sqlwhere
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            table["where"] = json!(predicate);
            table["filter_scope"] = json!("source");
        }
        Ok(table)
    }
}

impl LegacySync {
    fn rules(&self) -> Result<(bool, bool), String> {
        let abort_on_error =
            flag(&self.abortonerror.enabled)?.ok_or("旧同步作业缺少错误处理选项。")?;
        let skip_target_fk_checks =
            flag(&self.fkcheck.option)?.ok_or("旧同步作业缺少外键选项。")?;
        Ok((abort_on_error, skip_target_fk_checks))
    }

    fn convert(&self, input: Options<'_>) -> Result<(Value, Vec<String>), String> {
        let (abort_on_error, skip_target_fk_checks) = self.rules()?;
        let two_way = flag(&self.twowaysync.enabled)?.unwrap_or_default();
        let keep_target_rows = flag(&self.keep_target_rows.enabled)?.unwrap_or_default();
        let mode = if two_way {
            "reconcile_both"
        } else if keep_target_rows {
            "insert_and_update"
        } else {
            "mirror_receiver"
        };
        let source = self.source.for_sync(
            "--source-direct",
            "--source-http",
            input.source_password,
            input.source_direct,
            input.source_http,
        )?;
        let target = self.target.for_sync(
            "--target-direct",
            "--target-http",
            input.target_password,
            input.target_direct,
            input.target_http,
        )?;
        let continue_on_row_error = !abort_on_error && !input.native_rules;
        let all_tables = flag(&self.tables.all)?.ok_or("旧同步作业缺少同步表范围。")?;
        if all_tables != self.tables.members.is_empty() {
            return Err("请在旧作业中选择全部或明确列出同步表。".into());
        }
        let mut job = json!({
            "type":"data_sync", "source_schema":identifier(&self.source.database,"源数据库名称")?,
            "target_schema":identifier(&self.target.database,"目标数据库名称")?, "target":target,
            "mode":mode, "apply":false,
            "legacy":{
                "abort_on_error":abort_on_error,"fkcheck":skip_target_fk_checks,
                "source_transport":self.source.transport(),"target_transport":self.target.transport(),
            },
        });
        if skip_target_fk_checks {
            job["skip_target_fk_checks"] = json!(true);
        }
        if continue_on_row_error {
            job["continue_on_row_error"] = json!(true);
        }
        if two_way {
            job["conflict"] = json!("source_wins");
        }
        if all_tables {
            job["all_tables"] = json!(true);
        } else {
            job["tables"] = json!(
                self.tables
                    .members
                    .iter()
                    .map(Table::to_native)
                    .collect::<Result<Vec<_>, _>>()?
            );
        }
        let mut notes = Vec::new();
        if input.source_http.is_some() || input.target_http.is_some() {
            notes.push(
                "已保留旧作业的 HTTP 连接方式。网页登录或代理认证请在新作业中通过环境变量配置。"
                    .into(),
            );
        }
        if input.source_direct.is_some() && self.source.transport() != "native" {
            notes.push("源端已从旧版隧道改为明确指定的 MySQL 直连。".into());
        }
        if input.target_direct.is_some() && self.target.transport() != "native" {
            notes.push("目标端已从旧版隧道改为明确指定的 MySQL 直连。".into());
        }
        if !abort_on_error {
            let note = if continue_on_row_error {
                "已保留旧作业的设置，遇到单行写入错误后继续同步。"
            } else {
                "已选择遇到写入错误时停止同步。"
            };
            notes.push(note.into());
        }
        if skip_target_fk_checks {
            notes.push("已保留旧作业的设置，写入目标端时暂停外键检查。".into());
        }
        if !two_way && keep_target_rows {
            notes.push("单向同步会保留目标端独有的行。".into());
        }
        Ok((json!({"version":1,"connection":source,"job":job}), notes))
    }
}

pub(super) fn identifier(raw: &str, label: &str) -> Result<String, String> {
    let raw = raw.trim();
    let name = if let Some(inner) = raw
        .strip_prefix('`')
        .and_then(|text| text.strip_suffix('`'))
    {
        let mut name = String::new();
        let mut chars = inner.chars();
        while let Some(ch) = chars.next() {
            if ch == '`' && chars.next() != Some('`') {
                return Err(format!("旧版{label}引号无效。"));
            }
            name.push(ch);
        }
        name
    } else {
        if raw.contains('`') {
            return Err(format!("旧版{label}引号无效。"));
        }
        raw.to_owned()
    };
    validate_identifier(&name, label)?;
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_legacy_sync_target_only_row_handling() {
        let keep = r#"<dontdelfromtarget dontdel="yes"/>"#;
        let mirror = r#"<dontdelfromtarget dontdel="no"/>"#;
        let one_way = r#"<twowaysync twoway="no"/>"#;
        let two_way = r#"<twowaysync twoway="yes"/>"#;
        for (direction, row_policy, expected_mode) in [
            ("", keep, "insert_and_update"),
            (one_way, keep, "insert_and_update"),
            ("", mirror, "mirror_receiver"),
            (one_way, "", "mirror_receiver"),
            ("", "", "mirror_receiver"),
            (two_way, keep, "reconcile_both"),
        ] {
            let xml = format!(
                r#"<job version="6.5"><syncjob>
                    <abortonerror abort="yes"/><fkcheck check="no"/>
                    {direction}{row_policy}
                    <source><host>localhost</host><user>reader</user><database>from_db</database></source>
                    <target><host>localhost</host><user>writer</user><database>to_db</database></target>
                    <tables all="yes"/>
                </syncjob></job>"#
            );
            let legacy: File = quick_xml::de::from_str(&xml).unwrap();
            let options = Options {
                source_password: Some("LEGACY_SOURCE_PASSWORD"),
                target_password: Some("LEGACY_TARGET_PASSWORD"),
                ..Default::default()
            };
            let (definition, _) = legacy.jobs[0].convert(options).unwrap();
            let job = &definition["job"];
            assert_eq!(job["mode"], expected_mode);
            assert_eq!(job["apply"], false);
            if expected_mode == "reconcile_both" {
                assert_eq!(job["conflict"], "source_wins");
            } else {
                assert!(job.get("conflict").is_none());
            }
            let definition: JobFile = serde_json::from_value(definition).unwrap();
            let Job::DataSync(job) = definition.job else {
                panic!("expected a data synchronization job");
            };
            job.validate().unwrap();
        }
    }
}
