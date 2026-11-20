use std::path::{Path, PathBuf};

use serde::{Serialize, Serializer};

pub use super::mail::{Notification, Security, SubjectDate, Trigger};
pub use super::sql::{MAX_REPORT_ROWS, SqlJob};
use super::{Job, JobFile, parse_definition, read_definition_bytes, validated_editor_bytes};
pub use super::{JobConnection, Schedule};

#[derive(Serialize)]
pub struct Definition {
    version: u32,
    pub connection: JobConnection,
    #[serde(rename = "job", serialize_with = "serialize_job")]
    pub sql: SqlJob,
    pub schedule: Option<Schedule>,
    pub log: Option<PathBuf>,
    pub notification: Option<Notification>,
    #[serde(skip)]
    original: Option<Vec<u8>>,
}

fn serialize_job<S: Serializer>(sql: &SqlJob, serializer: S) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum TypedJob<'a> {
        Sql(&'a SqlJob),
    }
    TypedJob::Sql(sql).serialize(serializer)
}

impl Definition {
    pub fn new(schema: Option<String>) -> Self {
        Self {
            version: 1,
            connection: JobConnection::default(),
            sql: SqlJob {
                schema,
                ..Default::default()
            },
            schedule: None,
            log: None,
            notification: None,
            original: None,
        }
    }

    pub fn read(path: &Path) -> Result<Option<Self>, String> {
        let original = read_definition_bytes(path)?;
        let JobFile {
            version,
            connection,
            job,
            schedule,
            log,
            notification,
        } = parse_definition(&original)?;
        let Job::Sql(sql) = job else {
            return Ok(None);
        };
        Ok(Some(Self {
            version,
            connection,
            sql,
            schedule,
            log,
            notification,
            original: Some(original),
        }))
    }

    pub fn verify_unchanged(&self, path: &Path) -> Result<(), String> {
        let original = self.original.as_ref().ok_or("找不到原 SQL 作业内容。")?;
        if &read_definition_bytes(path)? != original {
            return Err("作业文件已在其他窗口更新，请重新打开后再编辑。".into());
        }
        Ok(())
    }

    pub fn validated_bytes(&self, destination: &Path) -> Result<Vec<u8>, String> {
        validated_editor_bytes(self, destination)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn sql_notification_definition_roundtrip() {
        let executable = std::env::current_exe().unwrap();
        let directory = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let path = directory.path().join("notification.json");
        let script = PathBuf::from("lookup.sql");
        fs::write(directory.path().join(&script), "SELECT 'a;b'; SELECT 2;").unwrap();

        let mut editor = Definition::new(Some("sample".into()));
        editor.sql.sql = None;
        editor.sql.script = Some(script.clone());
        editor.sql.report = Some("report-{unix_ms}.html".into());
        editor.sql.continue_on_error = true;
        editor.log = Some("run.jsonl".into());
        editor.notification = Some(Notification {
            smtp_host: "smtp.example.com".into(),
            security: Security::StartTls,
            username_env: Some("TEST_SMTP_USER".into()),
            password_env: Some("TEST_SMTP_PASSWORD".into()),
            from: "Report sender <sender@example.com>".into(),
            to: vec!["to@example.com".into()],
            cc: vec!["cc@example.com".into()],
            include_report: true,
            ..Notification::default()
        });
        let bytes = editor.validated_bytes(&path).unwrap();
        fs::write(&path, bytes).unwrap();

        let mut reopened = Definition::read(&path).unwrap().unwrap();
        reopened.verify_unchanged(&path).unwrap();
        assert_eq!(reopened.sql.check_statements(directory.path()), Ok(2));
        reopened.sql.include_sql = true;
        reopened.sql.include_messages = false;
        reopened.notification.as_mut().unwrap().subject_date = Some(SubjectDate::DateTime);
        fs::write(&path, reopened.validated_bytes(&path).unwrap()).unwrap();

        let saved = Definition::read(&path).unwrap().unwrap();
        assert_eq!(saved.sql.script, Some(script));
        assert_eq!(saved.log, Some(PathBuf::from("run.jsonl")));
        assert!(
            saved.sql.continue_on_error && saved.sql.include_sql && !saved.sql.include_messages
        );
        let mail = saved.notification.unwrap();
        assert!(mail.subject_date == Some(SubjectDate::DateTime));
        assert_eq!(mail.username_env.as_deref(), Some("TEST_SMTP_USER"));
        assert_eq!(mail.password_env.as_deref(), Some("TEST_SMTP_PASSWORD"));
        assert_eq!(mail.cc, ["cc@example.com"]);
        assert!(mail.include_report);
    }
}
