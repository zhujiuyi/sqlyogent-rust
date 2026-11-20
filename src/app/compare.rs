#[path = "compare_sync.rs"]
mod sync;

#[cfg(test)]
#[path = "compare/schema_defaults_test.rs"]
mod schema_defaults_test;

use std::collections::BTreeSet;
use std::io::Write;

use eframe::egui::{self, RichText, TextEdit};
use rfd::FileDialog;

use crate::database::{
    Command, ObjectKind, SchemaApply, SchemaApplyResult, SchemaApplyStop, SchemaSnapshot,
    SnapshotObject,
};

use self::sync::Difference;
use super::{kind_label, state::Workspace, style};

pub(super) use sync::{LayoutTableMatch, LayoutTableState};

pub(super) fn compare_layout_tables(
    source: &SchemaSnapshot,
    target: &SchemaSnapshot,
) -> Vec<LayoutTableMatch> {
    sync::compare_layout_tables(source, target)
}

pub(super) fn plan_layout_creations(
    source: &SchemaSnapshot,
    target: &SchemaSnapshot,
    selected: &BTreeSet<String>,
) -> Result<(String, SchemaApply), Vec<String>> {
    let plan = sync::plan_layout_creations(source, target, selected)?;
    Ok((
        plan.sql,
        SchemaApply {
            expected: plan.target,
            changes: plan.changes,
        },
    ))
}

#[derive(Clone, PartialEq, Eq)]
struct Side {
    workspace: u64,
    schema: String,
}

impl Side {
    fn index(&self, workspaces: &[Workspace]) -> Option<usize> {
        workspaces
            .iter()
            .position(|workspace| workspace.key == self.workspace)
    }
}

enum Next {
    Editor(String),
    Apply,
}

enum Application {
    Running { stopping: bool },
    Finished(Result<SchemaApplyResult, String>),
}

pub(super) struct Compare {
    source: Side,
    target: Side,
    wait_for_source: bool,
    requested: bool,
    only_differences: bool,
    tables_only: bool,
    ignore_definer: bool,
    include_changed: bool,
    include_source_only: bool,
    include_target_only: bool,
    sync_database_defaults: bool,
    pattern: String,
    focused: Option<usize>,
    items: Option<Vec<sync::Item>>,
    selected: BTreeSet<usize>,
    preview: Option<Result<sync::Plan, Vec<String>>>,
    reviewed: bool,
    exported: Option<Result<String, String>>,
    application: Option<Application>,
}

impl Compare {
    pub fn new(index: usize, workspaces: &[Workspace]) -> Self {
        let source = default_side(index, workspaces);
        let peer = workspaces
            .iter()
            .enumerate()
            .find(|(i, workspace)| *i != index && workspace.server.is_some())
            .map_or(index, |(i, _)| i);
        let mut target = default_side(peer, workspaces);
        if target == source
            && let Some(other) = workspaces[peer]
                .schemas
                .iter()
                .find(|name| **name != source.schema)
        {
            target.schema = other.clone();
        }
        Self {
            source,
            target,
            wait_for_source: false,
            requested: false,
            only_differences: true,
            tables_only: false,
            ignore_definer: false,
            include_changed: true,
            include_source_only: true,
            include_target_only: true,
            sync_database_defaults: false,
            pattern: String::new(),
            focused: None,
            items: None,
            selected: BTreeSet::new(),
            preview: None,
            reviewed: false,
            exported: None,
            application: None,
        }
    }

    fn reset_preview(&mut self) {
        self.preview = None;
        self.reviewed = false;
        self.exported = None;
    }

    fn reset_results(&mut self) {
        self.application = None;
        self.items = None;
        self.focused = None;
        self.selected.clear();
        self.sync_database_defaults = false;
        self.reset_preview();
    }
}

fn default_side(index: usize, workspaces: &[Workspace]) -> Side {
    let workspace = &workspaces[index];
    Side {
        workspace: workspace.key,
        schema: workspace
            .server
            .as_ref()
            .and_then(|server| server.database.clone())
            .or_else(|| workspace.schemas.first().cloned())
            .unwrap_or_default(),
    }
}

