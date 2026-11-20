use std::ffi::OsString;
use std::path::Path;

use lettre::message::{Mailbox, Mailboxes};
use serde::Deserialize;
use serde_json::{Value, json};

use super::legacy::{flag, parse_xml, save_definition};
use super::legacy_sync::{Endpoint, identifier, valid_host};
use super::{Job, JobFile, append_suffix, paths_match, validate_output};

pub(super) const USAGE: &str = "import-legacy-notify <旧通知.xml> <新作业.json> <报告.html> --password-env <数据库变量名> [--smtp-username-env <邮件用户名变量名> --smtp-password-env <邮件密码变量名>] --smtp-starttls-port <端口>（或 --smtp-tls-port）";

pub(super) fn import(args: &[OsString]) -> Result<(), String> {
    let [input, destination, options @ ..] = args else {
        return Err(USAGE.into());
    };
    let input = Path::new(input);
    let destination =
        std::path::absolute(destination).map_err(|error| format!("无法定位新作业文件：{error}"))?;
    let (definition, notes) = prepare(input, &destination, options)?;
    save_definition(input, &destination, &definition)?;
    println!("已转换 SQL 通知作业。{}", destination.display());
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
    let [
        report,
        database_flag,
        database_env,
        smtp_args @ ..,
        transport_flag,
        port,
    ] = args
    else {
        return Err(USAGE.into());
    };
    if database_flag != "--password-env" {
        return Err(USAGE.into());
    }
    let smtp_env = match smtp_args {
        [] => None,
        [user_flag, user_env, password_flag, password_env]
            if user_flag == "--smtp-username-env" && password_flag == "--smtp-password-env" =>
        {
            Some((user_env, password_env))
        }
        _ => return Err(USAGE.into()),
    };
    let runtime_env = |name: &OsString| -> Result<String, String> {
        name.to_str()
            .filter(|name| {
                !name.trim().is_empty()
                    && !name.contains('=')
                    && !name.chars().any(char::is_control)
            })
            .map(str::to_owned)
            .ok_or_else(|| "请提供有效的认证环境变量名。".into())
    };
    let database_env = runtime_env(database_env)?;
    let security = match transport_flag.to_str() {
        Some("--smtp-tls-port") => "tls",
        Some("--smtp-starttls-port") => "start_tls",
        _ => return Err(USAGE.into()),
    };
    let smtp_port = port
        .to_str()
        .and_then(|port| port.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .ok_or("请填写有效的加密 SMTP 端口。")?;
    let report = report.to_str().ok_or("报告路径应为 Unicode 路径。")?;
    validate_output(report)?;
    let directory = destination.parent().ok_or("无法定位新作业目录。")?;
    let report_output = directory.join(report);
    if [input, destination]
        .into_iter()
        .any(|path| paths_match(path, &report_output))
        || [".lock", ".log.jsonl"]
            .into_iter()
            .any(|suffix| paths_match(&append_suffix(destination, suffix), &report_output))
    {
        return Err("为旧作业、新作业、报告和日志使用不同的路径。".into());
    }
    let job = read(input)?;
    let smtp = &job.smtp;
    if !valid_host(smtp.host.trim()) {
        return Err("旧通知的 SMTP 主机需要使用主机名或 IP 地址。".into());
    }
    if smtp
        .displayname
        .as_deref()
        .is_some_and(|name| name.chars().any(char::is_control))
    {
        return Err("旧邮件发件人名称包含控制字符。".into());
    }
    let from = smtp
        .fromemail
        .trim()
        .parse()
        .map_err(|_| "旧邮件发件人地址无效。")?;
    let from = Mailbox::new(
        smtp.displayname
            .clone()
            .filter(|name| !name.trim().is_empty()),
        from,
    )
    .to_string();
    let include_report = flag(&job.result.value)?.ok_or("旧通知缺少结果报告选项。")?;
    let include_messages = flag(&job.includemsg.value)?.ok_or("旧通知缺少执行消息选项。")?;
    let mut definition = json!({
        "version":1,
        "connection":job.source.to_native("新的数据库连接配置",Some(&database_env),None)?,
        "job":{
            "type":"sql", "schema":identifier(&job.source.database,"数据库名称")?,
            "sql":job.query, "report":report,
            "continue_on_error":flag(&job.abortonerror.value)? == Some(false),
            "include_sql":flag(&job.includequery.value)? == Some(true),
            "include_messages":include_messages,
        },
        "notification":{
            "smtp_host":smtp.host.trim(),"port":smtp_port,"security":security,
            "from":from,
            "to":addresses(Some(&smtp.toemail))?,"cc":addresses(smtp.cc.as_deref())?,"bcc":addresses(smtp.bcc.as_deref())?,
            "reply_to":smtp.replyemail.as_deref().map(str::trim).filter(|address| !address.is_empty()),
            "subject":smtp.subject,"when":"always","include_report":include_report,
        }
    });
    let mut notes = vec!["新作业使用指定的加密 SMTP 端口。运行前请核对收件人和邮件服务器。".into()];
    if include_report {
        notes.push("成功的结果报告通过 HTML 附件发送。".into());
    }
    if smtp.auth.required()? {
        let (user_env, password_env) =
            smtp_env.ok_or("旧通知启用了 SMTP 认证，请提供 SMTP 用户名和密码的环境变量名。")?;
        let user_env = runtime_env(user_env)?;
        let password_env = runtime_env(password_env)?;
        if database_env.eq_ignore_ascii_case(&user_env)
            || database_env.eq_ignore_ascii_case(&password_env)
            || user_env.eq_ignore_ascii_case(&password_env)
        {
            return Err("数据库密码、SMTP 用户名和 SMTP 密码需要分别使用环境变量。".into());
        }
        definition["notification"]["username_env"] = json!(user_env);
        definition["notification"]["password_env"] = json!(password_env);
        notes.push("SMTP 登录信息由运行时环境变量提供。".into());
    }
    let parsed: JobFile =
        serde_json::from_value(definition.clone()).map_err(|_| "无法生成有效的 SQL 通知作业。")?;
    parsed.connection.validate()?;
    let Job::Sql(sql) = parsed.job else {
        return Err("旧通知转换生成了错误的作业类型。".into());
    };
    sql.validate(directory)?;
    parsed
        .notification
        .as_ref()
        .ok_or("新作业缺少 SMTP 通知。")?
        .validate()?;
    Ok((definition, notes))
}

pub(super) fn requires_smtp_auth(input: &Path) -> Result<bool, String> {
    read(input)?.smtp.auth.required()
}

fn read(input: &Path) -> Result<LegacyNotify, String> {
    let file: NotifyFile = parse_xml(input)?;
    let Ok([job]) = <[_; 1]>::try_from(file.jobs) else {
        return Err("请选择一个旧版 notifyjob。".into());
    };
    Ok(job)
}

pub(super) fn addresses(value: Option<&str>) -> Result<Vec<String>, String> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let addresses = value
        .parse::<Mailboxes>()
        .map_err(|_| "旧通知中的邮件地址列表无效。")?;
    Ok(addresses
        .into_iter()
        .map(|address| address.to_string())
        .collect())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifyFile {
    #[serde(rename = "@version")]
    _version: Option<String>,
    #[serde(rename = "notifyjob", default)]
    jobs: Vec<LegacyNotify>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyNotify {
    abortonerror: Choice,
    result: Choice,
    includequery: Choice,
    includemsg: Choice,
    source: Endpoint,
    smtp: Smtp,
    query: String,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(rename = "@value")]
    value: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Smtp {
    host: String,
    auth: Auth,
    displayname: Option<String>,
    fromemail: String,
    toemail: String,
    cc: Option<String>,
    bcc: Option<String>,
    replyemail: Option<String>,
    subject: Option<String>,
    #[serde(rename = "encryption")]
    _encryption: Option<serde::de::IgnoredAny>,
    #[serde(rename = "port")]
    _port: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Auth {
    #[serde(rename = "@required")]
    required: Option<String>,
    #[serde(rename = "user")]
    _username: Option<serde::de::IgnoredAny>,
    #[serde(rename = "pwd")]
    _password: Option<serde::de::IgnoredAny>,
}

impl Auth {
    fn required(&self) -> Result<bool, String> {
        flag(&self.required)?.ok_or_else(|| "旧通知缺少 SMTP 认证选项。".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::legacy_import::{Request, Source};
    use crate::jobs::mail::Security;

    #[test]
    fn migration_preserves_smtp_auth_and_execution_messages_independently() {
        let executable = std::env::current_exe().unwrap();
        let fixture = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let input = fixture.path().join("旧通知.xml");
        let destination = fixture.path().join("新作业.json");
        for (auth, messages) in [(false, true), (true, false)] {
            let xml_flag = |enabled| if enabled { "yes" } else { "no" };
            let auth_flag = xml_flag(auth);
            let messages_flag = xml_flag(messages);
            std::fs::write(
                &input,
                format!(
                    r#"<job><notifyjob>
  <abortonerror value="no"/><result value="yes"/>
  <includequery value="yes"/><includemsg value="{messages_flag}"/>
  <source><host>localhost</host><user>fixture</user><database>fixture</database></source>
  <smtp>
    <host>smtp.example.test</host><auth required="{auth_flag}"/>
    <fromemail>sender@example.test</fromemail><toemail>recipient@example.test</toemail>
    <subject>作业巡检</subject>
  </smtp>
  <query>SELECT 1</query>
</notifyjob></job>"#
                ),
            )
            .unwrap();

            let source = Source::open(&input).unwrap();
            assert_eq!(source.requires_smtp_auth(), auth);
            let smtp_user = auth.then_some("HARBOR_FIXTURE_SMTP_USER");
            let smtp_password = auth.then_some("HARBOR_FIXTURE_SMTP_PASSWORD");
            let prepared = source
                .prepare(
                    &destination,
                    &Request::Notification {
                        report: "报告.html".into(),
                        password_env: "HARBOR_FIXTURE_DB_PASSWORD".into(),
                        smtp_user_env: smtp_user.unwrap_or_default().into(),
                        smtp_password_env: smtp_password.unwrap_or_default().into(),
                        smtp_tls: false,
                        smtp_port: 587,
                    },
                )
                .unwrap();
            let parsed: JobFile =
                serde_json::from_slice(prepared.validated_bytes().unwrap()).unwrap();
            let Job::Sql(sql) = parsed.job else {
                panic!("应生成 SQL 作业");
            };
            assert_eq!(sql.include_messages, messages);
            assert_eq!(sql.report.as_deref(), Some("报告.html"));
            assert!(sql.include_sql && sql.continue_on_error);

            let notification = parsed.notification.unwrap();
            assert_eq!(notification.username_env.as_deref(), smtp_user);
            assert_eq!(notification.password_env.as_deref(), smtp_password);
            assert_eq!(notification.subject.as_deref(), Some("作业巡检"));
            assert!(notification.security == Security::StartTls);
            assert!(notification.include_report);
            assert!(notification.should_send(true) && notification.should_send(false));
        }
    }
}
