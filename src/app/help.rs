use eframe::egui::{self, Align, Button, Grid, Layout, RichText, ScrollArea, Ui};

use super::{preferences::Preferences, state, style};

#[derive(Clone, Copy)]
pub(super) enum Page {
    Guide,
    Shortcuts,
    About,
}

impl Page {
    fn title(self) -> &'static str {
        match self {
            Self::Guide => "使用帮助",
            Self::Shortcuts => "快捷键",
            Self::About => "关于小海豚MysqlGui",
        }
    }
}

pub(super) fn menu(ui: &mut Ui, page: &mut Option<Page>) {
    style::menu(ui, "帮助", |ui| {
        if ui
            .add(Button::new("使用帮助").shortcut_text("F1"))
            .clicked()
        {
            *page = Some(Page::Guide);
            ui.close();
        }
        for next in [Page::Shortcuts, Page::About] {
            if ui.button(next.title()).clicked() {
                *page = Some(next);
                ui.close();
            }
        }
    });
}

pub(super) fn dialog(ctx: &egui::Context, open: &mut Option<Page>, preferences: &Preferences) {
    let Some(page) = *open else {
        return;
    };
    let mut next = Some(page);
    let modal = style::modal(ctx, egui::Id::new("help")).show(ctx, |ui| {
        ui.set_width((ctx.content_rect().width() - 48.0).clamp(260.0, 580.0));
        ui.heading(page.title());
        ui.add_space(4.0);
        ui.separator();
        ui.add_space(4.0);
        ScrollArea::vertical()
            .id_salt(page.title())
            .auto_shrink([false, true])
            .max_height(ctx.content_rect().height() * 0.62)
            .show(ui, |ui| match page {
                Page::Guide => {
                    guide(ui);
                    ui.add_space(4.0);
                    if ui.button("查看快捷键").clicked() {
                        next = Some(Page::Shortcuts);
                    }
                }
                Page::Shortcuts => shortcuts(ui, preferences),
                Page::About => about(ui),
            });
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if !matches!(page, Page::Guide) && ui.button("返回使用帮助").clicked() {
                next = Some(Page::Guide);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(style::primary("关闭")).clicked() {
                    next = None;
                }
            });
        });
    });
    *open = if modal.should_close() { None } else { next };
}

fn guide(ui: &mut Ui) {
    for (title, instructions) in [
        (
            "连接数据库",
            "点击「新建连接」，填写服务器地址、端口和账户。连接前可测试配置。需要隧道或证书时，在连接窗口选择相应选项。",
        ),
        (
            "查询与编辑",
            "在查询页编写或打开 SQL，使用「查询」菜单运行当前语句、选区或全页脚本。展开左侧数据库并双击表，可分页浏览和修改数据。",
        ),
        (
            "管理数据库对象",
            "右键数据库、表或视图，可打开对应的创建、修改、复制、导入或导出操作。涉及数据库变更的操作可在执行前核对 SQL 或目标。",
        ),
        (
            "备份与还原",
            "右键数据库，在「备份与导出」中选择「备份数据库为 SQL 文件」。可逐个选择表、视图、存储过程、函数、触发器和事件。还原 SQL 文件可使用数据库右键菜单中的「导入」。",
        ),
        (
            "同步、设计与计划任务",
            "通过「工具」菜单比较数据库、打开图形设计器、创建数据同步或计划备份。使用「作业管理」编辑、运行和安排作业。",
        ),
    ] {
        style::subtle_card(egui::Margin::same(12)).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new(title).strong().color(style::ink()));
            ui.add_space(3.0);
            ui.label(RichText::new(instructions).color(style::muted()));
        });
        ui.add_space(7.0);
    }
}

