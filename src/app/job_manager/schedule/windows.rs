use std::path::{Path, PathBuf};
use std::process::Command;

use base64::{Engine as _, engine::general_purpose::STANDARD};

use super::{
    Change, LocalTime, MonthDays, Months, Status, Trigger, Weekdays, checked, id, schedule_date,
    validated_runner,
};

pub(super) fn apply(change: Change, job: &Path) -> Result<Status, String> {
    let powershell =
        PathBuf::from(std::env::var_os("SystemRoot").ok_or("无法定位 Windows 系统目录。")?)
            .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let mut command = Command::new(&powershell);
    command
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
        .arg(include_str!("windows.ps1"))
        .env("HARBOR_JOB_ID", id(job)?);

    match change {
        Change::Inspect => command.env("HARBOR_JOB_CHANGE", "inspect"),
        Change::Save(trigger) => {
            let executable = validated_runner(job)?;
            let (program, arguments) = action(&powershell, &executable, job)?;
            command
                .env("HARBOR_JOB_CHANGE", "save")
                .env("HARBOR_JOB_PROGRAM", program)
                .env("HARBOR_JOB_ARGS", arguments);
            if let Some(time) = trigger.time() {
                command.env("HARBOR_JOB_TIME", time.to_string());
            }
            match trigger {
                Trigger::Daily(_) => command
                    .env("HARBOR_JOB_TRIGGER", "daily")
                    .env("HARBOR_JOB_WEEKDAYS", "0"),
                Trigger::Weekly { days, .. } => command
                    .env("HARBOR_JOB_TRIGGER", "weekly")
                    .env("HARBOR_JOB_WEEKDAYS", days.mask().to_string()),
                Trigger::Monthly { days, months, .. } => command
                    .env("HARBOR_JOB_TRIGGER", "monthly")
                    .env("HARBOR_JOB_WEEKDAYS", "0")
                    .env("HARBOR_JOB_MONTH_DAYS", days.mask().to_string())
                    .env("HARBOR_JOB_MONTHS", months.mask().to_string()),
                Trigger::Once { date, .. } => command
                    .env("HARBOR_JOB_TRIGGER", "once")
                    .env("HARBOR_JOB_WEEKDAYS", "0")
                    .env("HARBOR_JOB_DATE", date.format("%Y-%m-%d").to_string()),
                Trigger::Logon { account } => command
                    .env("HARBOR_JOB_TRIGGER", "logon")
                    .env("HARBOR_JOB_LOGON_ACCOUNT", account.unwrap_or_default()),
            }
        }
        Change::Cancel => command.env("HARBOR_JOB_CHANGE", "cancel"),
    };

    let output = checked(&mut command, "Windows 任务计划程序操作失败")?;
    parse_status(output.stdout.trim())
}

fn parse_status(status: &str) -> Result<Status, String> {
    match status {
        "absent" => return Ok(Status::Absent),
        "conflict" => return Err("Windows 任务计划程序中已有同名的其他任务。".into()),
        _ => {}
    }
    let (state, schedule) = status
        .split_once('|')
        .ok_or("无法识别 Windows 调度状态。")?;
    let enabled = match state {
        "enabled" => true,
        "disabled" => false,
        _ => return Err("无法识别 Windows 调度状态。".into()),
    };
    let trigger = if schedule == "logon" {
        Trigger::logon(None)
    } else if let Some(account) = schedule.strip_prefix("logon|") {
        Trigger::logon(Some(account))
    } else if let Some(monthly) = schedule.strip_prefix("monthly|") {
        parse_monthly(monthly)
    } else if let Some(once) = schedule.strip_prefix("once|") {
        parse_once(once)
    } else if let Some((time, weekdays)) = schedule.split_once('|') {
        LocalTime::parse(time)
            .zip(weekdays.parse().ok().and_then(Weekdays::from_mask))
            .map(|(time, days)| Trigger::Weekly { time, days })
    } else {
        LocalTime::parse(schedule).map(Trigger::Daily)
    };
    Ok(Status::Registered { trigger, enabled })
}

