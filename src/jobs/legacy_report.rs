use std::ffi::OsString;
use std::net::IpAddr;

use lettre::message::Mailbox;
use serde::Deserialize;
use serde_json::{Value, json};

use super::legacy::flag;
use super::legacy_notify::addresses;
use super::mail::{Notification, Security, SubjectDate, Trigger};

pub(super) const USAGE: &str = "[--smtp-username-env <变量名> --smtp-password-env <变量名>] [--smtp-starttls-port <端口>|--smtp-tls-port <端口>]";

#[derive(Default)]
pub(super) struct ReportArgs<'a> {
    username_env: Option<&'a str>,
    password_env: Option<&'a str>,
    transport: Option<(Security, u16)>,
}

impl<'a> ReportArgs<'a> {
    pub(super) fn parse(mut args: &'a [OsString]) -> Result<Self, String> {
        let mut report = Self::default();
        while let Some((key, tail)) = args.split_first() {
            args = report.consume(key, tail)?.ok_or(USAGE)?;
        }
        Ok(report)
    }

    pub(super) fn consume(
        &mut self,
        key: &OsString,
        tail: &'a [OsString],
    ) -> Result<Option<&'a [OsString]>, String> {
        let Some(
            key @ ("--smtp-username-env"
            | "--smtp-password-env"
            | "--smtp-tls-port"
            | "--smtp-starttls-port"),
        ) = key.to_str()
        else {
            return Ok(None);
        };
        let [value, rest @ ..] = tail else {
            return Err(USAGE.into());
        };
        let value = value.to_str().map(str::trim).ok_or(USAGE)?;
        match key {
            "--smtp-username-env" | "--smtp-password-env" => {
                if value.is_empty()
                    || value.starts_with("--")
                    || value.contains('=')
                    || value.chars().any(char::is_control)
                {
                    return Err("请填写有效的 SMTP 凭据环境变量名。".into());
                }
                let slot = if key == "--smtp-username-env" {
                    &mut self.username_env
                } else {
                    &mut self.password_env
                };
                if slot.is_some() {
                    return Err(USAGE.into());
                }
                *slot = Some(value);
            }
            _ => {
                if self.transport.is_some() {
                    return Err(USAGE.into());
                }
                let port = value
                    .parse::<u16>()
                    .ok()
                    .filter(|port| *port != 0)
                    .ok_or("请填写有效的加密 SMTP 端口。")?;
                let security = if key == "--smtp-tls-port" {
                    Security::Tls
                } else {
                    Security::StartTls
                };
                self.transport = Some((security, port));
            }
        }
        Ok(Some(rest))
    }
}

#[derive(Clone, Copy)]
pub struct ReportInfo {
    pub needs_auth: bool,
    pub needs_transport: bool,
}

#[derive(Deserialize)]
pub(super) struct LegacyReport {
    #[serde(rename = "@send")]
    send: Option<String>,
    #[serde(rename = "@when")]
    when: Option<String>,
    smtp: Option<LegacySmtp>,
}

#[derive(Deserialize)]
struct LegacySmtp {
    host: String,
    port: Option<String>,
    encryption: Option<String>,
    displayname: Option<String>,
    fromemail: String,
    toemail: String,
    cc: Option<String>,
    bcc: Option<String>,
    replyemail: Option<String>,
    subject: Option<String>,
    auth: Option<LegacyAuth>,
}

#[derive(Deserialize)]
struct LegacyAuth {
    #[serde(rename = "@required")]
    required: Option<String>,
}

impl LegacySmtp {
    fn needs_auth(&self) -> Result<bool, String> {
        self.auth
            .as_ref()
            .map_or(Ok(false), |auth| Ok(flag(&auth.required)?.unwrap_or(false)))
    }

    fn transport(&self, needs_auth: bool) -> Option<(Security, u16)> {
        let encryption = self
            .encryption
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let security = match encryption.as_str() {
            "ssl" => Security::Tls,
            "tls" => Security::StartTls,
            "" | "no"
                if !needs_auth
                    && self
                        .host
                        .trim()
                        .parse::<IpAddr>()
                        .is_ok_and(|address| address.is_loopback()) =>
            {
                Security::LocalRelay
            }
            _ => return None,
        };
        let port = match self.port.as_deref().map(str::trim) {
            Some(port) if !port.is_empty() => port.parse::<u16>().ok().filter(|port| *port != 0)?,
            _ if security == Security::LocalRelay => security.default_port(),
            _ => return None,
        };
        Some((security, port))
    }
}

fn active_smtp(
    report: Option<&LegacyReport>,
) -> Result<Option<(&LegacyReport, &LegacySmtp)>, String> {
    let Some(report) = report else {
        return Ok(None);
    };
    if !flag(&report.send)?.unwrap_or(false) {
        return Ok(None);
    }
    let smtp = report
        .smtp
        .as_ref()
        .ok_or("旧作业启用了邮件报告，但缺少 SMTP 设置。")?;
    Ok(Some((report, smtp)))
}

