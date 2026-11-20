use eframe::egui;

use crate::app::output::selection::Point;
use crate::database::{DbObject, JoinedEditTarget, JoinedEditablePage, TablePage};

#[derive(Clone, Eq, PartialEq)]
pub(super) struct Target {
    alias: String,
    pub(super) table: DbObject,
}

impl Target {
    pub(super) fn new(target: &JoinedEditTarget) -> Self {
        Self {
            alias: target.alias.clone(),
            table: target.page.object.clone(),
        }
    }

    fn matches(&self, target: &JoinedEditTarget) -> bool {
        self.alias == target.alias && self.table == target.page.object
    }

    fn label(&self) -> String {
        format!("{} · {}.{}", self.alias, self.table.schema, self.table.name)
    }
}

pub(super) struct Joined {
    pub(super) workspace: u64,
    pub(super) tab: u64,
    pub(super) response: Option<JoinedEditablePage>,
    pub(super) selected: Option<Target>,
}

impl Joined {
    pub(super) fn new(workspace: u64, tab: u64, response: JoinedEditablePage) -> Self {
        let selected = response.targets.first().map(Target::new);
        Self {
            workspace,
            tab,
            response: Some(response),
            selected,
        }
    }

    pub(super) fn page(&self) -> Option<&TablePage> {
        self.binding().map(|target| &target.page)
    }

    pub(super) fn binding(&self) -> Option<&JoinedEditTarget> {
        let target = self.selected.as_ref()?;
        self.response
            .as_ref()?
            .targets
            .iter()
            .find(|candidate| target.matches(candidate))
    }

    pub(super) fn result_grid_id(&self) -> egui::Id {
        egui::Id::new(("joined-result-grid", self.workspace, self.tab))
    }

    pub(super) fn grid_id(&self) -> egui::Id {
        let identity = self
            .selected
            .as_ref()
            .map(|target| (&target.alias, &target.table.schema, &target.table.name));
        egui::Id::new(("editable-joined-grid", self.workspace, self.tab, identity))
    }

    pub(super) fn select(&mut self, target: Option<Target>) -> bool {
        if let Some(target) = &target
            && !self.response.as_ref().is_some_and(|reply| {
                reply
                    .targets
                    .iter()
                    .any(|candidate| target.matches(candidate))
            })
        {
            return false;
        }
        if self.selected == target {
            return false;
        }
        self.selected = target;
        true
    }

    pub(super) fn replace(&mut self, response: JoinedEditablePage) -> bool {
        self.response = Some(response);
        if self.selected.is_some() && self.page().is_none() {
            self.selected = None;
            true
        } else {
            false
        }
    }

    pub(super) fn target_picker(&self, ui: &mut egui::Ui, enabled: bool) -> Option<Option<Target>> {
        const READ_ONLY: &str = "只读 · 完整结果";
        let mut choice = self.selected.clone();
        ui.horizontal(|ui| {
            ui.label("写入目标");
            ui.add_enabled_ui(enabled, |ui| {
                egui::ComboBox::from_id_salt(("joined-target", self.workspace, self.tab))
                    .selected_text(
                        choice
                            .as_ref()
                            .map_or_else(|| READ_ONLY.into(), Target::label),
                    )
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut choice, None, READ_ONLY);
                        if let Some(reply) = &self.response {
                            for target in &reply.targets {
                                let target = Target::new(target);
                                let label = target.label();
                                ui.selectable_value(&mut choice, Some(target), label);
                            }
                        }
                    });
            });
        });
        (choice != self.selected).then_some(choice)
    }
}

pub(super) fn source_row(target: &JoinedEditTarget, result_row: usize) -> Option<usize> {
    target
        .result_rows
        .get(result_row)
        .copied()
        .flatten()
        .filter(|&row| row < target.page.rows.len())
}

pub(super) fn source_point(target: &JoinedEditTarget, result: Point) -> Option<Point> {
    let row = source_row(target, result.row)?;
    let column = target
        .result_columns
        .get(result.column)
        .copied()
        .flatten()?;
    target
        .page
        .columns
        .get(column)?
        .writable
        .then_some(Point { row, column })
}

pub(super) fn result_point(
    target: &JoinedEditTarget,
    source: Point,
    preferred: Point,
) -> Option<Point> {
    let row = result_row(target, source.row, preferred.row)?;
    let column = target
        .result_columns
        .iter()
        .enumerate()
        .filter(|(_, mapped)| **mapped == Some(source.column))
        .min_by_key(|(column, _)| column.abs_diff(preferred.column))?
        .0;
    Some(Point { row, column })
}

pub(super) fn result_row(
    target: &JoinedEditTarget,
    source_row: usize,
    preferred: usize,
) -> Option<usize> {
    target
        .result_rows
        .iter()
        .enumerate()
        .filter(|(_, mapped)| **mapped == Some(source_row))
        .min_by_key(|(row, _)| row.abs_diff(preferred))
        .map(|(row, _)| row)
}
