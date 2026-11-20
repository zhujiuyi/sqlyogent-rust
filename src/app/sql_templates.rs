use eframe::egui::{self, Align, ComboBox, Layout, ScrollArea, TextEdit, TextStyle};

use super::style;

pub(super) enum Choice {
    Insert(&'static str),
    Cancel,
}

pub(super) struct Dialog {
    selected: usize,
}

impl Dialog {
    pub(super) fn new() -> Self {
        Self { selected: 0 }
    }

    pub(super) fn show(&mut self, ctx: &egui::Context) -> Option<Choice> {
        let screen = ctx.content_rect();
        let modal = crate::app::style::modal(ctx, egui::Id::new("sql-templates")).show(ctx, |ui| {
            ui.set_width((screen.width() - 48.0).clamp(250.0, 560.0));
            ui.heading("插入 SQL 模板");
            ComboBox::from_id_salt("template")
                .selected_text(TEMPLATES[self.selected].label)
                .width(ui.available_width())
                .height((screen.height() * 0.48).clamp(120.0, 340.0))
                .show_ui(ui, |ui| {
                    for (index, template) in TEMPLATES.iter().enumerate() {
                        ui.selectable_value(&mut self.selected, index, template.label);
                    }
                });

            let sql = TEMPLATES[self.selected].sql;
            let mut preview = sql;
            ui.label("SQL 预览");
            ScrollArea::vertical()
                .max_height((screen.height() * 0.35).clamp(95.0, 240.0))
                .show(ui, |ui| {
                    ui.add(
                        TextEdit::multiline(&mut preview)
                            .font(TextStyle::Monospace)
                            .desired_width(f32::INFINITY)
                            .desired_rows(8),
                    );
                });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(style::primary("插入")).clicked() {
                    Some(Choice::Insert(sql))
                } else if ui.button("取消").clicked() {
                    Some(Choice::Cancel)
                } else {
                    None
                }
            })
            .inner
        });
        if modal.should_close() {
            return Some(Choice::Cancel);
        }
        modal.inner
    }
}

struct Template {
    label: &'static str,
    sql: &'static str,
}

