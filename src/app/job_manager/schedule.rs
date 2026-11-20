use std::fmt;
use std::num::{NonZeroU8, NonZeroU16, NonZeroU32};
use std::path::Path;

use chrono::{Datelike as _, Days, Local, NaiveDate, NaiveDateTime, TimeZone as _};
use eframe::egui::{self, TextEdit};
#[cfg(any(windows, target_os = "macos"))]
use sha2::{Digest, Sha256};

#[cfg(any(test, target_os = "macos"))]
mod calendar;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

pub(super) const SUPPORTED: bool = cfg!(any(windows, target_os = "macos"));

#[cfg(windows)]
pub(super) const ACCOUNT_NOTE: &str =
    "由 Windows 任务计划程序启动独立作业。请为运行账户提供作业所需的环境变量。";
#[cfg(target_os = "macos")]
pub(super) const ACCOUNT_NOTE: &str = "当前 macOS 账户登录后按计划时间运行。登录事件触发仅适用于 Windows。请在登录会话中提供作业所需的环境变量。";
#[cfg(not(any(windows, target_os = "macos")))]
pub(super) const ACCOUNT_NOTE: &str = "";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LocalTime {
    hour: u8,
    minute: u8,
}

impl LocalTime {
    fn parse(input: &str) -> Option<Self> {
        let (hour, minute) = input.split_once(':')?;
        if hour.len() != 2
            || minute.len() != 2
            || !hour
                .bytes()
                .chain(minute.bytes())
                .all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        Self::new(hour.parse().ok()?, minute.parse().ok()?)
    }

    fn new(hour: u8, minute: u8) -> Option<Self> {
        (hour < 24 && minute < 60).then_some(Self { hour, minute })
    }
}

fn schedule_date(input: &str) -> Option<NaiveDate> {
    let date = NaiveDate::parse_from_str(input, "%Y-%m-%d").ok()?;
    ((1..=9999).contains(&date.year()) && date.format("%Y-%m-%d").to_string() == input)
        .then_some(date)
}

impl fmt::Display for LocalTime {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(output, "{:02}:{:02}", self.hour, self.minute)
    }
}

// Both Task Scheduler and launchd number Sunday first.
const DAY_LABELS: [&str; 7] = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];
const MONTH_MAX_DAYS: [u8; 12] = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Weekdays(NonZeroU8);

impl Weekdays {
    fn from_mask(mask: u8) -> Option<Self> {
        if mask >= 1 << DAY_LABELS.len() {
            return None;
        }
        NonZeroU8::new(mask).map(Self)
    }

    fn mask(self) -> u8 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MonthDays(NonZeroU32);

impl MonthDays {
    fn from_mask(mask: u32) -> Option<Self> {
        if mask >= 1 << 31 {
            return None;
        }
        NonZeroU32::new(mask).map(Self)
    }

    fn mask(self) -> u32 {
        self.0.get()
    }