fn send(side: &Side, workspaces: &mut [Workspace]) -> bool {
    let Some(index) = side.index(workspaces) else {
        return false;
    };
    let workspace = &mut workspaces[index];
    workspace.snapshots.remove(&side.schema);
    workspace.send(
        Command::SnapshotSchema {
            schema: side.schema.clone(),
        },
        "正在读取数据库对象定义…",
    )
}

pub(super) fn dialog(
    ctx: &egui::Context,
    state: &mut Option<Compare>,
    workspaces: &mut [Workspace],
) -> Option<usize> {
    let compare = state.as_mut()?;
    if compare.wait_for_source {
        if let Some(index) = compare.source.index(workspaces) {
            let source = &workspaces[index];
            if !source.busy() {
                compare.wait_for_source = false;
                if source.snapshots.contains_key(&compare.source.schema) {
                    send(&compare.target, workspaces);
                }
            }
        } else {
            compare.wait_for_source = false;
        }
    }
    if matches!(compare.application, Some(Application::Running { .. })) {
        match compare.target.index(workspaces) {
            Some(index) if !workspaces[index].busy() => {
                let result = workspaces[index]
                    .schema_apply_result
                    .take()
                    .unwrap_or_else(|| Err("未收到结构同步结果。请重新比较并检查目标库。".into()));
                compare.application = Some(Application::Finished(result));
            }
            None => {
                compare.application = Some(Application::Finished(Err(
                    "目标连接已关闭，无法确认执行结果。请重新比较并检查目标库。".into(),
                )));
            }
            _ => {}
        }
    }
    let applying = matches!(compare.application, Some(Application::Running { .. }));
    let reading = compare.requested
        && compare.application.is_none()
        && (compare.wait_for_source
            || [&compare.source, &compare.target].iter().any(|side| {
                side.index(workspaces)
                    .is_some_and(|index| workspaces[index].busy())
            }));
    let mut start = false;
    let mut close = false;
    let mut stage = None;
    let screen = ctx.content_rect();
    crate::app::style::modal(ctx, egui::Id::new("schema-comparison")).show(ctx, |ui| {
        ui.set_width((screen.width() - 70.0).clamp(540.0, 1_060.0));
        style::page_title(ui, "结构同步");
        ui.add_space(6.0);
        let before = (compare.source.clone(), compare.target.clone());
        ui.add_enabled_ui(!reading && !applying, |ui| {
            style::split_two_columns(
                ui,
                0.5,
                12.0,
                |ui| {
                    style::section_card(ui, "源端（基准结构）", |ui| {
                        side_fields(ui, ("Source", "源端"), &mut compare.source, workspaces);
                    });
                },
                |ui| {
                    style::section_card(ui, "目标端（待同步）", |ui| {
                        side_fields(ui, ("Target", "目标端"), &mut compare.target, workspaces);
                    });
                },
            );
        });
        ui.add_space(8.0);
        if before != (compare.source.clone(), compare.target.clone()) {
            compare.requested = false;
            compare.reset_results();
        }
        let ready = compare.source != compare.target
            && [&compare.source, &compare.target].iter().all(|side| {
                !side.schema.is_empty()
                    && side.index(workspaces).is_some_and(|index| {
                        workspaces[index].server.is_some() && !workspaces[index].busy()
                    })
            });
        ui.horizontal(|ui| {
            let can_compare = ready && !reading && !applying;
            start = ui
                .add_enabled(can_compare, style::primary_enabled(can_compare, "比较"))
                .clicked();
            if reading {
                ui.spinner();
                ui.label("正在读取定义…");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = ui
                    .add_enabled(!applying, egui::Button::new("关闭"))
                    .clicked();
            });
        });
        let source = compare
            .source
            .index(workspaces)
            .map(|index| &workspaces[index]);
        let target = compare
            .target
            .index(workspaces)
            .map(|index| &workspaces[index]);
        if let Some(application) = &mut compare.application {
            application_view(ui, application, target, screen.height());
        } else if compare.requested && !reading && !start {
            let (Some(source), Some(target)) = (source, target) else {
                ui.colored_label(style::error(), "所选连接已关闭，请重新选择。");
                return;
            };
            match (
                source.snapshots.get(&compare.source.schema),
                target.snapshots.get(&compare.target.schema),
            ) {
                (Some(left), Some(right)) if ready => {
                    stage = differences(ui, compare, left, right, &target.label, screen.height());
                }
                (None, _) => {
                    ui.colored_label(style::error(), &source.status);
                }
                _ => {
                    ui.colored_label(style::error(), &target.status);
                }
            }
        }
    });
    if close {
        *state = None;
        return None;
    }
    match stage {
        Some(Next::Editor(sql)) => {
            let index = compare.target.index(workspaces)?;
            workspaces[index].new_tab(sql, None);
            *state = None;
            return Some(index);
        }
        Some(Next::Apply) => {
            let index = compare.target.index(workspaces)?;
            let workspace = &mut workspaces[index];
            let plan = compare.preview.as_ref()?.as_ref().ok()?;
            let started = workspace.send(
                Command::ApplySchema {
                    apply: Box::new(SchemaApply {
                        expected: plan.target.clone(),
                        changes: plan.changes.clone(),
                    }),
                },
                "正在执行结构同步…",
            );
            compare.application = Some(if started {
                Application::Running { stopping: false }
            } else {
                Application::Finished(Err(workspace.status.clone()))
            });
        }
        None => {}
    }
    if start {
        let index = compare.target.index(workspaces)?;
        workspaces[index].snapshots.remove(&compare.target.schema);
        compare.requested = send(&compare.source, workspaces);
        compare.reset_results();
        if compare.requested {
            if compare.source.workspace == compare.target.workspace {
                compare.wait_for_source = true;
            } else {
                send(&compare.target, workspaces);
            }
        }
    }
    None
}

