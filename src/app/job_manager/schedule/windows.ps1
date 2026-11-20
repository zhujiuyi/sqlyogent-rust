$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$taskName = 'HarborSQL-Job-' + $sid + '-' + $env:HARBOR_JOB_ID
$owner = 'Harbor SQL job ' + $env:HARBOR_JOB_ID
$service = New-Object -ComObject Schedule.Service
$service.Connect()
try {
    $folder = $service.GetFolder('\')
} catch {
    if ($_.Exception.GetBaseException().HResult -ne -2147024893) { throw }
    [Console]::Error.WriteLine('无法访问任务计划程序根目录。Windows 返回 0x80070003。')
    exit 1
}
$task = $null
try {
    $task = $folder.GetTask($taskName)
} catch {
    # HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)
    if ($_.Exception.GetBaseException().HResult -ne -2147024894) { throw }
}
if ($null -ne $task -and $task.Definition.RegistrationInfo.Description -cne $owner) {
    [Console]::WriteLine('conflict')
    exit 0
}
$culture = [Globalization.CultureInfo]::InvariantCulture

function Resolve-AccountSid([string]$account) {
    if ($account -cmatch '\AS-1-\d+(?:-\d+)+\z') {
        return [System.Security.Principal.SecurityIdentifier]::new($account).Value
    }
    return [System.Security.Principal.NTAccount]::new($account).Translate([System.Security.Principal.SecurityIdentifier]).Value
}

function Get-AccountLabel([string]$accountSid) {
    try {
        return [System.Security.Principal.SecurityIdentifier]::new($accountSid).Translate([System.Security.Principal.NTAccount]).Value
    } catch [System.Security.Principal.IdentityNotMappedException] {
        return $accountSid
    }
}

switch ($env:HARBOR_JOB_CHANGE) {
    'save' {
        $definition = $service.NewTask(0)
        $definition.RegistrationInfo.Description = $owner
        $definition.Principal.LogonType = 3 # TASK_LOGON_INTERACTIVE_TOKEN
        $definition.Settings.DisallowStartIfOnBatteries = $false
        $definition.Settings.StopIfGoingOnBatteries = $false
        $definition.Settings.ExecutionTimeLimit = 'PT0S'

        $kind = $env:HARBOR_JOB_TRIGGER
        if ($kind -cne 'logon') {
            $clock = [TimeSpan]::ParseExact($env:HARBOR_JOB_TIME, 'hh\:mm', $culture)
            $weekdays = 0
            if ($env:HARBOR_JOB_WEEKDAYS -and
                ((-not [int]::TryParse($env:HARBOR_JOB_WEEKDAYS, [ref]$weekdays)) -or $weekdays -lt 0 -or $weekdays -gt 127)) {
                throw '每周调度的日期选择无效。'
            }
            $now = [DateTime]::Now
            $start = $now.Date.Add($clock)
            if ($start -le $now) { $start = $start.AddDays(1) }
            if (-not $kind) {
                $kind = if ($weekdays -eq 0) { 'daily' } else { 'weekly' }
            }
        }
        $runSid = $sid
        switch -CaseSensitive ($kind) {
            'daily' {
                $trigger = $definition.Triggers.Create(2) # TASK_TRIGGER_DAILY
                $trigger.DaysInterval = 1
            }
            'weekly' {
                if ($weekdays -eq 0) { throw '每周调度需要选择运行日。' }
                while (($weekdays -band (1 -shl [int]$start.DayOfWeek)) -eq 0) {
                    $start = $start.AddDays(1)
                }
                $trigger = $definition.Triggers.Create(3) # TASK_TRIGGER_WEEKLY
                $trigger.DaysOfWeek = [int16]$weekdays
                $trigger.WeeksInterval = 1
            }
            'monthly' {
                $days = 0
                $months = 0
                if (-not [int]::TryParse($env:HARBOR_JOB_MONTH_DAYS, [ref]$days) -or $days -le 0 -or
                    -not [int]::TryParse($env:HARBOR_JOB_MONTHS, [ref]$months) -or $months -le 0 -or $months -gt 4095) {
                    throw '每月调度需要选择有效的月份和日期。'
                }

                $month = $start.Date.AddDays(1 - $start.Day)
                $firstDay = $start.Day
                $found = $false
                # A valid Gregorian leap-day pattern can have a gap of eight years.
                for ($offset = 0; $offset -le 96 -and -not $found; $offset++) {
                    if (($months -band (1 -shl ($month.Month - 1))) -ne 0) {
                        $last = [DateTime]::DaysInMonth($month.Year, $month.Month)
                        for ($day = $firstDay; $day -le $last; $day++) {
                            if (($days -band (1 -shl ($day - 1))) -ne 0) {
                                $start = $month.AddDays($day - 1).Add($clock)
                                $found = $true
                                break
                            }
                        }
                    }
                    $month = $month.AddMonths(1)
                    $firstDay = 1
                }
                if (-not $found) { throw '所选月份没有可以运行的日期。' }

                $trigger = $definition.Triggers.Create(4) # TASK_TRIGGER_MONTHLY
                $trigger.DaysOfMonth = $days
                $trigger.MonthsOfYear = [int16]$months
            }
            'once' {
                $date = [DateTime]::MinValue
                if (-not [DateTime]::TryParseExact($env:HARBOR_JOB_DATE, 'yyyy-MM-dd', $culture, [Globalization.DateTimeStyles]::None, [ref]$date)) {
                    throw '单次调度需要有效的日期。'
                }
                $start = $date.Add($clock)
                if ($start -le $now) { throw '单次调度的时间必须晚于现在。' }
                $trigger = $definition.Triggers.Create(1) # TASK_TRIGGER_TIME
            }
            'logon' {
                if ($env:HARBOR_JOB_LOGON_ACCOUNT) {
                    $runSid = Resolve-AccountSid $env:HARBOR_JOB_LOGON_ACCOUNT
                }
                $trigger = $definition.Triggers.Create(9) # TASK_TRIGGER_LOGON
                $trigger.UserId = $runSid
            }
            default { throw '无法识别系统调度类型。' }
        }
        if ($kind -cne 'logon') {
            $trigger.StartBoundary = $start.ToString('s', $culture)
        }
        $definition.Principal.UserId = $runSid

        $action = $definition.Actions.Create(0) # TASK_ACTION_EXEC
        $action.Path = $env:HARBOR_JOB_PROGRAM
        $action.Arguments = $env:HARBOR_JOB_ARGS
        $mode = if ($null -eq $task) { 2 } else { 4 } # TASK_CREATE / TASK_UPDATE
        $task = $folder.RegisterTaskDefinition($taskName, $definition, $mode, $runSid, $null, 3, $null)
    }
    'cancel' {
        if ($null -ne $task) {
            $folder.DeleteTask($taskName, 0)
            $task = $null
        }
    }
    'inspect' {}
    default { exit 1 }
}

if ($null -eq $task) {
    [Console]::WriteLine('absent')
} else {
    $enabled = $task.Enabled
    $schedule = ''
    $definition = $task.Definition
    $triggers = $definition.Triggers
    if ($triggers.Count -eq 1) {
        $trigger = $triggers.Item(1)
        $enabled = $enabled -and $trigger.Enabled
        $once = $trigger.Type -eq 1
        $daily = $trigger.Type -eq 2 -and $trigger.DaysInterval -eq 1
        $weekly = $trigger.Type -eq 3 -and $trigger.WeeksInterval -eq 1 -and
            $trigger.DaysOfWeek -gt 0 -and $trigger.DaysOfWeek -le 127
        $monthly = $trigger.Type -eq 4 -and -not $trigger.RunOnLastDayOfMonth -and
            $trigger.DaysOfMonth -gt 0 -and $trigger.DaysOfMonth -le 2147483647 -and
            $trigger.MonthsOfYear -gt 0 -and $trigger.MonthsOfYear -le 4095
        if ($trigger.Type -eq 9 -and $trigger.UserId -and $definition.Principal.UserId -and
            $definition.Principal.LogonType -eq 3 -and -not $trigger.StartBoundary -and
            -not $trigger.EndBoundary -and -not $trigger.Repetition.Interval -and
            -not $trigger.Repetition.Duration -and (-not $trigger.Delay -or $trigger.Delay -ceq 'PT0S')) {
            try {
                $triggerSid = Resolve-AccountSid $trigger.UserId
                $principalSid = Resolve-AccountSid $definition.Principal.UserId
                if ($triggerSid -ceq $principalSid) {
                    $schedule = if ($triggerSid -ceq $sid) {
                        'logon'
                    } else {
                        'logon|' + (Get-AccountLabel $triggerSid)
                    }
                }
            } catch [System.Security.Principal.IdentityNotMappedException] {
                # An externally changed or removed account remains cancellable.
            }
        } elseif ($daily -or $weekly -or $monthly -or $once) {
            $start = [DateTime]::MinValue
            $simple = -not $trigger.EndBoundary -and -not $trigger.Repetition.Interval -and
                -not $trigger.Repetition.Duration -and
                (-not $trigger.RandomDelay -or $trigger.RandomDelay -ceq 'PT0S')
            if ($simple -and
                [DateTime]::TryParseExact($trigger.StartBoundary, 's', $culture, [Globalization.DateTimeStyles]::None, [ref]$start) -and
                $start.Second -eq 0) {
                $time = $start.ToString('HH:mm', $culture)
                $schedule = if ($weekly) {
                    $time + '|' + ([int]$trigger.DaysOfWeek).ToString($culture)
                } elseif ($monthly) {
                    'monthly|' + $time + '|' + ([int]$trigger.DaysOfMonth).ToString($culture) +
                        '|' + ([int]$trigger.MonthsOfYear).ToString($culture)
                } elseif ($once) {
                    'once|' + $start.ToString('yyyy-MM-dd', $culture) + '|' + $time
                } else {
                    $time
                }
            }
        }
    }
    $state = if ($enabled) { 'enabled' } else { 'disabled' }
    [Console]::WriteLine("$state|$schedule")
}
