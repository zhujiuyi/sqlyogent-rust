use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use quick_xml::escape::escape;
use serde::Deserialize;

use super::{Change, MonthDays, Months, Status, Trigger, calendar, checked, id, validated_runner};
use crate::app::job_manager::{CATALOG_LIMIT, run_command, write_atomic};
use harbor_sql::jobs::job_lock_path;

struct Agent {
    label: String,
    plist: PathBuf,
    domain: String,
    target: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Definition {
    label: String,
    program: Option<PathBuf>,
    program_arguments: Vec<PathBuf>,
    #[serde(default)]
    enable_globbing: bool,
    start_calendar_interval: Option<serde_json::Value>,
    #[serde(default)]
    environment_variables: ScheduleEnvironment,
}

#[derive(Default, Deserialize)]
struct ScheduleEnvironment {
    #[serde(rename = "HARBOR_SQL_MONTHLY_DAY_MASK")]
    monthly_days: Option<String>,
    #[serde(rename = "HARBOR_SQL_MONTH_MASK")]
    months: Option<String>,
}

impl Definition {
    fn is_owned_by(&self, label: &str, job: &Path) -> bool {
        let [runner, operation, file, extra @ ..] = self.program_arguments.as_slice() else {
            return false;
        };
        self.label == label
            && runner.is_absolute()
            && runner
                .file_name()
                .is_some_and(|name| name == "harbor-sql-job")
            && self
                .program
                .as_ref()
                .is_none_or(|program| program == runner)
            && !self.enable_globbing
            && file == job
            && match extra {
                [] => operation == Path::new("run"),
                [_] => matches!(self.trigger(), Some(Trigger::Once { .. })),
                _ => false,
            }
    }

    fn trigger(&self) -> Option<Trigger> {
        let intervals = self.start_calendar_interval.as_ref()?;
        match (
            self.program_arguments.as_slice(),
            self.environment_variables.monthly_days.as_deref(),
            self.environment_variables.months.as_deref(),
        ) {
            ([_, operation, _, timestamp], None, None) if operation == Path::new("run-once-at") => {
                calendar::parse_once(intervals, timestamp.to_str()?)
            }
            ([_, operation, _], None, None) if operation == Path::new("run") => {
                calendar::parse(intervals)
            }
            ([_, operation, _], Some(days), Some(months)) if operation == Path::new("run") => {
                calendar::parse_with_monthly_selection(
                    intervals,
                    Some((
                        MonthDays::from_mask(days.parse().ok()?)?,
                        Months::from_mask(months.parse().ok()?)?,
                    )),
                )
            }
            _ => None,
        }
    }
}

struct LiveAgent {
    idle: bool,
    path: Option<PathBuf>,
}

pub(super) fn apply(change: Change, job: &Path) -> Result<Status, String> {
    let agent = Agent::new(job)?;
    let _lock = if matches!(change, Change::Inspect) {
        None
    } else {
        lock_job(job)?
    };
    let installed = agent.installed(job)?;
    let live = agent.live()?;
    if installed.is_none() && live.is_some() {
        return Err("系统中已有同名调度，但找不到港湾 SQL 的调度文件。".into());
    }

    match change {
        Change::Inspect => Ok(
            installed.map_or(Status::Absent, |definition| Status::Registered {
                trigger: definition.trigger(),
                enabled: live.is_some(),
            }),
        ),
        Change::Save(trigger) => {
            let runner = validated_runner(job)?;
            let plist = make_plist(&agent.label, &runner, job, &trigger)?;
            agent.save(plist, live.as_ref())?;
            Ok(Status::Registered {
                trigger: Some(trigger),
                enabled: true,
            })
        }
        Change::Cancel => {
            if installed.is_some() {
                agent.cancel(live.as_ref())?;
            }
            Ok(Status::Absent)
        }
    }
}

impl Agent {
    fn new(job: &Path) -> Result<Self, String> {
        let label = format!("io.harborsql.job.{}", id(job)?);
        let home = directories::BaseDirs::new().ok_or("无法找到当前账户的个人目录。")?;
        let plist = home
            .home_dir()
            .join("Library/LaunchAgents")
            .join(format!("{label}.plist"));
        let uid = checked(
            Command::new("/usr/bin/id").arg("-u"),
            "无法查询当前 macOS 账户",
        )?
        .stdout
        .trim()
        .parse::<u32>()
        .map_err(|_| "无法确认当前 macOS 账户。")?;
        let domain = format!("gui/{uid}");
        let target = format!("{domain}/{label}");
        Ok(Self {
            label,
            plist,
            domain,
            target,
        })
    }