fn parse_monthly(schedule: &str) -> Option<Trigger> {
    let (time, masks) = schedule.split_once('|')?;
    let (days, months) = masks.split_once('|')?;
    Trigger::monthly(
        LocalTime::parse(time)?,
        MonthDays::from_mask(days.parse().ok()?)?,
        Months::from_mask(months.parse().ok()?)?,
    )
}

fn parse_once(schedule: &str) -> Option<Trigger> {
    let (date, time) = schedule.split_once('|')?;
    Trigger::once(schedule_date(date)?, LocalTime::parse(time)?)
}

fn action<'a>(
    powershell: &'a Path,
    executable: &'a Path,
    job: &Path,
) -> Result<(&'a Path, String), String> {
    let file = job.to_str().ok_or("作业路径包含无法识别的字符。")?;
    let program = executable.to_str().ok_or("程序路径包含无法识别的字符。")?;
    let arguments = format!(r#"run "{file}""#);
    if !file.contains('%') && !program.contains('%') {
        return Ok((executable, arguments));
    }

    // Task Scheduler expands percent variables in execution paths and arguments.
    // Keep literal percent characters in these paths by encoding the PowerShell action.
    let program = program.replace('\'', "''");
    let arguments = arguments.replace('\'', "''");
    let script = format!(
        "$ErrorActionPreference = 'Stop'\n\
         $process = Start-Process -FilePath '{program}' -ArgumentList '{arguments}' -WindowStyle Hidden -Wait -PassThru\n\
         exit $process.ExitCode"
    );
    let encoded = STANDARD.encode(
        script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    Ok((
        powershell,
        format!("-NoLogo -NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand {encoded}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedules_the_runner_with_a_single_job_path() {
        let powershell = Path::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe");
        let executable = Path::new(r"C:\Program Files\港湾 SQL\HarborSQL-Job.exe");
        let job = Path::new(r"C:\我的作业\夜间备份 (A&B).json");
        let (program, arguments) = action(powershell, executable, job).unwrap();
        assert_eq!(program, executable);
        assert_eq!(arguments, r#"run "C:\我的作业\夜间备份 (A&B).json""#);
    }

    #[test]
    fn inspects_single_task_trigger_metadata() {
        let daily = parse_status("enabled|06:30").unwrap();
        assert_eq!(daily.label(), "每天 06:30（已启用）");
        let weekly = parse_status("disabled|06:30|34").unwrap();
        assert_eq!(weekly.label(), "每周一、周五 06:30（当前未启用）");
        let monthly = parse_status("enabled|monthly|06:30|536870913|5").unwrap();
        assert_eq!(
            monthly.trigger(),
            Trigger::monthly(
                LocalTime::parse("06:30").unwrap(),
                MonthDays::from_mask(1 | (1 << 29)).unwrap(),
                Months::from_mask(1 | (1 << 2)).unwrap(),
            )
            .as_ref()
        );
        let once = parse_status("enabled|once|2030-05-07|09:05").unwrap();
        assert_eq!(
            once.trigger(),
            Trigger::once(
                chrono::NaiveDate::from_ymd_opt(2030, 5, 7).unwrap(),
                LocalTime::parse("09:05").unwrap(),
            )
            .as_ref()
        );
    }

    #[test]
    fn restores_current_and_named_logon_triggers_in_the_job_form() {
        use super::super::Fields;

        let current = parse_status("enabled|logon").unwrap();
        assert_eq!(current.label(), "当前 Windows 账户登录时（已启用）");
        let named = parse_status(r"enabled|logon|WORKGROUP\operator").unwrap();
        assert_eq!(named.label(), r"WORKGROUP\operator登录时（已启用）");
        for status in [current, named] {
            let fields = Fields::for_authoring(status.trigger().cloned());
            assert_eq!(fields.trigger().as_ref(), status.trigger());
            assert!(status.trigger().unwrap().time().is_none());
        }
    }
}