fn side_fields(
    ui: &mut egui::Ui,
    (id, label): (&str, &str),
    side: &mut Side,
    workspaces: &[Workspace],
) {
    let workspace = side.index(workspaces).map(|index| &workspaces[index]);
    ui.horizontal(|ui| {
        ui.label(label);
        egui::ComboBox::from_id_salt((id, "connection"))
            .selected_text(workspace.map_or("连接已关闭", |workspace| workspace.label.as_str()))
            .show_ui(ui, |ui| {
                for (index, workspace) in workspaces.iter().enumerate() {
                    if workspace.server.is_some()
                        && ui
                            .selectable_label(side.workspace == workspace.key, &workspace.label)
                            .clicked()
                    {
                        *side = default_side(index, workspaces);
                    }
                }
            });
        egui::ComboBox::from_id_salt((id, "schema"))
            .selected_text(if side.schema.is_empty() {
                "请选择数据库"
            } else {
                &side.schema
            })
            .show_ui(ui, |ui| {
                if let Some(index) = side.index(workspaces) {
                    for schema in &workspaces[index].schemas {
                        ui.selectable_value(&mut side.schema, schema.clone(), schema);
                    }
                }
            });
    });
}

fn differences(
    ui: &mut egui::Ui,
    compare: &mut Compare,
    left: &SchemaSnapshot,
    right: &SchemaSnapshot,
    target_label: &str,
    height: f32,
) -> Option<Next> {
    if left.omitted_objects != 0 || right.omitted_objects != 0 {
        ui.colored_label(
            style::error(),
            format!(
                "对象清单不完整 · 源端省略 {} 个对象，目标端省略 {} 个对象",
                left.omitted_objects, right.omitted_objects
            ),
        );
    }
    if compare.preview.is_some() {
        return preview(ui, compare, height, target_label, &right.schema);
    }

    ui.separator();
    let ignore_changed = ui
        .horizontal_wrapped(|ui| {
            ui.checkbox(&mut compare.only_differences, "仅显示差异");
            ui.checkbox(&mut compare.tables_only, "仅比较表");
            ui.checkbox(&mut compare.include_changed, "显示定义不同");
            ui.checkbox(&mut compare.include_source_only, "显示源端独有");
            ui.checkbox(&mut compare.include_target_only, "显示目标端独有");
            ui.checkbox(&mut compare.ignore_definer, "忽略 DEFINER")
                .changed()
        })
        .inner;
    ui.add(
        TextEdit::singleline(&mut compare.pattern)
            .hint_text("对象名筛选（% 和 _）")
            .desired_width(210.0),
    );
    if ignore_changed {
        compare.items = None;
    }
    let items = compare
        .items
        .get_or_insert_with(|| sync::differences(left, right, compare.ignore_definer));
    let compare_defaults = !compare.tables_only && compare.include_changed;
    let defaults = if compare_defaults {
        sync::database_defaults_diff(left, right)
    } else {
        None
    };
    if let Some((source, target)) = defaults {
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(
                &mut compare.sync_database_defaults,
                "同步数据库默认字符集和排序规则",
            );
            ui.label(format!("源端 {} / {}", source.charset, source.collation));
            ui.label(format!("目标端 {} / {}", target.charset, target.collation));
        });
    } else if compare_defaults && (left.defaults.is_none() || right.defaults.is_none()) {
        ui.colored_label(style::muted(), "数据库默认字符集和排序规则读取不完整。");
    }
    let pattern = NamePattern::new(&compare.pattern);
    let visible = items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            (!compare.only_differences || item.difference != Difference::Same)
                && (compare.include_changed || item.difference != Difference::Changed)
                && (compare.include_source_only || item.difference != Difference::SourceOnly)
                && (compare.include_target_only || item.difference != Difference::TargetOnly)
                && (!compare.tables_only || item.kind == ObjectKind::Table)
                && pattern.matches(&item.name)
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if visible.is_empty() {
        ui.label("当前筛选条件下没有对象。");
        if defaults.is_none() {
            return None;
        }
    }

    let selectable = visible
        .iter()
        .copied()
        .filter(|&index| items[index].selectable())
        .collect::<Vec<_>>();
    ui.horizontal_wrapped(|ui| {
        if ui.button("全选差异").clicked() {
            compare.selected.extend(
                selectable
                    .iter()
                    .filter(|&&index| items[index].difference != Difference::Same),
            );
            if defaults.is_some() {
                compare.sync_database_defaults = true;
            }
        }
        if ui.button("取消勾选").clicked() {
            compare.selected.clear();
            compare.sync_database_defaults = false;
        }
        if ui.button("反选差异").clicked() {
            for &index in &selectable {
                if items[index].difference != Difference::Same && !compare.selected.remove(&index) {
                    compare.selected.insert(index);
                }
            }
            if defaults.is_some() {
                compare.sync_database_defaults = !compare.sync_database_defaults;
            }
        }
    });

    if compare
        .focused
        .is_none_or(|index| !visible.contains(&index))
    {
        compare.focused = visible.first().copied();
    }
    egui::ScrollArea::vertical()
        .id_salt("sync-objects")
        .max_height((height * 0.23).clamp(84.0, 205.0))
        .show_rows(ui, 25.0, visible.len(), |ui, range| {
            for row in range {
                let index = visible[row];
                let item = &items[index];
                let mut checked = compare.selected.contains(&index);
                ui.horizontal(|ui| {
                    let checkbox = ui.add_enabled(
                        item.selectable(),
                        egui::Checkbox::without_text(&mut checked),
                    );
                    if checkbox.changed() {
                        if checked {
                            compare.selected.insert(index);
                        } else {
                            compare.selected.remove(&index);
                        }
                    }
                    if item.difference == Difference::Same && checkbox.enabled() {
                        checkbox.on_hover_text("同步依赖此表时可勾选");
                    } else if let Some(reason) = item.problem() {
                        checkbox.on_disabled_hover_text(reason);
                    }
                    let color = match item.difference {
                        Difference::Changed => style::accent(),
                        Difference::SourceOnly => style::success(),
                        Difference::Unavailable => style::error(),
                        _ => style::muted(),
                    };
                    let label = RichText::new(format!(
                        "{}  ·  {} / {}",
                        item.difference.label(),
                        kind_label(item.kind),
                        item.name
                    ))
                    .color(color);
                    if ui
                        .selectable_label(compare.focused == Some(index), label)
                        .clicked()
                    {
                        compare.focused = Some(index);
                    }
                });
            }
        });

    ui.separator();
    if let Some(index) = compare.focused {
        let item = &items[index];
        ui.columns(2, |columns| {
            code(
                &mut columns[0],
                "source-definition",
                &left.schema,
                item.source.map(|index| &left.objects[index]),
                height,
            );
            code(
                &mut columns[1],
                "target-definition",
                &right.schema,
                item.target.map(|index| &right.objects[index]),
                height,
            );
        });
        if let Some(reason) = item.problem() {
            ui.colored_label(style::error(), reason);
        }
    }

    let checked = selectable
        .into_iter()
        .filter(|index| compare.selected.contains(index))
        .collect::<BTreeSet<_>>();
    let sync_database_defaults = defaults.is_some() && compare.sync_database_defaults;
    let change_count = checked.len() + usize::from(sync_database_defaults);
    ui.separator();
    ui.horizontal(|ui| {
        ui.label(format!("已勾选 {change_count} 项"));
        if ui
            .add_enabled(change_count != 0, style::primary("预览同步 SQL"))
            .clicked()
        {
            compare.preview = Some(sync::plan(
                left,
                right,
                items,
                &checked,
                sync::PlanOptions {
                    ignore_definer: compare.ignore_definer,
                    sync_database_defaults,
                },
            ));
        }
    });
    None
}

