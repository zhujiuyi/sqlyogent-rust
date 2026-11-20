# 小海豚MysqlGui

小海豚MysqlGui（`sqlyogent-rust`）是用 Rust 和 egui 编写的 MySQL 与 MariaDB 桌面客户端，提供简体中文界面。项目独立开发，操作流程参考 SQLyog。左侧浏览数据库对象，右侧编写 SQL，下方查看查询结果和消息。

## 主要功能

- **连接与迁移**　支持直连、数据库 TLS、SSH 和兼容 SQLyog PHP 8.05 协议的 HTTP/HTTPS 隧道。可导入 `.sycs` 或 `sqlyog.ini` 中能识别的连接设置，使用时重新输入密码。
- **查询与编辑**　支持多连接、多查询页、SQL 补全、格式化、历史和收藏。查询结果可按表格、文本或表单查看。能定位源记录的查询支持编辑，也可从表数据页分页查看和修改记录。
- **数据库管理**　可创建和修改库表、管理索引和外键、查看对象定义。查询设计器可从画布生成 SQL，架构设计器可查看表关系。服务器菜单提供进程、变量、状态和账户管理。
- **导入、导出与备份**　支持 SQL 转储和还原、CSV 和 Excel 2003 XML 导出，以及从文本、Excel、Access 或 ODBC 导入数据。还可生成 HTML 结构说明，或跨库、跨连接复制对象及数据。
- **比较与同步**　可比较库结构和表数据，预览差异并生成同步 SQL。数据同步作业默认只比较，按需启用写入。

## 构建与运行

从此仓库下载或克隆源码，在项目根目录执行对应平台的命令。两套脚本均按 `Cargo.lock` 构建桌面程序和独立作业程序。

### Windows x64

已有 Windows x64 程序包时，解压后运行 `HarborSQL.exe`。把 `HarborSQL-Job.exe` 留在同一目录，供「作业管理」调用。

从源码构建需要 Rust 稳定版的 MSVC 工具链。安装 Visual Studio 或 Build Tools 时选择「使用 C++ 的桌面开发」，并安装 MSVC x64 工具和 Windows SDK。 [微软的 Rust 环境配置说明](https://learn.microsoft.com/en-us/windows/dev-environment/rust/setup)

```powershell
rustup target add x86_64-pc-windows-msvc
.\scripts\package-windows.ps1
.\dist\HarborSQL.exe
```

脚本将 `HarborSQL.exe` 和 `HarborSQL-Job.exe` 写入 `dist/`。Windows 11 x64 上已用 MySQL 8.4 和 MariaDB 11.4 实测连接、查询和常规数据操作。

### macOS 27

在 Apple 芯片 Mac 上安装 Rust 稳定版，并选择带 macOS 27 或更新版 SDK 的 Xcode。脚本的最低部署目标为 macOS 27.0。 [Xcode 与 SDK 对照](https://developer.apple.com/xcode/system-requirements)

```sh
rustup target add aarch64-apple-darwin
xcrun --sdk macosx --show-sdk-version
sh scripts/package-macos.sh
open 'dist/小海豚MysqlGui.app'
```

脚本生成带临时签名的 `dist/小海豚MysqlGui.app`，包内含桌面程序和独立作业程序。对外分发应用包时可使用 Apple Developer ID 签名并公证。 [Apple 签名与公证说明](https://developer.apple.com/developer-id/)

Apple 芯片目标已通过 Rust 交叉类型检查，应用包的构建和运行待 Apple 芯片 Mac 实机验证。

## 常用操作

1. 启动后点「新建连接」，填写服务器地址、端口和数据库账户。用「测试连接」核对设置，再打开连接。
2. 在左侧展开数据库，从表的右键菜单选择「打开表数据」或「查看对象定义」。点击「执行」可运行当前语句或选区，「批量执行」可运行选区或整页脚本。默认快捷键为 `F9` 和 `Shift+F9`。按 `F8` 可执行当前查询并尝试打开可编辑结果。
3. 右键数据库，在「备份与导出」下可将所选对象备份为 SQL 文件，在「导入」下可还原 SQL 文件或从外部数据源导入。同步、作业管理和偏好设置在「工具」菜单。

使用 ODBC 时，Windows x64 需安装 64 位数据源驱动，Apple 芯片 Mac 需安装 arm64 的 unixODBC 和相应驱动。直接读取 CSV、Excel 或 Access 文件时，可在外部导入窗口选择对应的文件来源。

HTTP/HTTPS 隧道需先部署兼容的 PHP 隧道端点。每次请求使用独立数据库会话。需要在多条语句间共享会话，或在备份中保持一致性快照时，使用直连或 SSH。

应用默认在系统用户配置目录保存连接配置、偏好、收藏夹、最近文件和作业列表。启动时可用 `-dir` 选择另一个可写目录。相对路径以可执行文件所在目录为准，例如从项目根目录运行 `.\dist\HarborSQL.exe -dir "harbor-data"` 后，配置存放在 `dist/harbor-data/`。

## 独立后台作业

在「工具 → 作业管理」可以创建或登记 JSON 作业，执行单库或多库备份、数据同步、SQL 脚本与 HTML 报告、外部数据导入。也可导入能识别的 SQLyog 作业 XML，核对转换结果后另存。

以下是单库备份示例。保存为 `backup.json`，将地址、账户和库名换成自己的配置，并为启动作业的进程提供 `HARBOR_BACKUP_PASSWORD` 环境变量。

```json
{
  "version": 1,
  "connection": {
    "host": "127.0.0.1",
    "port": 3306,
    "user": "backup_user",
    "password_env": "HARBOR_BACKUP_PASSWORD"
  },
  "job": {
    "type": "backup",
    "schema": "example_db",
    "output": "backups/example-{unix_ms}.sql"
  },
  "schedule": { "interval_seconds": 3600, "run_immediately": true }
}
```

```powershell
.\dist\HarborSQL-Job.exe check .\backup.json
.\dist\HarborSQL-Job.exe run .\backup.json
```

在 macOS 上，从应用包内调用同一个命令行入口。

```sh
'dist/小海豚MysqlGui.app/Contents/MacOS/harbor-sql-job' check backup.json
'dist/小海豚MysqlGui.app/Contents/MacOS/harbor-sql-job' run backup.json
```

`check` 校验作业定义，`run` 立即执行一次，`watch` 在前台按 `schedule` 循环执行。用 `--help` 查看迁移等命令。作业的相对路径以 JSON 文件所在目录为准，默认日志为同目录下的 `backup.json.log.jsonl`。

「作业管理 → 系统调度」可通过 Windows 任务计划程序或 macOS LaunchAgent 定时执行，桌面关闭后仍可触发。请为任务运行账户或 LaunchAgent 的登录会话提供作业所需的环境变量。

## 源码结构

- 桌面程序包含连接工作区、SQL 编辑器和图形设计器。 [桌面入口](src/main.rs) · [界面代码](src/app/)
- 数据库层处理会话、对象操作和同步，供桌面和作业程序调用。 [数据库入口](src/database.rs) · [数据库模块](src/database/)
- 独立作业程序读取 JSON 并执行任务。 [作业命令行入口](src/bin/harbor-sql-job.rs) · [作业执行代码](src/jobs.rs)
- 两个平台各有打包脚本，Cargo 使用仓库内的 MySQL 驱动补丁。 [脚本目录](scripts/) · [Cargo 配置](Cargo.toml)