const TEMPLATES: &[Template] = &[
    Template {
        label: "SELECT 查询",
        sql: "SELECT `id`, `name`\nFROM `example_table`\nWHERE `id` = 1;",
    },
    Template {
        label: "JOIN 联表查询",
        sql: "SELECT t.`id`, d.`description`\n\
              FROM `example_table` AS t\n\
              INNER JOIN `example_details` AS d ON d.`table_id` = t.`id`;",
    },
    Template {
        label: "INSERT 插入",
        sql: "INSERT INTO `example_table` (`id`, `name`)\nVALUES (1, '示例');",
    },
    Template {
        label: "INSERT … SELECT 插入查询结果",
        sql: "INSERT INTO `example_archive` (`id`, `name`)\n\
              SELECT `id`, `name` FROM `example_table`\nWHERE `id` = 1;",
    },
    Template {
        label: "UPDATE 更新",
        sql: "UPDATE `example_table`\nSET `name` = '新名称'\nWHERE `id` = 1;",
    },
    Template {
        label: "DELETE 删除记录",
        sql: "DELETE FROM `example_table`\nWHERE `id` = 1;",
    },
    Template {
        label: "TRUNCATE 清空表",
        sql: "TRUNCATE TABLE `example_table`;",
    },
    Template {
        label: "REPLACE 替换记录",
        sql: "REPLACE INTO `example_table` (`id`, `name`)\nVALUES (1, '新名称');",
    },
    Template {
        label: "LOAD DATA 导入服务器文件",
        sql: "LOAD DATA INFILE '/path/to/example.csv'\n\
              INTO TABLE `example_table`\n\
              CHARACTER SET utf8mb4\n\
              FIELDS TERMINATED BY ',' OPTIONALLY ENCLOSED BY '\"'\n\
              LINES TERMINATED BY '\\n'\n\
              IGNORE 1 LINES\n(`id`, `name`);",
    },
    Template {
        label: "CREATE TABLE 新建表",
        sql: "CREATE TABLE `example_table` (\n    `id` INT PRIMARY KEY,\n    `name` VARCHAR(100) NOT NULL\n);",
    },
    Template {
        label: "CREATE DATABASE 新建数据库",
        sql: "CREATE DATABASE `example_db` CHARACTER SET utf8mb4;",
    },
    Template {
        label: "CREATE INDEX 新建索引",
        sql: "CREATE INDEX `idx_example_name` ON `example_table` (`name`);",
    },
    Template {
        label: "DROP DATABASE 删除数据库",
        sql: "DROP DATABASE `example_db`;",
    },
    Template {
        label: "DROP TABLE 删除表",
        sql: "DROP TABLE `example_table`;",
    },
    Template {
        label: "ALTER TABLE 修改表",
        sql: "ALTER TABLE `example_table`\nADD COLUMN `description` VARCHAR(255);",
    },
    Template {
        label: "ALTER DATABASE 修改数据库",
        sql: "ALTER DATABASE `example_db` CHARACTER SET utf8mb4;",
    },
    Template {
        label: "RENAME TABLE 重命名表",
        sql: "RENAME TABLE `example_table` TO `example_renamed`;",
    },
    Template {
        label: "USE 切换数据库",
        sql: "USE `example_db`;",
    },
    Template {
        label: "DESCRIBE 查看表结构",
        sql: "DESCRIBE `example_table`;",
    },
    Template {
        label: "BEGIN / COMMIT / ROLLBACK 开始和结束事务",
        sql: "BEGIN;\n\n-- 在此编写事务内要执行的语句。\n\nCOMMIT;\n-- 放弃变更时，将 COMMIT 改为 ROLLBACK。",
    },
    Template {
        label: "LOCK / UNLOCK 锁定和解锁表",
        sql: "LOCK TABLES `example_table` READ;\n\n\
              SELECT `id`, `name` FROM `example_table`;\n\n\
              UNLOCK TABLES;",
    },
    Template {
        label: "SET TRANSACTION 设置事务隔离级别",
        sql: "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED;",
    },
    Template {
        label: "备份 / 恢复表数据",
        sql: "-- 仅适用于支持这两个命令的旧版 MySQL。\n\
              BACKUP TABLE `example_table` TO '/path/to/backup';\n\
              RESTORE TABLE `example_table` FROM '/path/to/backup';",
    },
    Template {
        label: "CREATE PROCEDURE 新建存储过程",
        sql: "CREATE PROCEDURE `example_procedure` (IN p_id INT)\n\
              SELECT `name` FROM `example_table` WHERE `id` = p_id;",
    },
    Template {
        label: "CREATE FUNCTION 新建存储函数",
        sql: "CREATE FUNCTION `example_function` (p_value INT)\n\
              RETURNS INT DETERMINISTIC NO SQL\nRETURN p_value * 2;",
    },
    Template {
        label: "ALTER PROCEDURE 修改存储过程",
        sql: "ALTER PROCEDURE `example_procedure` COMMENT '按编号查名称';",
    },
    Template {
        label: "ALTER FUNCTION 修改存储函数",
        sql: "ALTER FUNCTION `example_function` COMMENT '返回输入的两倍';",
    },
    Template {
        label: "DROP PROCEDURE 删除存储过程",
        sql: "DROP PROCEDURE `example_procedure`;",
    },
    Template {
        label: "DROP FUNCTION 删除存储函数",
        sql: "DROP FUNCTION `example_function`;",
    },
    Template {
        label: "CREATE VIEW 新建视图",
        sql: "CREATE VIEW `example_view` AS\nSELECT `id`, `name` FROM `example_table`;",
    },
    Template {
        label: "ALTER VIEW 修改视图",
        sql: "ALTER VIEW `example_view` AS\nSELECT `id` FROM `example_table`;",
    },
    Template {
        label: "DROP VIEW 删除视图",
        sql: "DROP VIEW `example_view`;",
    },
    Template {
        label: "CREATE TRIGGER 新建触发器",
        sql: "CREATE TRIGGER `example_trigger`\nBEFORE INSERT ON `example_table`\n\
              FOR EACH ROW SET NEW.`name` = TRIM(NEW.`name`);",
    },
    Template {
        label: "DROP TRIGGER 删除触发器",
        sql: "DROP TRIGGER `example_trigger`;",
    },
    Template {
        label: "CREATE EVENT 新建事件",
        sql: "CREATE EVENT `example_event`\n\
              ON SCHEDULE AT CURRENT_TIMESTAMP + INTERVAL 1 DAY\n\
              DO UPDATE `example_table` SET `name` = '已处理' WHERE `id` = 1;",
    },
    Template {
        label: "ALTER EVENT 修改事件",
        sql: "ALTER EVENT `example_event`\nON SCHEDULE EVERY 7 DAY;",
    },
    Template {
        label: "DROP EVENT 删除事件",
        sql: "DROP EVENT `example_event`;",
    },
];