fn preview(
    ui: &mut egui::Ui,
    compare: &mut Compare,
    height: f32,
    target_label: &str,
    schema: &str,
) -> Option<Next> {
    ui.separator();
    if ui.button("返回对象选择").clicked() {
        compare.reset_preview();
        return None;
    }
    let plan = match compare.preview.as_ref()? {
        Ok(plan) => plan,
        Err(issues) => {
            egui::ScrollArea::vertical()
                .id_salt("sync-issues")
                .max_height((height * 0.44).clamp(120.0, 410.0))
                .show(ui, |ui| {
                    for issue in issues {
                        ui.colored_label(style::error(), issue);
                    }
                });
            return None;
        }
    };
    ui.label(format!(
        "目标连接 {target_label}，数据库 {schema}，{} 条结构变更",
        plan.statements
    ));
    egui::ScrollArea::both()
        .id_salt("sync-script")
        .max_height((height * 0.39).clamp(110.0, 360.0))
        .show(ui, |ui| {
            let mut sql = plan.sql.as_str();
            ui.add(
                TextEdit::multiline(&mut sql)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY),
            );
        });
    if !plan.review.is_empty() {
        egui::CollapsingHeader::new(format!("需审阅的变更（{}）", plan.review.len()))
            .default_open(true)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("sync-review")
                    .max_height(84.0)
                    .show(ui, |ui| {
                        for change in &plan.review {
                            ui.colored_label(style::error(), change);
                        }
                    });
            });
    }
    ui.label("执行前会重新读取目标结构。停止或出错后，已执行的 DDL 会保留。");
    ui.checkbox(&mut compare.reviewed, "已核对目标库和待执行的结构变更");
    let enabled = compare.reviewed || plan.review.is_empty();
    let mut next = None;
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(enabled, egui::Button::new("保存 SQL 文件"))
            .clicked()
            && let Some(result) = export(&plan.sql)
        {
            compare.exported = Some(result);
        }
        if ui
            .add_enabled(enabled, egui::Button::new("在目标连接中打开 SQL"))
            .clicked()
        {
            next = Some(Next::Editor(plan.sql.clone()));
        }
        if ui
            .add_enabled(compare.reviewed, style::primary("执行所选变更"))
            .clicked()
        {
            next = Some(Next::Apply);
        }
    });
    if let Some(result) = &compare.exported {
        match result {
            Ok(path) => {
                ui.label(format!("已保存到 {path}"));
            }
            Err(error) => {
                ui.colored_label(style::error(), error);
            }
        }
    }
    next
}