fn shortcuts(ui: &mut Ui, preferences: &Preferences) {
    let command = if cfg!(target_os = "macos") {
        "⌘"
    } else {
        "Ctrl"
    };
    let option = if cfg!(target_os = "macos") {
        "⌥"
    } else {
        "Alt"
    };
    let (execute, refresh) = preferences.query_keys();
    let (key, refresh_key) = (execute.name(), refresh.name());

    shortcut_group(
        ui,
        "连接和页签",
        [
            ("新建连接", "Ctrl+M".into()),
            ("使用当前设置新建连接", "Ctrl+N".into()),
            ("断开当前连接", "Ctrl+F4 / Ctrl+W".into()),
            ("切换上一个或下一个连接", "Ctrl+Shift+Tab / Ctrl+Tab".into()),
            ("切换到第 1 至 8 个连接", "Ctrl+Shift+1…8".into()),
            ("切换到最后一个连接", "Ctrl+Shift+9".into()),
            ("新建查询页", format!("{command}+T")),
            (
                "切换上一个或下一个页签",
                "Ctrl+Page Up / Ctrl+Page Down".into(),
            ),
            ("重命名查询页签", format!("{option}+F2")),
            ("关闭当前页签", format!("{option}+L")),
        ],
    );
    shortcut_group(
        ui,
        "SQL 编辑器",
        [
            ("焦点移到 SQL 编辑器", "Ctrl+E".into()),
            ("打开到新查询页", format!("{command}+O")),
            ("打开到当前查询页", format!("{command}+{option}+O")),
            ("保存 SQL", format!("{command}+S")),
            ("SQL 另存为", format!("{command}+Shift+S")),
            ("查找", format!("{command}+F / F3")),
            ("替换", "Ctrl+H".into()),
            ("转到指定行", format!("{command}+G")),
            ("匹配的补全项", "Ctrl+Space".into()),
            ("函数参数提示", "Ctrl+Shift+Space".into()),
            ("格式化当前语句", "F12".into()),
            ("格式化选区", "Ctrl+F12".into()),
            ("格式化整页 SQL", "Shift+F12".into()),
        ],
    );
    shortcut_group(
        ui,
        "执行与查看结果",
        [
            ("执行当前语句或选区", format!("{key} / {command}+Enter")),
            ("执行选区", format!("Ctrl+{key}")),
            ("执行选区或整页", format!("Shift+{key}")),
            ("刷新对象浏览器", refresh_key.into()),
            ("执行当前语句并编辑结果，有选区时执行选区", "F8".into()),
            ("打开所选表的数据页", "F11".into()),
            ("导出当前结果或表数据", "Ctrl+Shift+E".into()),
            ("在文本和上次的表格或表单之间切换", format!("{command}+L")),
            ("在表格和表单之间切换", format!("{command}+{option}+L")),
            ("查找文本结果", format!("{command}+F / F3")),
            ("查找上一处文本匹配", "Shift+F3".into()),
            ("显示查询历史", format!("{command}+Shift+H")),
            ("焦点移到对象树", "Ctrl+B".into()),
            ("焦点移到当前输出页", "Ctrl+R".into()),
            ("显示或隐藏对象浏览器", "Ctrl+1".into()),
            ("显示或隐藏输出面板", "Ctrl+2".into()),
            ("显示或隐藏 SQL 编辑器", "Ctrl+3".into()),
        ],
    );
    shortcut_group(
        ui,
        "工具",
        [
            ("新建架构设计", format!("{command}+{option}+D")),
            ("将当前 SQL 加入收藏夹", format!("{command}+Shift+F")),
            ("表诊断", "Ctrl+Alt+T".into()),
            ("刷新服务器资源", "Ctrl+Alt+F".into()),
            ("备份为 SQL 转储文件", "Ctrl+Alt+E".into()),
            ("从 SQL 转储文件还原", "Ctrl+Shift+Q".into()),
            ("计划备份", "Ctrl+Alt+S".into()),
            ("导入文本文件", "Ctrl+Shift+M".into()),
            ("导出数据库结构说明", "Ctrl+Alt+Shift+S".into()),
            ("导出所选整表", "Ctrl+Alt+C".into()),
            ("使用帮助", "F1".into()),
        ],
    );
}

fn shortcut_group(
    ui: &mut Ui,
    group: &str,
    rows: impl IntoIterator<Item = (&'static str, String)>,
) {
    style::card(egui::Margin::same(12)).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        ui.label(RichText::new(group).strong().color(style::accent()));
        ui.add_space(6.0);
        Grid::new(group)
            .striped(true)
            .min_col_width(200.0)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                for (description, shortcut) in rows {
                    ui.label(description);
                    style::keycap(ui, &shortcut);
                    ui.end_row();
                }
            });
    });
    ui.add_space(9.0);
}

fn about(ui: &mut Ui) {
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "Windows x64".to_owned(),
        ("macos", "aarch64") => "macOS · Apple 芯片".to_owned(),
        (os, arch) => format!("{os} · {arch}"),
    };
    style::subtle_card(egui::Margin::same(14)).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        ui.horizontal(|ui| {
            style::badge(
                ui,
                format!("v{}", env!("CARGO_PKG_VERSION")),
                style::accent(),
                style::accent_soft(),
            );
            style::badge(ui, platform, style::muted(), style::chrome());
        });
        ui.add_space(6.0);
        ui.label(RichText::new("用于管理 MySQL 和 MariaDB 的原生桌面客户端。").color(style::ink()));
        if let Some(path) = state::profile_path()
            && let Some(dir) = path.parent()
        {
            ui.add_space(8.0);
            ui.label(RichText::new("数据目录").small().strong().color(style::muted()));
            ui.add(
                egui::Label::new(
                    RichText::new(dir.display().to_string())
                        .small()
                        .monospace()
                        .color(style::ink()),
                )
                .selectable(true),
            );
        }
    });
}