pub(super) fn info(report: Option<&LegacyReport>) -> Result<Option<ReportInfo>, String> {
    let Some((_, smtp)) = active_smtp(report)? else {
        return Ok(None);
    };
    let needs_auth = smtp.needs_auth()?;
    Ok(Some(ReportInfo {
        needs_auth,
        needs_transport: smtp.transport(needs_auth).is_none(),
    }))
}

pub(super) fn migrate(
    report: Option<&LegacyReport>,
    args: ReportArgs<'_>,
    other_env: &[Option<&str>],
    definition: &mut Value,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    let Some((report, smtp)) = active_smtp(report)? else {
        if args.username_env.is_some() || args.password_env.is_some() || args.transport.is_some() {
            return Err("旧作业没有启用邮件报告，请移除 SMTP 转换参数。".into());
        }
        return Ok(());
    };
    let (when, label) = match report
        .when
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "always" => (Trigger::Always, "每次运行后"),
        "onerror" => (Trigger::Failure, "仅失败时"),
        _ => return Err("无法识别旧邮件报告的发送条件，请核对 when 的值。".into()),
    };
    let needs_auth = smtp.needs_auth()?;
    let (security, port) = args.transport.or_else(|| smtp.transport(needs_auth)).ok_or("旧 SMTP 传输设置无法沿用，请用 --smtp-starttls-port 或 --smtp-tls-port 选择已配置的加密端口。")?;
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
    let subject = smtp
        .subject
        .as_deref()
        .filter(|subject| !subject.is_empty());
    let subject_date = subject.and_then(|subject| match subject.to_ascii_lowercase().as_str() {
        "yyyy-mm-dd" => Some(SubjectDate::Date),
        "yyyy-mm-dd hh:mm:ss" => Some(SubjectDate::DateTime),
        _ => None,
    });
    let mut notification = Notification {
        smtp_host: smtp.host.trim().into(),
        port: Some(port),
        security,
        from: Mailbox::new(
            smtp.displayname
                .clone()
                .filter(|name| !name.trim().is_empty()),
            from,
        )
        .to_string(),
        to: report_addresses(Some(&smtp.toemail))?,
        cc: report_addresses(smtp.cc.as_deref())?,
        bcc: report_addresses(smtp.bcc.as_deref())?,
        reply_to: smtp
            .replyemail
            .as_deref()
            .map(str::trim)
            .filter(|address| !address.is_empty())
            .map(str::to_owned),
        subject: subject
            .filter(|_| subject_date.is_none())
            .map(str::to_owned),
        subject_date,
        when,
        ..Notification::default()
    };
    if needs_auth {
        let (Some(user), Some(password)) = (args.username_env, args.password_env) else {
            return Err(
                "旧邮件报告启用了 SMTP 认证，请提供 SMTP 用户名和密码的环境变量名。".into(),
            );
        };
        if user.eq_ignore_ascii_case(password)
            || other_env
                .iter()
                .flatten()
                .any(|name| name.eq_ignore_ascii_case(user) || name.eq_ignore_ascii_case(password))
        {
            return Err("SMTP 用户名、SMTP 密码和数据连接凭据需要分别使用环境变量。".into());
        }
        notification.username_env = Some(user.into());
        notification.password_env = Some(password.into());
    } else if args.username_env.is_some() || args.password_env.is_some() {
        return Err("旧邮件报告未启用 SMTP 认证，请移除 SMTP 凭据参数。".into());
    }
    notification.validate()?;
    notes.push(format!(
        "报告将在{label}发送。收件人 {}。",
        notification.to.join("、")
    ));
    for (label, recipients) in [("抄送", &notification.cc), ("密送", &notification.bcc)] {
        if !recipients.is_empty() {
            notes.push(format!("{label} {}。", recipients.join("、")));
        }
    }
    if let Some(reply_to) = &notification.reply_to {
        notes.push(format!("回复地址 {reply_to}。"));
    }
    if args.transport.is_some() {
        notes.push("SMTP 加密方式和端口采用本次指定的设置。".into());
    }
    definition["notification"] = json!(notification);
    Ok(())
}

fn report_addresses(value: Option<&str>) -> Result<Vec<String>, String> {
    let value = value.unwrap_or_default();
    let mut quoted = false;
    let mut escaped = false;
    let normalized: String = value
        .chars()
        .map(|ch| {
            if escaped {
                escaped = false;
            } else if quoted && ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = !quoted;
            } else if !quoted && ch == ';' {
                return ',';
            }
            ch
        })
        .collect();
    addresses(Some(&normalized))
}