    fn installed(&self, job: &Path) -> Result<Option<Definition>, String> {
        match fs::symlink_metadata(&self.plist) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("无法打开 macOS 调度文件：{error}")),
            Ok(info) if info.is_file() && info.len() <= CATALOG_LIMIT => {}
            Ok(_) => return Err("macOS 调度文件无效。".into()),
        }
        let converted = checked(
            Command::new("/usr/bin/plutil")
                .args(["-convert", "json", "-o", "-"])
                .arg(&self.plist),
            "无法读取 macOS 调度文件",
        )?;
        let definition: Definition =
            serde_json::from_str(&converted.stdout).map_err(|_| "无法识别 macOS 调度文件。")?;
        if !definition.is_owned_by(&self.label, job) {
            return Err("同名 macOS 调度文件对应另一个作业。".into());
        }
        Ok(Some(definition))
    }

    fn live(&self) -> Result<Option<LiveAgent>, String> {
        let output = run_command(Command::new("/bin/launchctl").args(["print", &self.target]))?;
        match output.code {
            Some(0) => {
                // The inspection may contain the session environment. Retain only the status
                // and the file backing this agent, and never display the inspection output.
                let idle = output
                    .stdout
                    .lines()
                    .any(|line| line.trim() == "state = not running");
                let path = output
                    .stdout
                    .lines()
                    .find_map(|line| line.trim_start().strip_prefix("path = ").map(PathBuf::from));
                Ok(Some(LiveAgent { idle, path }))
            }
            Some(3 | 113) => Ok(None),
            Some(112) => Err("当前账户尚未登录 macOS 桌面。".into()),
            Some(code) => Err(format!("无法查询 macOS 调度状态（退出码 {code}）。")),
            None => Err("无法查询 macOS 调度状态。".into()),
        }
    }

    fn pause(&self, live: Option<&LiveAgent>) -> Result<(), String> {
        let Some(live) = live else {
            return Ok(());
        };
        if !live.path.as_deref().is_some_and(|path| {
            path == self.plist
                || fs::canonicalize(&self.plist).is_ok_and(|expected| {
                    fs::canonicalize(path).is_ok_and(|actual| actual == expected)
                })
        }) {
            return Err("无法确认 macOS 已加载的同名调度来自港湾 SQL。".into());
        }
        if !live.idle {
            return Err("作业正在运行，或 macOS 暂时无法确认作业状态。请稍后重试。".into());
        }
        let result = Command::new("/bin/launchctl")
            .args(["bootout", &self.target])
            .output()
            .map_err(|error| format!("无法卸载现有 macOS 调度：{error}"))?;
        if !result.status.success() && !matches!(result.status.code(), Some(3 | 113)) {
            return Err(format!("无法卸载现有 macOS 调度（{}）。", result.status));
        }
        Ok(())
    }

    fn load(&self) -> Result<(), String> {
        checked(
            Command::new("/bin/launchctl")
                .args(["bootstrap", &self.domain])
                .arg(&self.plist),
            "无法启用 macOS 调度",
        )
        .map(|_| ())
    }

    fn save(&self, plist: String, live: Option<&LiveAgent>) -> Result<(), String> {
        let parent = self.plist.parent().ok_or("无法定位 macOS 调度目录。")?;
        fs::create_dir_all(parent).map_err(|error| format!("无法创建 macOS 调度目录：{error}"))?;
        let previous = match fs::read(&self.plist) {
            Ok(previous) => Some(previous),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("无法读取现有 macOS 调度：{error}")),
        };
        self.pause(live)?;
        let mut saved = false;
        let result = (|| {
            write_atomic(&self.plist, plist.as_bytes())
                .map_err(|error| format!("无法保存 macOS 调度文件：{error}"))?;
            saved = true;
            self.load()
        })();
        if let Err(error) = result {
            return Err(restore_error(error, || {
                if saved {
                    match previous {
                        Some(previous) => write_atomic(&self.plist, &previous),
                        None => fs::remove_file(&self.plist),
                    }
                    .map_err(|error| error.to_string())?;
                }
                if live.is_some() {
                    self.load()?;
                }
                Ok(())
            }));
        }
        Ok(())
    }

    fn cancel(&self, live: Option<&LiveAgent>) -> Result<(), String> {
        self.pause(live)?;
        if let Err(error) = fs::remove_file(&self.plist)
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(restore_error(
                format!("无法移除 macOS 调度文件：{error}"),
                || {
                    if live.is_some() {
                        self.load()?;
                    }
                    Ok(())
                },
            ));
        }
        Ok(())
    }
}

fn lock_job(job: &Path) -> Result<Option<File>, String> {
    let path = match fs::canonicalize(job) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("无法定位作业锁文件：{error}")),
    };
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(job_lock_path(&path))
        .map_err(|error| format!("无法打开作业锁文件：{error}"))?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(TryLockError::WouldBlock) => Err("作业正在运行或计划正在修改，请稍后重试。".into()),
        Err(TryLockError::Error(error)) => Err(format!("无法锁定作业：{error}")),
    }
}

fn restore_error(error: String, restore: impl FnOnce() -> Result<(), String>) -> String {
    match restore() {
        Ok(()) => error,
        Err(restore) => format!("{error}\n恢复此前的 macOS 调度失败：{restore}"),
    }
}

fn make_plist(
    label: &str,
    executable: &Path,
    job: &Path,
    trigger: &Trigger,
) -> Result<String, String> {
    let path_text = |path: &Path| {
        let text = path.to_str().ok_or("路径包含无法识别的字符。")?;
        if text.chars().any(|c| {
            !matches!(c, '\t' | '\r' | '\n' | '\x20'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
        }) {
            return Err("路径包含 macOS 调度文件无法表示的字符。");
        }
        Ok(escape(text).replace('\r', "&#13;"))
    };
    let executable = path_text(executable)?;
    let job = path_text(job)?;
    let arguments = match trigger.once_timestamp() {
        Some(timestamp) => format!(
            "<string>run-once-at</string><string>{job}</string><string>{timestamp}</string>"
        ),
        None => format!("<string>run</string><string>{job}</string>"),
    };
    let calendar = calendar::plist_calendar(trigger);
    let hint = calendar::monthly_selection_hint(trigger).map_or_else(String::new, |(days, months)| {
        format!(
            "\n    <key>EnvironmentVariables</key>\n    <dict>\n      <key>HARBOR_SQL_MONTHLY_DAY_MASK</key><string>{}</string>\n      <key>HARBOR_SQL_MONTH_MASK</key><string>{}</string>\n    </dict>",
            days.mask(),
            months.mask(),
        )
    });
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>Label</key><string>{label}</string>
    <key>ProgramArguments</key>
    <array><string>{executable}</string>{arguments}</array>
    <key>StartCalendarInterval</key>
    {calendar}{hint}
  </dict>
</plist>
"#
    ))
}
