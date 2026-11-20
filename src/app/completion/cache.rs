use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use eframe::egui;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app::state::{Profile, Workspace, write_atomic};
use crate::database::{DbObject, ObjectKind};
use crate::sql_tools::{EditorTokenKind, editor_tokens};

use super::Details;

const VERSION: u32 = 1;
const MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Tags {
    pub objects: BTreeMap<String, Vec<DbObject>>,
    pub details: HashMap<DbObject, Details>,
}

impl Tags {
    pub(super) fn reconcile_schema(&mut self, schema: &str, objects: &HashSet<&DbObject>) {
        self.objects.remove(schema);
        self.details
            .retain(|object, _| object.schema != schema || objects.contains(object));
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct Snapshot {
    version: u32,
    connection: String,
    schemas: BTreeMap<String, Vec<Entry>>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    name: String,
    #[serde(with = "Kind")]
    kind: ObjectKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<Details>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "ObjectKind", rename_all = "snake_case")]
enum Kind {
    Table,
    View,
    Procedure,
    Function,
    Trigger,
    Event,
}

impl Snapshot {
    pub(super) fn capture(workspace: &Workspace, connection: String) -> Self {
        let tags = &workspace.tags;
        let schemas = workspace
            .schemas
            .iter()
            .filter_map(|schema| {
                let objects = workspace
                    .objects
                    .get(schema)
                    .or_else(|| tags.cached.objects.get(schema))?;
                let entries = objects
                    .iter()
                    .map(|object| Entry {
                        name: object.name.clone(),
                        kind: object.kind,
                        detail: tags
                            .details
                            .get(object)
                            .or_else(|| tags.cached.details.get(object))
                            .filter(|detail| persistable(detail))
                            .cloned(),
                    })
                    .collect();
                Some((schema.clone(), entries))
            })
            .collect();
        Self {
            version: VERSION,
            connection,
            schemas,
        }
    }

    pub(super) fn into_tags(self, schemas: &[String]) -> Tags {
        let mut tags = Tags::default();
        for (schema, entries) in self.schemas {
            if !schemas.contains(&schema) {
                continue;
            }
            let mut objects = Vec::with_capacity(entries.len());
            for entry in entries {
                let object = DbObject {
                    schema: schema.clone(),
                    name: entry.name,
                    kind: entry.kind,
                };
                if let Some(detail) = entry.detail {
                    tags.details.insert(object.clone(), detail);
                }
                objects.push(object);
            }
            tags.objects.insert(schema, objects);
        }
        tags
    }
}

fn persistable(detail: &Details) -> bool {
    match detail {
        Details::Columns { .. } => true,
        Details::Parameters(Some(parameters)) => parameters.iter().all(|parameter| {
            // Keep declarations containing SQL values or comments in the live session.
            !editor_tokens(parameter, false, false).any(|token| {
                matches!(token.kind, EditorTokenKind::Protected { .. })
                    || token.text.eq_ignore_ascii_case("DEFAULT")
            })
        }),
        Details::Parameters(None) => false,
    }
}

pub(super) struct Store {
    pub directory: PathBuf,
    pub connection: String,
    pending: Option<Receiver<Result<Outcome, String>>>,
    loading: bool,
    pub last_error: Option<String>,
}

pub(super) enum Outcome {
    Read {
        generation: u64,
        snapshot: Option<Snapshot>,
    },
    Saved {
        announce: bool,
    },
}

impl Store {
    pub(super) fn new(directory: &Path, profile: &Profile) -> Self {
        Self {
            directory: directory.to_owned(),
            connection: connection_key(profile),
            pending: None,
            loading: false,
            last_error: None,
        }
    }

    pub(super) fn idle(&self) -> bool {
        self.pending.is_none()
    }

    pub(super) fn loading(&self) -> bool {
        self.loading
    }

    pub(super) fn load(&mut self, ctx: &egui::Context, generation: u64) -> Result<(), String> {
        let directory = self.directory.clone();
        let connection = self.connection.clone();
        self.start(ctx, move || {
            let snapshot = read(&directory, &connection).map_err(|error| {
                format!("无法读取补全标签目录 {}。{error}", directory.display())
            })?;
            Ok(Outcome::Read {
                generation,
                snapshot,
            })
        })?;
        self.loading = true;
        Ok(())
    }

    pub(super) fn save(
        &mut self,
        ctx: &egui::Context,
        snapshot: Snapshot,
        announce: bool,
    ) -> Result<(), String> {
        let directory = self.directory.clone();
        self.start(ctx, move || {
            write(&directory, snapshot).map_err(|error| {
                format!("无法写入补全标签目录 {}。{error}", directory.display())
            })?;
            Ok(Outcome::Saved { announce })
        })
    }

    pub(super) fn poll(&mut self) -> Option<Result<Outcome, String>> {
        let result = match self.pending.as_ref()?.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err("补全标签文件操作未返回结果。".into()),
        };
        self.pending = None;
        self.loading = false;
        Some(result)
    }

    fn start(
        &mut self,
        ctx: &egui::Context,
        task: impl FnOnce() -> Result<Outcome, String> + Send + 'static,
    ) -> Result<(), String> {
        let ctx = ctx.clone();
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("harbor-completion-cache".into())
            .spawn(move || {
                let _ = sender.send(task());
                ctx.request_repaint();
            })
            .map_err(|error| format!("无法启动补全标签文件操作。{error}"))?;
        self.pending = Some(receiver);
        Ok(())
    }
}

