use std::fs::File;
use std::io::Read;
use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use chrono::Local;
use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart, header::ContentType};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use serde::{Deserialize, Serialize};

use crate::APP_NAME;

use super::{legacy_sync::valid_host, redact_values};

const ATTACHMENT_LIMIT: u64 = 10 * 1024 * 1024;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    pub smtp_host: String,
    pub port: Option<u16>,
    #[serde(default)]
    pub security: Security,
    pub username: Option<String>,
    pub username_env: Option<String>,
    pub password_env: Option<String>,
    pub from: String,
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    pub reply_to: Option<String>,
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject_date: Option<SubjectDate>,
    #[serde(default)]
    pub when: Trigger,
    #[serde(default)]
    pub include_report: bool,
}

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Security {
    #[default]
    Tls,
    StartTls,
    LocalRelay,
}

impl Security {
    pub fn default_port(self) -> u16 {
        match self {
            Self::Tls => 465,
            Self::StartTls => 587,
            Self::LocalRelay => 25,
        }
    }
}

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    #[default]
    Always,
    Success,
    Failure,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectDate {
    Date,
    DateTime,
}

impl Notification {
    pub(super) fn validate(&self) -> Result<(), String> {
        if !valid_host(&self.smtp_host) || self.port == Some(0) {
            return Err("请填写 SMTP 主机名或 IP 地址，端口和认证信息分别设置。".into());
        }
        let auth = self.username.is_some() || self.username_env.is_some();
        let invalid_env = |name: &String| {
            name.trim().is_empty() || name.contains('=') || name.chars().any(char::is_control)
        };
        if auth != self.password_env.is_some()
            || (self.username.is_some() && self.username_env.is_some())
            || self
                .username
                .as_ref()
                .is_some_and(|name| name.trim().is_empty())
            || self.password_env.as_ref().is_some_and(invalid_env)
            || self.username_env.as_ref().is_some_and(invalid_env)
        {
            return Err(
                "SMTP 登录需要设置 username 或 username_env，以及有效的 password_env。".into(),
            );
        }
        if matches!(self.security, Security::LocalRelay)
            && (auth
                || !self
                    .smtp_host
                    .parse::<IpAddr>()
                    .is_ok_and(|address| address.is_loopback()))
        {
            return Err("local_relay 需要使用回环 IP 地址和无需登录的 SMTP 中继。".into());
        }
        if self.from.parse::<Mailbox>().is_err()
            || self.to.is_empty()
            || self
                .to
                .iter()
                .chain(&self.cc)
                .chain(&self.bcc)
                .chain(self.reply_to.iter())
                .any(|email| email.parse::<Mailbox>().is_err())
        {
            return Err("请设置有效的邮件发件人和至少一个收件人。".into());
        }
        if self
            .subject
            .as_ref()
            .is_some_and(|subject| subject.chars().any(char::is_control))
        {
            return Err("邮件主题中不能包含控制字符。".into());
        }
        if self.subject.is_some() && self.subject_date.is_some() {
            return Err("邮件主题请选择固定文本或日期格式。".into());
        }
        Ok(())
    }

    pub(super) fn should_send(&self, success: bool) -> bool {
        match self.when {
            Trigger::Always => true,
            Trigger::Success => success,
            Trigger::Failure => !success,
        }
    }

    pub(super) fn send(
        &self,
        success: bool,
        body: String,
        report: Option<&Path>,
    ) -> Result<(), String> {
        self.deliver(&self.subject_for_run(success), body, report)
    }

    pub fn send_test(&self) -> Result<(), String> {
        self.validate()?;
        self.deliver(
            &format!("{APP_NAME} 邮件设置测试"),
            format!("这封测试邮件用于验证 {APP_NAME} 的 SMTP 设置。"),
            None,
        )
    }

    fn subject_for_run(&self, success: bool) -> String {
        match self.subject_date {
            Some(SubjectDate::Date) => Local::now().format("%Y-%m-%d").to_string(),
            Some(SubjectDate::DateTime) => Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            None => self.subject.clone().unwrap_or_else(|| {
                let status = if success { "完成" } else { "失败" };
                format!("{APP_NAME} 作业{status}")
            }),
        }
    }

    fn deliver(&self, subject: &str, body: String, report: Option<&Path>) -> Result<(), String> {
        let mut email = Message::builder()
            .from(self.from.parse().map_err(|_| "邮件发件人地址无效。")?)
            .subject(subject);
        for address in &self.to {
            email = email.to(address.parse().map_err(|_| "邮件收件人地址无效。")?);
        }
        for address in &self.cc {
            email = email.cc(address.parse().map_err(|_| "邮件抄送地址无效。")?);
        }
        for address in &self.bcc {
            email = email.bcc(address.parse().map_err(|_| "邮件密送地址无效。")?);
        }
        if let Some(address) = &self.reply_to {
            email = email.reply_to(address.parse().map_err(|_| "邮件回复地址无效。")?);
        }
        let email = if let Some(report) = report {
            email.multipart(
                MultiPart::mixed()
                    .singlepart(SinglePart::plain(body))
                    .singlepart(report_attachment(report)?),
            )
        } else {
            email.header(ContentType::TEXT_PLAIN).body(body)
        }
        .map_err(|error| format!("无法生成作业通知邮件：{error}"))?;

        let builder = match self.security {
            Security::Tls => SmtpTransport::relay(&self.smtp_host),
            Security::StartTls => SmtpTransport::starttls_relay(&self.smtp_host),
            Security::LocalRelay => Ok(SmtpTransport::builder_dangerous(&self.smtp_host)),
        }
        .map_err(|error| format!("无法配置 SMTP 连接：{error}"))?;
        let mut builder = builder.timeout(Some(Duration::from_secs(20)));
        if let Some(port) = self.port {
            builder = builder.port(port);
        }
        let username_env = self
            .username_env
            .as_ref()
            .map(|variable| {
                std::env::var(variable)
                    .map_err(|_| format!("无法读取 SMTP 用户名环境变量 {variable}。"))
            })
            .transpose()?;
        let username = self.username.as_ref().or(username_env.as_ref());
        let password = if let (Some(username), Some(variable)) = (username, &self.password_env) {
            let password = std::env::var(variable)
                .map_err(|_| format!("无法读取 SMTP 密码环境变量 {variable}。"))?;
            builder = builder.credentials(Credentials::new(username.clone(), password.clone()));
            password
        } else {
            String::new()
        };
        builder.build().send(&email).map(|_| ()).map_err(|error| {
            format!(
                "SMTP 发送失败。{}",
                redact_values(
                    &error.to_string(),
                    &[
                        password.as_str(),
                        username.map(String::as_str).unwrap_or_default(),
                    ]
                )
            )
        })
    }
}

fn report_attachment(path: &Path) -> Result<SinglePart, String> {
    let file = File::open(path).map_err(|error| format!("无法打开邮件附件：{error}"))?;
    let mut bytes = Vec::new();
    file.take(ATTACHMENT_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("无法读取邮件附件：{error}"))?;
    if bytes.len() as u64 > ATTACHMENT_LIMIT {
        return Err("报告超过邮件附件的 10 MiB 上限。".into());
    }
    let name = path
        .file_name()
        .ok_or("邮件附件缺少文件名。")?
        .to_string_lossy()
        .into_owned();
    Ok(Attachment::new(name).body(bytes, ContentType::TEXT_HTML))
}