    fn occurs_in(self, months: Months) -> bool {
        MONTH_MAX_DAYS.iter().enumerate().any(|(index, &limit)| {
            months.mask() & (1 << index) != 0 && self.mask() & ((1 << limit) - 1) != 0
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Months(NonZeroU16);

impl Months {
    const ALL: u16 = (1 << MONTH_MAX_DAYS.len()) - 1;

    fn from_mask(mask: u16) -> Option<Self> {
        if mask > Self::ALL {
            return None;
        }
        NonZeroU16::new(mask).map(Self)
    }

    fn mask(self) -> u16 {
        self.0.get()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Trigger {
    Daily(LocalTime),
    Once {
        date: NaiveDate,
        time: LocalTime,
    },
    Weekly {
        time: LocalTime,
        days: Weekdays,
    },
    Monthly {
        time: LocalTime,
        days: MonthDays,
        months: Months,
    },
    #[cfg(windows)]
    Logon {
        account: Option<String>,
    },
}

impl Trigger {
    fn once(date: NaiveDate, time: LocalTime) -> Option<Self> {
        (1..=9999)
            .contains(&date.year())
            .then_some(Self::Once { date, time })
    }

    fn once_datetime(&self) -> Option<NaiveDateTime> {
        let Self::Once { date, time } = self else {
            return None;
        };
        date.and_hms_opt(u32::from(time.hour), u32::from(time.minute), 0)
    }

    #[cfg(any(test, target_os = "macos"))]
    fn once_timestamp(&self) -> Option<String> {
        self.once_datetime()
            .map(|date| date.format("%Y-%m-%dT%H:%M").to_string())
    }

    fn monthly(time: LocalTime, days: MonthDays, months: Months) -> Option<Self> {
        days.occurs_in(months)
            .then_some(Self::Monthly { time, days, months })
    }

    #[cfg(windows)]
    fn logon(account: Option<&str>) -> Option<Self> {
        let account = if let Some(account) = account.map(str::trim) {
            if account.is_empty() || account.chars().any(char::is_control) {
                return None;
            }
            Some(account.to_owned())
        } else {
            None
        };
        Some(Self::Logon { account })
    }

    fn time(&self) -> Option<LocalTime> {
        match self {
            Self::Daily(time)
            | Self::Once { time, .. }
            | Self::Weekly { time, .. }
            | Self::Monthly { time, .. } => Some(*time),
            #[cfg(windows)]
            Self::Logon { .. } => None,
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Daily(time) => format!("每天 {time}"),
            Self::Once { date, time } => format!("仅一次 {} {time}", date.format("%Y-%m-%d")),
            Self::Weekly { time, days } => {
                let names = (1..7)
                    .chain([0])
                    .filter(|&day| days.mask() & (1 << day) != 0)
                    .map(|day| DAY_LABELS[day])
                    .collect::<Vec<_>>()
                    .join("、");
                format!("每{names} {time}")
            }
            Self::Monthly { time, days, months } => {
                let dates = (1..=31)
                    .filter(|day| days.mask() & (1 << (day - 1)) != 0)
                    .map(|day| format!("{day}日"))
                    .collect::<Vec<_>>()
                    .join("、");
                if months.mask() == Months::ALL {
                    format!("每月{dates} {time}")
                } else {
                    let selection = (1..=12)
                        .filter(|month| months.mask() & (1 << (month - 1)) != 0)
                        .map(|month| format!("{month}月"))
                        .collect::<Vec<_>>()
                        .join("、");
                    format!("每年{selection} · {dates} {time}")
                }
            }
            #[cfg(windows)]
            Self::Logon { account } => format!(
                "{}登录时",
                account.as_deref().unwrap_or("当前 Windows 账户")
            ),
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Frequency {
    #[default]
    Daily,
    Weekly,
    Monthly,
    Once,
    #[cfg(windows)]
    Logon,
}

pub(super) struct Fields {
    time: String,
    date: String,
    frequency: Frequency,
    weekdays: u8,
    month_days: u32,
    months: u16,
    #[cfg(windows)]
    account: Option<String>,
}

impl Default for Fields {
    fn default() -> Self {
        let today = Local::now().date_naive();
        Self {
            time: String::new(),
            date: today
                .checked_add_days(Days::new(1))
                .unwrap_or(today)
                .format("%Y-%m-%d")
                .to_string(),
            frequency: Frequency::Daily,
            weekdays: 0,
            month_days: 1,
            months: Months::ALL,
            #[cfg(windows)]
            account: None,
        }
    }
}

impl Fields {
    pub(super) fn for_authoring(trigger: Option<Trigger>) -> Self {
        let mut form = Self {
            time: "02:00".into(),
            ..Self::default()
        };
        if let Some(trigger) = trigger {
            form.set(&trigger);
        }
        form
    }

    pub(super) fn set(&mut self, trigger: &Trigger) {
        if let Some(time) = trigger.time() {
            self.time = time.to_string();
        }
        self.frequency = match trigger {
            Trigger::Daily(_) => Frequency::Daily,
            Trigger::Once { date, .. } => {
                self.date = date.format("%Y-%m-%d").to_string();
                Frequency::Once
            }
            Trigger::Weekly { days, .. } => {
                self.weekdays = days.mask();
                Frequency::Weekly
            }
            Trigger::Monthly { days, months, .. } => {
                self.month_days = days.mask();
                self.months = months.mask();
                Frequency::Monthly
            }
            #[cfg(windows)]
            Trigger::Logon { account } => {
                self.account.clone_from(account);
                Frequency::Logon
            }
        };
    }

    pub(super) fn trigger(&self) -> Option<Trigger> {
        let time = || LocalTime::parse(&self.time);
        match self.frequency {
            Frequency::Daily => Some(Trigger::Daily(time()?)),
            Frequency::Once => Trigger::once(schedule_date(&self.date)?, time()?),
            Frequency::Weekly => Some(Trigger::Weekly {
                time: time()?,
                days: Weekdays::from_mask(self.weekdays)?,
            }),
            Frequency::Monthly => Trigger::monthly(
                time()?,
                MonthDays::from_mask(self.month_days)?,
                Months::from_mask(self.months)?,
            ),
            #[cfg(windows)]
            Frequency::Logon => Trigger::logon(self.account.as_deref()),
        }
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.frequency, Frequency::Daily, "每天");
            ui.selectable_value(&mut self.frequency, Frequency::Weekly, "每周");
            ui.selectable_value(&mut self.frequency, Frequency::Monthly, "每月");
            ui.selectable_value(&mut self.frequency, Frequency::Once, "仅一次");
            #[cfg(windows)]
            {
                ui.selectable_value(&mut self.frequency, Frequency::Logon, "登录时");
                if self.frequency == Frequency::Logon {
                    return;
                }
            }
            ui.add(
                TextEdit::singleline(&mut self.time)
                    .char_limit(5)
                    .desired_width(64.0)
                    .hint_text("HH:mm"),
            );
            ui.label("系统本地时间");
        });
        #[cfg(windows)]
        if self.frequency == Frequency::Logon {
            ui.horizontal_wrapped(|ui| {
                ui.label("登录账户");
                let was_named = self.account.is_some();
                let mut named = was_named;
                ui.radio_value(&mut named, false, "当前 Windows 账户");
                ui.radio_value(&mut named, true, "指定账户");
                if named != was_named {
                    self.account = named.then(String::new);
                }
                if let Some(account) = &mut self.account {
                    ui.add(TextEdit::singleline(account).hint_text(r"域或计算机名\用户名"));
                }
            });
            if self.account.is_some() {
                ui.label("为其他账户登记调度需要 Windows 管理员权限。");
            }
            return;
        }
        if self.frequency == Frequency::Once {
            ui.horizontal_wrapped(|ui| {
                ui.label("运行日期");
                ui.add(
                    TextEdit::singleline(&mut self.date)
                        .char_limit(10)
                        .desired_width(106.0)
                        .hint_text("YYYY-MM-DD"),
                );
                if schedule_date(&self.date).is_none() {
                    ui.label("请输入有效日期。");
                }
            });
        } else if self.frequency == Frequency::Weekly {
            ui.horizontal_wrapped(|ui| {
                for day in (1..7).chain([0]) {
                    let bit = 1 << day;
                    let mut selected = self.weekdays & bit != 0;
                    if ui.checkbox(&mut selected, DAY_LABELS[day]).changed() {
                        self.weekdays ^= bit;
                    }
                }
            });
            if self.weekdays == 0 {
                ui.label("请至少选择一天。");
            }
        } else if self.frequency == Frequency::Monthly {
            ui.label("运行日期");
            ui.horizontal_wrapped(|ui| {
                for day in 1..=31 {
                    let bit = 1 << (day - 1);
                    let mut selected = self.month_days & bit != 0;
                    if ui.checkbox(&mut selected, day.to_string()).changed() {
                        self.month_days ^= bit;
                    }
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("运行月份");
                if ui.button("全选").clicked() {
                    self.months = Months::ALL;
                }
                if ui.button("清空").clicked() {
                    self.months = 0;
                }
            });
            ui.horizontal_wrapped(|ui| {
                for month in 1..=12 {
                    let bit = 1 << (month - 1);
                    let mut selected = self.months & bit != 0;
                    if ui.checkbox(&mut selected, format!("{month}月")).changed() {
                        self.months ^= bit;
                    }
                }
            });
            match (
                MonthDays::from_mask(self.month_days),
                Months::from_mask(self.months),
            ) {
                (Some(days), Some(months)) => {
                    if !days.occurs_in(months) {
                        ui.label("所选月份没有这些日期。");
                    }
                }
                _ => {
                    ui.label("请至少选择一个日期和一个月份。");
                }
            }
        }
    }
}

#[derive(Clone)]
pub(super) enum Change {
    Inspect,
    Save(Trigger),
    Cancel,
}

impl Change {
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Inspect => "查询调度",
            Self::Save(_) => "保存调度",
            Self::Cancel => "取消调度",
        }
    }
}

pub(super) enum Status {
    Absent,
    Registered {
        trigger: Option<Trigger>,
        enabled: bool,
    },
}

impl Status {
    pub(super) fn trigger(&self) -> Option<&Trigger> {
        match self {
            Self::Registered { trigger, .. } => trigger.as_ref(),
            Self::Absent => None,
        }
    }

    pub(super) fn label(&self) -> String {
        match self {
            Self::Absent => "未登记系统调度".into(),
            Self::Registered { trigger, enabled } => {
                let status = if *enabled {
                    "已启用"
                } else {
                    "当前未启用"
                };
                trigger.as_ref().map_or_else(
                    || format!("系统调度{status}，请在系统中查看计划"),
                    |trigger| {
                        if let Trigger::Once { date, .. } = trigger
                            && *date < Local::now().date_naive()
                        {
                            format!("{}（已过计划日期）", trigger.label())
                        } else {
                            format!("{}（{status}）", trigger.label())
                        }
                    },
                )
            }
        }
    }
}

pub(super) fn apply(change: Change, job: &Path) -> Result<Status, String> {
    if let Change::Save(trigger) = &change
        && let Some(date) = trigger.once_datetime()
    {
        let now = Local::now();
        if date <= now.naive_local() {
            return Err("仅一次调度的时间须晚于当前时间。".into());
        }
        if Local.from_local_datetime(&date).earliest().is_none() {
            return Err("所选时间在当前系统时区中不存在。".into());
        }
    }
    #[cfg(windows)]
    return windows::apply(change, job);
    #[cfg(target_os = "macos")]
    return macos::apply(change, job);
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (change, job);
        Err("当前系统不支持系统调度。".into())
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn id(job: &Path) -> Result<String, String> {
    let path = job.to_str().ok_or("作业路径包含无法识别的字符。")?;
    Ok(format!("{:x}", Sha256::digest(path.as_bytes())))
}

#[cfg(any(windows, target_os = "macos"))]
fn validated_runner(job: &Path) -> Result<std::path::PathBuf, String> {
    super::backing_file(job)?;
    let executable = super::companion()?;
    let output = super::run(&executable, super::Operation::Check, job)?;
    if output.code == Some(0) {
        Ok(executable)
    } else if output.stderr.trim().is_empty() {
        Err("作业定义检查失败。".into())
    } else {
        Err(format!("作业定义检查失败：{}", output.stderr.trim()))
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn checked(command: &mut std::process::Command, context: &str) -> Result<super::Output, String> {
    let output = super::run_command(command).map_err(|error| format!("{context}：{error}"))?;
    if output.code == Some(0) {
        return Ok(output);
    }
    let detail = output.stderr.lines().find(|line| !line.trim().is_empty());
    Err(match detail {
        Some(detail) => format!("{context}：{}", detail.trim()),
        None => match output.code {
            Some(code) => format!("{context}（退出码 {code}）。"),
            None => format!("{context}。"),
        },
    })
}

#[cfg(all(test, windows))]
mod tests {
    use std::time::{Duration, Instant};

    use chrono::Timelike as _;

    use super::*;

    #[test]
    #[ignore = "requires an isolated MariaDB, an action runner beside the test executable and Task Scheduler"]
    fn monthly_task_runs_standalone_and_reads_back_selected_dates() {
        let path =
            std::fs::canonicalize(std::env::var_os("HARBOR_MONTHLY_FIXTURE_JOB").unwrap()).unwrap();
        let log = path.with_file_name("monthly.jsonl");
        let report = path.with_file_name("monthly.html");
        assert!(matches!(
            apply(Change::Inspect, &path).unwrap(),
            Status::Absent
        ));
        assert!(!log.exists() && !report.exists());

        let now = chrono::Local::now();
        let delay = if now.second() >= 35 { 2 } else { 1 };
        let at = now + chrono::Duration::minutes(delay);
        let time = LocalTime::new(at.hour() as u8, at.minute() as u8).unwrap();
        let days = MonthDays::from_mask((1 << at.day0()) | (1 << 30)).unwrap();
        let months =
            Months::from_mask((1 << at.month0()) | (1 << ((at.month0() + 1) % 12))).unwrap();
        let trigger = Trigger::monthly(time, days, months).unwrap();
        assert_eq!(
            Fields::for_authoring(Some(trigger.clone()))
                .trigger()
                .as_ref(),
            Some(&trigger)
        );
        let result = std::panic::catch_unwind(|| {
            for change in [Change::Save(trigger.clone()), Change::Inspect] {
                let status = apply(change, &path).unwrap();
                assert!(
                    matches!(
                        &status,
                        Status::Registered { trigger: Some(found), enabled: true } if found == &trigger
                    ),
                    "{}",
                    status.label()
                );
            }
            let start = Instant::now();
            loop {
                if std::fs::read_to_string(&log)
                    .is_ok_and(|log| log.contains("\"event\":\"succeeded\""))
                {
                    break;
                }
                assert!(
                    start.elapsed() < Duration::from_secs(115),
                    "monthly action did not finish in time"
                );
                std::thread::sleep(Duration::from_millis(500));
            }
            assert!(
                std::fs::read_to_string(&report)
                    .unwrap()
                    .contains("harbor_monthly_job_marker")
            );
        });
        let cancelled = apply(Change::Cancel, &path);
        if let Err(error) = result {
            assert!(
                matches!(cancelled, Ok(Status::Absent)),
                "failed to clean up the fixture schedule"
            );
            std::panic::resume_unwind(error);
        }
        assert!(matches!(cancelled.unwrap(), Status::Absent));
        assert!(matches!(
            apply(Change::Inspect, &path).unwrap(),
            Status::Absent
        ));
    }
}