fn connection_key(profile: &Profile) -> String {
    let route = if let Some(http) = &profile.http {
        serde_json::json!([
            "http",
            http.url.trim(),
            http.web_user.trim(),
            http.proxy_url.trim(),
            http.proxy_user.trim()
        ])
    } else if profile.ssh {
        serde_json::json!([
            "ssh",
            profile.ssh_host.trim().to_ascii_lowercase(),
            profile.ssh_port,
            profile.ssh_user.trim(),
            profile.tls,
            profile.client_identity.trim()
        ])
    } else {
        serde_json::json!(["direct", profile.tls, profile.client_identity.trim()])
    };
    // The profile describes the route. Database, tunnel and proxy passwords come from
    // the connection form separately and never enter the key or the saved tags.
    let key = serde_json::json!([
        profile.label,
        profile.host.trim().to_ascii_lowercase(),
        profile.port,
        profile.user.trim(),
        profile.schema.trim(),
        profile.advanced.displayed_databases.trim(),
        route
    ]);
    format!("{:x}", Sha256::digest(key.to_string()))
}

fn file_path(directory: &Path, connection: &str) -> PathBuf {
    directory.join(format!("harbor-sql-tags-v{VERSION}-{connection}.json"))
}

fn check_directory(directory: &Path) -> io::Result<()> {
    if !directory.is_absolute() || !fs::metadata(directory)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "请选择一个存在的绝对目录。",
        ));
    }
    Ok(())
}

fn read(directory: &Path, connection: &str) -> io::Result<Option<Snapshot>> {
    check_directory(directory)?;
    let path = file_path(directory, connection);
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "补全标签文件类型或大小无效。",
        ));
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "补全标签文件超过 16 MiB。",
        ));
    }
    let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
    if snapshot.version != VERSION || snapshot.connection != connection {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "补全标签的版本或连接身份不符。",
        ));
    }
    Ok(Some(snapshot))
}

fn write(directory: &Path, snapshot: Snapshot) -> io::Result<()> {
    check_directory(directory)?;
    let bytes = serde_json::to_vec(&snapshot)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "补全标签文件超过 16 MiB。",
        ));
    }
    write_atomic(&file_path(directory, &snapshot.connection), &bytes)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::super::{Suggestions, poll_cache};
    use super::*;
    use crate::database::{ColumnInfo, ObjectDetail, Response, ServerInfo};

    #[test]
    fn tags_reopen_per_connection_and_refresh_from_live_catalog() {
        let executable = std::env::current_exe().unwrap();
        let directory = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let profile = Profile {
            user: "first".into(),
            ..Profile::default()
        };
        let other = Profile {
            user: "second".into(),
            ..profile.clone()
        };
        let mut workspace = Workspace::new("测试连接".into(), Some(profile.clone()));
        let table = DbObject {
            schema: "库存".into(),
            name: "商品".into(),
            kind: ObjectKind::Table,
        };
        workspace.schemas.push(table.schema.clone());
        workspace
            .objects
            .insert(table.schema.clone(), vec![table.clone()]);
        let connection = connection_key(&profile);
        let other_connection = connection_key(&other);
        let context = egui::Context::default();
        let settle_cache = |workspace: &mut Workspace| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                poll_cache(workspace, Some(directory.path()), &context);
                if workspace.tags.store.as_ref().unwrap().idle() {
                    break;
                }
                assert!(Instant::now() < deadline, "补全缓存操作未结束");
                thread::sleep(Duration::from_millis(1));
            }
        };
        for (column, objects_first) in [("旧列", false), ("新列", true)] {
            workspace.tags.invalidate(Some(&table.schema));
            workspace.tags.remember(&ObjectDetail {
                object: table.clone(),
                columns: vec![ColumnInfo {
                    name: column.into(),
                    data_type: "int".into(),
                    nullable: false,
                    key: String::new(),
                    default_value: None,
                    extra: String::new(),
                }],
                ddl: String::new(),
                ddl_error: None,
            });
            write(
                directory.path(),
                Snapshot::capture(&workspace, connection.clone()),
            )
            .unwrap();
            let mut reopened = Workspace::new("重启".into(), Some(profile.clone()));
            reopened.server = Some(ServerInfo {
                version: "test".into(),
                connection_id: 1,
                database: Some(table.schema.clone()),
                http_tunnel: false,
            });
            let origin = reopened.tab().id;
            reopened.receive(origin, Response::Schemas(workspace.schemas.clone()), false);
            let mut store = Store::new(directory.path(), &profile);
            store
                .load(&context, reopened.tags.cache_generation)
                .unwrap();
            reopened.tags.store = Some(store);
            let object_list = || Response::Objects {
                schema: table.schema.clone(),
                objects: vec![table.clone()],
            };
            if !objects_first {
                settle_cache(&mut reopened);
            }
            reopened.receive(origin, object_list(), false);
            settle_cache(&mut reopened);
            assert_eq!(
                Suggestions::new(&reopened).columns(Some(&table.schema), &table.name),
                [column]
            );
            assert!(reopened.tags.columns(&table).is_none());
            assert!(
                read(directory.path(), &connection)
                    .unwrap()
                    .unwrap()
                    .into_tags(&reopened.schemas)
                    .details
                    .contains_key(&table)
            );
            reopened.receive(origin, object_list(), true);
            settle_cache(&mut reopened);
            assert!(
                Suggestions::new(&reopened)
                    .columns(Some(&table.schema), &table.name)
                    .is_empty()
            );
        }
        assert!(read(directory.path(), &other_connection).unwrap().is_none());
    }
}