fn application_view(
    ui: &mut egui::Ui,
    application: &mut Application,
    target: Option<&Workspace>,
    height: f32,
) {
    ui.separator();
    match application {
        Application::Running { stopping } => {
            ui.horizontal(|ui| {
                ui.spinner();
                let progress = target.and_then(|workspace| workspace.schema_apply_progress);
                if let Some(progress) = progress.filter(|progress| !progress.validating) {
                    ui.label(format!(
                        "已确认执行 {} / {} 条",
                        progress.completed, progress.total
                    ));
                } else {
                    ui.label("正在复核目标库结构…");
                }
                if ui
                    .add_enabled(!*stopping, egui::Button::new("停止"))
                    .clicked()
                    && target.is_some_and(Workspace::cancel_schema_apply)
                {
                    *stopping = true;
                }
            });
            if let Some(progress) = target
                .and_then(|workspace| workspace.schema_apply_progress)
                .filter(|progress| !progress.validating)
            {
                ui.add(egui::ProgressBar::new(
                    progress.completed as f32 / progress.total as f32,
                ));
            }
            if *stopping {
                ui.label("当前操作结束后停止执行后续语句。");
            }
        }
        Application::Finished(Err(error)) => {
            ui.colored_label(style::error(), error);
        }
        Application::Finished(Ok(result)) => {
            let label = match result.stopped {
                None => "结构同步完成",
                Some(SchemaApplyStop::Cancelled) => "已停止结构同步",
                Some(SchemaApplyStop::Failed { .. }) => "结构同步因错误停止",
            };
            ui.strong(format!(
                "{label}，已确认执行 {} / {} 条。",
                result.completed, result.total
            ));
            if let Some(SchemaApplyStop::Failed {
                sql,
                error,
                uncertain,
            }) = &result.stopped
            {
                ui.colored_label(
                    style::error(),
                    format!("第 {} 条：{error}", result.completed + 1),
                );
                if *uncertain {
                    ui.label("无法确认这一条是否已生效。请检查目标库。");
                }
                egui::ScrollArea::both()
                    .id_salt("failed-schema-sql")
                    .max_height((height * 0.30).clamp(84.0, 280.0))
                    .show(ui, |ui| {
                        let mut sql = sql.as_str();
                        ui.add(TextEdit::multiline(&mut sql).font(egui::TextStyle::Monospace));
                    });
            }
            if result.stopped.is_some() {
                ui.label("已执行的 DDL 会保留。继续前请重新比较。");
            }
        }
    }
}

fn export(sql: &str) -> Option<Result<String, String>> {
    let path = FileDialog::new()
        .set_title("保存结构同步脚本")
        .add_filter("SQL 文件", &["sql"])
        .set_file_name("structure-sync.sql")
        .save_file()?;
    let result = (|| -> std::io::Result<()> {
        let folder = path
            .parent()
            .ok_or_else(|| std::io::Error::other("无法确定输出目录"))?;
        let mut temp = tempfile::NamedTempFile::new_in(folder)?;
        temp.write_all(sql.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(&path)?;
        Ok(())
    })();
    Some(
        result
            .map(|()| path.display().to_string())
            .map_err(|error| format!("无法保存脚本：{error}")),
    )
}

fn code(ui: &mut egui::Ui, id: &str, schema: &str, object: Option<&SnapshotObject>, height: f32) {
    ui.strong(schema);
    let mut text = match object.map(|item| &item.definition) {
        Some(Ok(text) | Err(text)) => text.as_str(),
        None => "对象未列出",
    };
    egui::ScrollArea::both()
        .id_salt(id)
        .max_height((height * 0.23).clamp(85.0, 218.0))
        .show(ui, |ui| {
            ui.add(
                TextEdit::multiline(&mut text)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY),
            );
        });
}

enum MatchChar {
    Any,
    One,
    Literal(char),
}

struct NamePattern(Vec<MatchChar>);

impl NamePattern {
    fn new(pattern: &str) -> Self {
        let pattern = pattern.to_lowercase();
        let mut chars = pattern.chars();
        let mut tokens = Vec::new();
        while let Some(char) = chars.next() {
            tokens.push(match char {
                '%' => MatchChar::Any,
                '_' => MatchChar::One,
                '\\' => MatchChar::Literal(chars.next().unwrap_or('\\')),
                literal => MatchChar::Literal(literal),
            });
        }
        Self(tokens)
    }

    fn matches(&self, name: &str) -> bool {
        if self.0.is_empty() {
            return true;
        }
        let name = name.to_lowercase().chars().collect::<Vec<_>>();
        let mut possible = vec![false; name.len() + 1];
        possible[0] = true;
        for token in &self.0 {
            match token {
                MatchChar::Any => {
                    for index in 1..possible.len() {
                        possible[index] |= possible[index - 1];
                    }
                }
                MatchChar::One | MatchChar::Literal(_) => {
                    for (index, &char) in name.iter().enumerate().rev() {
                        possible[index + 1] = possible[index]
                            && match token {
                                MatchChar::Literal(literal) => *literal == char,
                                _ => true,
                            };
                    }
                    possible[0] = false;
                }
            }
        }
        possible[name.len()]
    }
}
