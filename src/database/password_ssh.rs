use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use russh::Channel;
use russh::client::{self, ChannelOpenHandle, Msg, Session};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use tokio::sync::oneshot;

use super::{SSH_TIMEOUT, SharedLifetime, SshConfig, SshTunnel};

type Diagnostics = Arc<Mutex<VecDeque<String>>>;

struct Authentication {
    host: String,
    port: u16,
    user: String,
    credentials: Credentials,
    hosts: PathBuf,
}

enum Credentials {
    Password(String),
    PrivateKey {
        path: PathBuf,
        passphrase: Option<String>,
    },
}

pub(super) fn open(
    config: &SshConfig,
    host: String,
    target: &str,
    target_port: u16,
    socket: TcpListener,
    port: u16,
    lifetime: &SharedLifetime,
) -> Result<SshTunnel, String> {
    let hosts = config
        .known_hosts_path
        .clone()
        .or_else(|| {
            directories::BaseDirs::new()
                .map(|base| base.home_dir().join(".ssh").join("known_hosts"))
        })
        .ok_or("找不到用户目录，请选择 SSH 主机密钥文件。")?;
    if !hosts.is_file() {
        return Err(format!("找不到 SSH 主机密钥文件 {}。", hosts.display()));
    }
    socket
        .set_nonblocking(true)
        .map_err(|error| format!("无法监听 SSH 本地端口：{error}"))?;
    let credentials = if let Some(password) = &config.password {
        Credentials::Password(password.clone())
    } else {
        Credentials::PrivateKey {
            path: config.identity_file.clone().ok_or("未选择 SSH 私钥。")?,
            passphrase: config.key_passphrase.clone(),
        }
    };
    let user = if config.user.is_empty() {
        let variable = if cfg!(windows) { "USERNAME" } else { "USER" };
        std::env::var(variable)
            .ok()
            .filter(|value| {
                !value.is_empty()
                    && !value
                        .chars()
                        .any(|ch| ch.is_whitespace() || ch.is_control())
            })
            .ok_or("无法取得当前用户名，请填写 SSH 用户名。")?
    } else {
        config.user.clone()
    };
    let auth = Authentication {
        host,
        port: config.port,
        user,
        credentials,
        hosts,
    };
    let (signal_stop, stop) = oneshot::channel();
    {
        let mut state = lifetime.lock().unwrap();
        if state.stopped {
            return Err("数据库连接服务已停止。".into());
        }
        state.password_ssh = Some(signal_stop);
    }
    let diagnostics = Arc::new(Mutex::new(VecDeque::new()));
    let tunnel = SshTunnel {
        lifetime: Arc::clone(lifetime),
        port,
        diagnostics: Arc::clone(&diagnostics),
    };
    let (ready, opened) = mpsc::sync_channel(1);
    let receiver = (target.to_owned(), target_port);
    std::thread::Builder::new()
        .name("harbor-ssh-forward".into())
        .spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => {
                    runtime.block_on(forward(auth, receiver, socket, stop, ready, diagnostics))
                }
                Err(error) => {
                    let _ = ready.send(Err(format!("无法启动 SSH 转发服务：{error}")));
                }
            }
        })
        .map_err(|error| format!("无法启动 SSH 转发线程：{error}"))?;
    match opened.recv_timeout(SSH_TIMEOUT + Duration::from_secs(1)) {
        Ok(Ok(())) => Ok(tunnel),
        Ok(Err(error)) => Err(tunnel.error(error)),
        Err(_) => Err(tunnel.error("等待 SSH 隧道建立时超时或连接已结束。")),
    }
}

async fn forward(
    auth: Authentication,
    target: (String, u16),
    socket: TcpListener,
    mut stop: oneshot::Receiver<()>,
    ready: mpsc::SyncSender<Result<(), String>>,
    diagnostics: Diagnostics,
) {
    let listener = match tokio::net::TcpListener::from_std(socket) {
        Ok(listener) => listener,
        Err(error) => {
            let _ = ready.send(Err(format!("无法监听 SSH 本地端口：{error}")));
            return;
        }
    };
    let start = tokio::time::timeout(SSH_TIMEOUT, connect(auth, Arc::clone(&diagnostics)));
    let result = tokio::select! {
        _ = &mut stop => Err("已关闭 SSH 隧道。".into()),
        result = start => result.unwrap_or_else(|_| Err("SSH 登录或认证超时。".into())),
    };
    let session = match result {
        Ok(session) => Arc::new(session),
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        return;
    }
    let mut transfers = tokio::task::JoinSet::new();
    let mut probe = tokio::time::interval(Duration::from_secs(1));
    probe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = probe.tick() => if session.is_closed() { break; },
            _ = transfers.join_next(), if !transfers.is_empty() => {},
            incoming = listener.accept() => {
                let (mut socket, origin) = match incoming {
                    Ok(incoming) => incoming,
                    Err(error) => { record(&diagnostics, format!("本地转发端口关闭：{error}")); break; }
                };
                let session = Arc::clone(&session);
                let target = target.clone();
                let diagnostics = Arc::clone(&diagnostics);
                transfers.spawn(async move {
                    let result = tokio::time::timeout(SSH_TIMEOUT, session.channel_open_direct_tcpip(
                        target.0, u32::from(target.1), origin.ip().to_string(), u32::from(origin.port()),
                    )).await;
                    match result {
                        Ok(Ok(channel)) => {
                            let mut remote = channel.into_stream();
                            let _ = tokio::io::copy_bidirectional(&mut socket, &mut remote).await;
                        }
                        Ok(Err(error)) => record(&diagnostics, format!("SSH 服务器拒绝打开数据库转发：{error}")),
                        Err(_) => record(&diagnostics, "SSH 服务器打开数据库转发超时。".into()),
                    }
                });
            }
        }
    }
    transfers.abort_all();
}

async fn connect(
    auth: Authentication,
    diagnostics: Diagnostics,
) -> Result<client::Handle<Verifier>, String> {
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    });
    let handler = Verifier {
        host: auth.host.clone(),
        port: auth.port,
        hosts: auth.hosts,
        diagnostics,
    };
    let mut session = client::connect(config, (auth.host.as_str(), auth.port), handler)
        .await
        .map_err(|error| format!("SSH 连接失败：{error}"))?;
    match auth.credentials {
        Credentials::Password(password) => {
            let result = session
                .authenticate_password(auth.user, password)
                .await
                .map_err(|error| format!("SSH 密码认证失败：{error}"))?;
            if !result.success() {
                return Err("SSH 服务器未接受该用户名和密码。".into());
            }
        }
        Credentials::PrivateKey { path, passphrase } => {
            let key = private_key(&path, passphrase.as_deref())?;
            let hash = if key.algorithm().is_rsa() {
                session
                    .best_supported_rsa_hash()
                    .await
                    .map_err(|error| format!("无法协商 SSH 签名方式：{error}"))?
                    .flatten()
            } else {
                None
            };
            let result = session
                .authenticate_publickey(auth.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await
                .map_err(|error| format!("SSH 私钥认证失败：{error}"))?;
            if !result.success() {
                return Err("SSH 服务器未接受该用户名和私钥。".into());
            }
        }
    }
    Ok(session)
}

fn private_key(
    path: &std::path::Path,
    passphrase: Option<&str>,
) -> Result<russh::keys::PrivateKey, String> {
    const LIMIT: u64 = 1024 * 1024;
    let file =
        std::fs::File::open(path).map_err(|error| format!("无法打开 SSH 私钥文件。{error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("无法读取 SSH 私钥文件信息。{error}"))?;
    if !metadata.is_file() || metadata.len() > LIMIT {
        return Err("请选择不超过 1 MiB 的普通 SSH 私钥文件。".into());
    }
    let mut text = String::new();
    file.take(LIMIT + 1)
        .read_to_string(&mut text)
        .map_err(|_| "无法读取 SSH 私钥。请核对文件格式。")?;
    if text.len() as u64 > LIMIT {
        return Err("SSH 私钥文件超过 1 MiB。".into());
    }
    russh::keys::decode_secret_key(&text, passphrase)
        .map_err(|_| "无法解开 SSH 私钥。请核对私钥格式和口令。".into())
}

pub(super) fn is_putty_key(path: &std::path::Path) -> bool {
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ppk"))
    {
        return true;
    }
    const PREFIX: &[u8] = b"PuTTY-User-Key-File-";
    let mut bytes = [0; PREFIX.len()];
    path.is_file()
        && std::fs::File::open(path)
            .and_then(|mut file| file.read_exact(&mut bytes))
            .is_ok()
        && bytes == PREFIX
}

fn record(diagnostics: &Diagnostics, message: String) {
    let mut buffer = diagnostics.lock().unwrap();
    if buffer.len() == 6 {
        buffer.pop_front();
    }
    buffer.push_back(message.chars().take(600).collect());
}

struct Verifier {
    host: String,
    port: u16,
    hosts: PathBuf,
    diagnostics: Diagnostics,
}

impl Verifier {
    fn verify(&self, presented: &PublicKeyOrCertificate) -> Result<(), String> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = presented else {
            return Err("请为密码连接的 SSH 主机登记公钥。此入口尚未验证 SSH 主机证书。".into());
        };
        let encoded = key.to_openssh().map_err(|_| "无法解析 SSH 主机公钥。")?;
        let blob = encoded
            .split_whitespace()
            .nth(1)
            .ok_or("SSH 主机公钥缺少有效内容。")?;
        let file = std::fs::File::open(&self.hosts)
            .map_err(|error| format!("无法读取 SSH 主机密钥文件：{error}"))?;
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|error| format!("无法读取 SSH 主机密钥条目：{error}"))?;
            let mut fields = line.split_whitespace();
            if fields.next() == Some("@revoked") && fields.nth(2) == Some(blob) {
                return Err("SSH 主机公钥已在密钥文件中吊销。".into());
            }
        }
        match russh::keys::known_hosts::check_known_hosts_path(
            &self.host,
            self.port,
            key,
            &self.hosts,
        ) {
            Ok(true) => Ok(()),
            Ok(false) => Err("SSH 主机公钥未登记。请先核实并登记该主机的密钥。".into()),
            Err(error) => Err(format!("SSH 主机公钥验证失败：{error}")),
        }
    }
}

impl client::Handler for Verifier {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(match self.verify(public_key) {
            Ok(()) => true,
            Err(error) => {
                record(&self.diagnostics, error);
                false
            }
        })
    }

    // The application opens only direct-tcpip channels. Drop unsolicited
    // server open handles to reject them.
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        _: Channel<Msg>,
        _: &str,
        _: u32,
        _: &str,
        _: u32,
        _: ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn server_channel_open_forwarded_streamlocal(
        &mut self,
        _: Channel<Msg>,
        _: &str,
        _: ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn server_channel_open_agent_forward(
        &mut self,
        _: Channel<Msg>,
        _: ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn server_channel_open_session(
        &mut self,
        _: Channel<Msg>,
        _: ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn server_channel_open_direct_tcpip(
        &mut self,
        _: Channel<Msg>,
        _: &str,
        _: u32,
        _: &str,
        _: u32,
        _: ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn server_channel_open_direct_streamlocal(
        &mut self,
        _: Channel<Msg>,
        _: &str,
        _: ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn server_channel_open_x11(
        &mut self,
        _: Channel<Msg>,
        _: &str,
        _: u32,
        _: ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use russh::keys::PrivateKey;
    use russh::keys::ssh_key::{
        PublicKey,
        private::{Ed25519Keypair, KeypairData},
    };
    use russh::server::{self, Auth, ChannelOpenHandle, Msg, Session};

    use super::*;
    use crate::database::{
        Cell, Command, ConnectionConfig, DbObject, ObjectKind, Response, TlsClientIdentity,
        TlsConfig, Worker,
    };

    struct Gateway {
        database_port: u16,
        database_host: &'static str,
        allowed_key: Option<PublicKey>,
        authentications: Arc<AtomicUsize>,
    }

    impl server::Handler for Gateway {
        type Error = russh::Error;

        async fn auth_publickey(
            &mut self,
            user: &str,
            key: &PublicKey,
        ) -> Result<Auth, Self::Error> {
            self.authentications.fetch_add(1, Ordering::Relaxed);
            Ok(
                if user == "harbor_fixture"
                    && self
                        .allowed_key
                        .as_ref()
                        .is_some_and(|allowed| allowed.key_data() == key.key_data())
                {
                    Auth::Accept
                } else {
                    Auth::reject()
                },
            )
        }

        async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
            self.authentications.fetch_add(1, Ordering::Relaxed);
            Ok(
                if user == "harbor_fixture" && password == "synthetic-fixture-password" {
                    Auth::Accept
                } else {
                    Auth::reject()
                },
            )
        }

        async fn channel_open_direct_tcpip(
            &mut self,
            channel: Channel<Msg>,
            host: &str,
            port: u32,
            _: &str,
            _: u32,
            reply: ChannelOpenHandle,
            _: &mut Session,
        ) -> Result<(), Self::Error> {
            if host != self.database_host || port != u32::from(self.database_port) {
                return Ok(());
            }
            let mut database =
                match tokio::net::TcpStream::connect(("127.0.0.1", self.database_port)).await {
                    Ok(database) => database,
                    Err(_) => return Ok(()),
                };
            reply.accept().await;
            tokio::spawn(async move {
                let mut stream = channel.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut stream, &mut database).await;
            });
            Ok(())
        }
    }

    struct GatewayFixture {
        port: u16,
        line: String,
        public_key: String,
        counter: Arc<AtomicUsize>,
        stop: Option<oneshot::Sender<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl GatewayFixture {
        fn start(port: u16, database_host: &'static str, allowed_key: Option<PublicKey>) -> Self {
            let server = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let gateway_port = server.local_addr().unwrap().port();
            server.set_nonblocking(true).unwrap();
            let fixture_key = PrivateKey::new(
                KeypairData::Ed25519(Ed25519Keypair::from_seed(&[7; 32])),
                "synthetic-fixture",
            )
            .unwrap();
            let encoded = fixture_key.public_key().to_openssh().unwrap();
            let key_line = format!("[127.0.0.1]:{gateway_port} {encoded}\n");
            let config = Arc::new(server::Config {
                keys: vec![fixture_key],
                auth_rejection_time: Duration::ZERO,
                ..Default::default()
            });
            let counter = Arc::new(AtomicUsize::new(0));
            let worker_counter = Arc::clone(&counter);
            let (signal_stop, mut stop) = oneshot::channel();
            let gateway = std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                let server = tokio::net::TcpListener::from_std(server).unwrap();
                loop {
                    tokio::select! {
                        _ = &mut stop => break,
                        accepted = server.accept() => {
                            let (stream, _) = accepted.unwrap();
                            let handler = Gateway { database_port: port, database_host, allowed_key: allowed_key.clone(), authentications: Arc::clone(&worker_counter) };
                            let config = Arc::clone(&config);
                            tokio::spawn(async move {
                                if let Ok(session) = server::run_stream(config, stream, handler).await {
                                    let _ = session.await;
                                }
                            });
                        }
                    }
                }
            });
            });
            Self {
                port: gateway_port,
                line: key_line,
                public_key: encoded,
                counter,
                stop: Some(signal_stop),
                thread: Some(gateway),
            }
        }
    }

    impl Drop for GatewayFixture {
        fn drop(&mut self) {
            if let Some(stop) = self.stop.take() {
                let _ = stop.send(());
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn fixture_directory() -> tempfile::TempDir {
        let binary = std::env::current_exe().unwrap();
        tempfile::Builder::new()
            .prefix("harbor-ssh-fixture-")
            .tempdir_in(binary.parent().unwrap())
            .unwrap()
    }

    #[test]
    #[ignore = "以一次性 MariaDB 测试本机密码 SSH 转发，需要 HARBOR_SSH_TEST_DB_PORT"]
    fn authenticates_only_after_verifying_the_host_and_forwards_mysql() {
        let port = std::env::var("HARBOR_SSH_TEST_DB_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let gateway = GatewayFixture::start(port, "127.0.0.1", None);
        let fixture = fixture_directory();
        let pinned = fixture.path().join("known_hosts");
        let revoked = fixture.path().join("known_hosts_revoked");
        std::fs::write(&pinned, &gateway.line).unwrap();
        std::fs::write(
            &revoked,
            format!("{}@revoked * {}\n", gateway.line, gateway.public_key),
        )
        .unwrap();
        let worker = Worker::new();
        let local = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let local_port = local.local_addr().unwrap().port();
        drop(local);
        let config = |hosts| ConnectionConfig {
            port,
            ssh: Some(SshConfig {
                host: "127.0.0.1".into(),
                port: gateway.port,
                user: "harbor_fixture".into(),
                password: Some("synthetic-fixture-password".into()),
                known_hosts_path: Some(hosts),
                local_port: Some(local_port),
                ..Default::default()
            }),
            ..Default::default()
        };
        worker
            .send(
                1,
                Command::TestConnection {
                    config: config(revoked),
                    http: None,
                },
            )
            .unwrap();
        let denied = worker.recv().unwrap().result;
        assert!(denied.is_err_and(|error| error.contains("吊销")));
        assert_eq!(gateway.counter.load(Ordering::Relaxed), 0);
        worker.send(2, Command::Connect(config(pinned))).unwrap();
        assert!(matches!(
            worker.recv().unwrap().result.unwrap(),
            Response::Connected(_)
        ));
        worker
            .send(
                3,
                Command::Query {
                    sql: "SELECT '隧道连通'".into(),
                    limit: 1,
                },
            )
            .unwrap();
        let Response::Queried(report) = worker.recv().unwrap().result.unwrap() else {
            panic!("未返回查询结果。");
        };
        assert_eq!(report.sets[0].rows, [vec![Cell::Text("隧道连通".into())]]);
        assert_eq!(gateway.counter.load(Ordering::Relaxed), 1);
        worker.send(4, Command::Disconnect).unwrap();
        assert!(matches!(
            worker.recv().unwrap().result.unwrap(),
            Response::Disconnected
        ));
    }

    #[test]
    #[ignore = "以一次性 MariaDB 验证独立 SSH 连接，需要 HARBOR_SSH_TEST_DB_PORT"]
    fn clones_an_ssh_connection_with_an_independent_forwarding_port() {
        let port = std::env::var("HARBOR_SSH_TEST_DB_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let gateway = GatewayFixture::start(port, "127.0.0.1", None);
        let fixture = fixture_directory();
        let known_hosts = fixture.path().join("known_hosts");
        std::fs::write(&known_hosts, &gateway.line).unwrap();
        let local = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let local_port = local.local_addr().unwrap().port();
        drop(local);
        let config = ConnectionConfig {
            port,
            ssh: Some(SshConfig {
                host: "127.0.0.1".into(),
                port: gateway.port,
                user: "harbor_fixture".into(),
                password: Some("synthetic-fixture-password".into()),
                known_hosts_path: Some(known_hosts),
                local_port: Some(local_port),
                ..Default::default()
            }),
            ..Default::default()
        };
        let source = Worker::new();
        let request = |worker: &Worker, id, command| {
            worker.send(id, command).unwrap();
            let event = worker
                .recv_timeout(Duration::from_secs(12))
                .unwrap()
                .unwrap();
            assert_eq!(event.id, id);
            event.result.unwrap()
        };
        let Response::Connected(original) = request(&source, 1, Command::Connect(config)) else {
            panic!("未返回原连接信息。");
        };
        let Response::ClonedConnection {
            worker,
            request: id,
        } = request(&source, 2, Command::CloneConnection)
        else {
            panic!("未返回新连接服务。");
        };
        let event = worker
            .recv_timeout(Duration::from_secs(12))
            .unwrap()
            .unwrap();
        assert_eq!(event.id, id);
        let Response::Connected(cloned) = event.result.unwrap() else {
            panic!("未返回新连接信息。");
        };
        assert_ne!(original.connection_id, cloned.connection_id);
        assert_eq!(gateway.counter.load(Ordering::Relaxed), 2);
        assert!(matches!(
            request(&source, 3, Command::Disconnect),
            Response::Disconnected
        ));
        let Response::Queried(report) = request(
            &worker,
            id + 1,
            Command::Query {
                sql: "SELECT '新隧道仍连接'".into(),
                limit: 1,
            },
        ) else {
            panic!("未返回查询结果。");
        };
        assert!(report.error.is_none());
        assert_eq!(
            report.sets[0].rows,
            [vec![Cell::Text("新隧道仍连接".into())]]
        );
        assert!(matches!(
            request(&worker, id + 2, Command::Disconnect),
            Response::Disconnected
        ));
    }

    #[test]
    #[ignore = "需要一次性 MariaDB 和 TLS 证书：HARBOR_TLS_CLONE_FIXTURE_PORT、HARBOR_TLS_FIXTURE_DIR"]
    fn clones_two_way_tls_connections_directly_and_inside_ssh() {
        use mysql::{Conn, OptsBuilder, prelude::Queryable};
        use std::path::PathBuf;

        let port = std::env::var("HARBOR_TLS_CLONE_FIXTURE_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let tls = PathBuf::from(std::env::var_os("HARBOR_TLS_FIXTURE_DIR").unwrap());
        let remote_host = "db.harbor.invalid";
        let gateway = GatewayFixture::start(port, remote_host, None);
        let folder = fixture_directory();
        let known_hosts = folder.path().join("known_hosts");
        std::fs::write(&known_hosts, &gateway.line).unwrap();
        let mut admin = Conn::new(
            OptsBuilder::new()
                .ip_or_hostname(Some("127.0.0.1"))
                .tcp_port(port)
                .user(Some("root"))
                .prefer_socket(false),
        )
        .unwrap();

        let schema = format!("harbor_tls_clone_{}", std::process::id());
        let account = format!("harbor_tls_clone_{}", std::process::id());
        let qualified = format!("'{account}'@'127.0.0.1'");
        admin
            .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
            .unwrap();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            admin
                .query_drop(format!(
                    "CREATE USER {qualified} IDENTIFIED BY '' REQUIRE X509"
                ))
                .unwrap();
            admin
                .query_drop(format!("CREATE TABLE {schema}.sample (body VARCHAR(32))"))
                .unwrap();
            admin
                .query_drop(format!(
                    "INSERT INTO {schema}.sample VALUES ('双向 TLS 连接')"
                ))
                .unwrap();
            admin
                .query_drop(format!("GRANT SELECT ON {schema}.* TO {qualified}"))
                .unwrap();

            let request = |worker: &Worker, id, command| {
                worker.send(id, command).unwrap();
                let event = worker
                    .recv_timeout(Duration::from_secs(12))
                    .unwrap()
                    .unwrap();
                assert_eq!(event.id, id);
                event.result.unwrap()
            };
            let query = |worker: &Worker, id, sql: &str| {
                let Response::Queried(report) = request(
                    worker,
                    id,
                    Command::Query {
                        sql: sql.into(),
                        limit: 1,
                    },
                ) else {
                    panic!("未返回查询结果。");
                };
                assert!(report.error.is_none(), "{:?}", report.error);
                report.sets.into_iter().next().unwrap().rows
            };
            let ssh = SshConfig {
                host: "127.0.0.1".into(),
                port: gateway.port,
                user: "harbor_fixture".into(),
                password: Some("synthetic-fixture-password".into()),
                known_hosts_path: Some(known_hosts),
                ..Default::default()
            };
            let tls_config = TlsConfig {
                ca_path: Some(tls.join("fixture-ca.pem")),
                client_identity: Some(TlsClientIdentity {
                    certificate: tls.join("fixture-client.pem"),
                    private_key: tls.join("fixture-client.key"),
                }),
            };
            for (host, ssh) in [("127.0.0.1", None), (remote_host, Some(ssh))] {
                let source = Worker::new();
                let config = ConnectionConfig {
                    host: host.into(),
                    port,
                    user: account.clone(),
                    database: Some(schema.clone()),
                    tls: Some(tls_config.clone()),
                    ssh,
                    ..Default::default()
                };
                let Response::Connected(original) = request(&source, 1, Command::Connect(config))
                else {
                    panic!("未返回原连接信息。");
                };
                let status = query(&source, 2, "SHOW SESSION STATUS LIKE 'Ssl_cipher'");
                assert!(matches!(&status[0][1], Cell::Text(cipher) if !cipher.is_empty()));
                let Response::ClonedConnection {
                    worker,
                    request: id,
                } = request(&source, 3, Command::CloneConnection)
                else {
                    panic!("未返回新连接服务。");
                };
                let event = worker
                    .recv_timeout(Duration::from_secs(12))
                    .unwrap()
                    .unwrap();
                assert_eq!(event.id, id);
                let Response::Connected(cloned) = event.result.unwrap() else {
                    panic!("未返回新连接信息。");
                };
                assert_ne!(original.connection_id, cloned.connection_id);
                assert_eq!(cloned.database.as_ref(), Some(&schema));
                assert!(matches!(
                    request(&source, 4, Command::Disconnect),
                    Response::Disconnected
                ));
                let status = query(&worker, id + 1, "SHOW SESSION STATUS LIKE 'Ssl_cipher'");
                assert!(matches!(&status[0][1], Cell::Text(cipher) if !cipher.is_empty()));
                assert_eq!(
                    query(&worker, id + 2, "SELECT body FROM sample"),
                    [vec![Cell::Text("双向 TLS 连接".into())]]
                );
                request(&worker, id + 3, Command::Disconnect);
            }
            assert_eq!(gateway.counter.load(Ordering::Relaxed), 2);
        }));
        admin
            .query_drop(format!("DROP USER IF EXISTS {qualified}"))
            .unwrap();
        admin
            .query_drop(format!("DROP DATABASE IF EXISTS {schema}"))
            .unwrap();
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }

    #[test]
    #[ignore = "requires disposable MariaDB, TLS fixture and encrypted PPK fixture via HARBOR_SSH_TEST_DB_PORT, HARBOR_TLS_FIXTURE_DIR, HARBOR_TEST_PPK and HARBOR_TEST_PPK_PASSPHRASE"]
    fn verifies_the_remote_database_certificate_inside_ssh_and_reuses_tls_for_transfers() {
        use mysql::{Conn, OptsBuilder, prelude::Queryable};
        use std::path::PathBuf;
        let port = std::env::var("HARBOR_SSH_TEST_DB_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let tls = PathBuf::from(std::env::var_os("HARBOR_TLS_FIXTURE_DIR").unwrap());
        let ppk = PathBuf::from(std::env::var_os("HARBOR_TEST_PPK").unwrap());
        let passphrase = std::env::var("HARBOR_TEST_PPK_PASSPHRASE").unwrap();
        let public_key = private_key(&ppk, Some(&passphrase))
            .unwrap()
            .public_key()
            .clone();
        let host = "db.harbor.invalid";
        let gateway = GatewayFixture::start(port, host, Some(public_key));
        let folder = fixture_directory();
        let known_hosts = folder.path().join("known_hosts");
        std::fs::write(&known_hosts, &gateway.line).unwrap();
        let mut admin = Conn::new(
            OptsBuilder::new()
                .ip_or_hostname(Some("127.0.0.1"))
                .tcp_port(port)
                .user(Some("root"))
                .prefer_socket(false),
        )
        .unwrap();
        let schema = format!("harbor_tls_{}", std::process::id());
        let account = format!("harbor_x509_{}", std::process::id());
        let qualified = format!("'{account}'@'127.0.0.1'");
        admin
            .query_drop(format!("CREATE DATABASE {schema} CHARACTER SET utf8mb4"))
            .unwrap();
        admin
            .query_drop(format!(
                "CREATE USER {qualified} IDENTIFIED BY '' REQUIRE X509"
            ))
            .unwrap();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            admin.query_drop(format!("CREATE TABLE {schema}.sample (id INT PRIMARY KEY, body VARCHAR(32)) ENGINE=InnoDB")).unwrap();
            admin
                .query_drop(format!(
                    "INSERT INTO {schema}.sample VALUES (1,'隧道内的 TLS')"
                ))
                .unwrap();
            admin
                .query_drop(format!("GRANT SELECT ON {schema}.* TO {qualified}"))
                .unwrap();
            let worker = Worker::new();
            let tls_config = TlsConfig {
                ca_path: Some(tls.join("fixture-ca.pem")),
                client_identity: Some(TlsClientIdentity {
                    certificate: tls.join("fixture-client.pem"),
                    private_key: tls.join("fixture-client.key"),
                }),
            };
            let config = ConnectionConfig {
                host: host.into(),
                port,
                user: account.clone(),
                database: Some(schema.clone()),
                tls: Some(tls_config),
                ssh: Some(SshConfig {
                    host: "127.0.0.1".into(),
                    port: gateway.port,
                    user: "harbor_fixture".into(),
                    password: Some("synthetic-fixture-password".into()),
                    known_hosts_path: Some(known_hosts.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let request = |id, command| {
                worker.send(id, command).unwrap();
                worker
                    .recv_timeout(Duration::from_secs(12))
                    .unwrap()
                    .unwrap()
                    .result
                    .unwrap()
            };
            assert!(matches!(
                request(1, Command::Connect(config)),
                Response::Connected(_)
            ));
            let Response::Queried(report) = request(
                2,
                Command::Query {
                    sql: "SHOW SESSION STATUS LIKE 'Ssl_cipher'".into(),
                    limit: 1,
                },
            ) else {
                panic!("expected TLS status");
            };
            assert!(matches!(&report.sets[0].rows[0][1], Cell::Text(cipher) if !cipher.is_empty()));
            let Response::TablePage(page) = request(
                3,
                Command::TablePage {
                    object: DbObject {
                        schema: schema.clone(),
                        name: "sample".into(),
                        kind: ObjectKind::Table,
                    },
                    limit: 10,
                    offset: 0,
                },
            ) else {
                panic!("expected data over an independent connection");
            };
            assert_eq!(page.rows[0].cells[1], Cell::Text("隧道内的 TLS".into()));

            let job = folder.path().join("ssh-tls-job.json");
            let settings = serde_json::json!({
                "version": 1,
                "connection": {
                    "host":host, "port":port, "user":account,
                    "tls": {
                        "ca_path": tls.join("fixture-ca.pem"),
                        "client_certificate": tls.join("fixture-client.pem"),
                        "client_key": tls.join("fixture-client.key")
                    },
                    "ssh": {
                        "host":"127.0.0.1", "port":gateway.port, "user":"harbor_fixture",
                        "known_hosts_path": known_hosts, "identity_file":ppk, "key_passphrase_env":"HARBOR_TEST_PPK_PASSPHRASE"
                    }
                },
                "job": {"type":"sql","sql":"SELECT 42","report":"report.html"}
            });
            std::fs::write(&job, serde_json::to_vec(&settings).unwrap()).unwrap();
            assert_eq!(crate::jobs::cli(["run".into(), job.into_os_string()]), 0);
            assert!(
                std::fs::read_to_string(folder.path().join("report.html"))
                    .unwrap()
                    .contains("42")
            );
            assert_eq!(gateway.counter.load(Ordering::Relaxed), 2);
        }));
        admin
            .query_drop(format!("DROP USER IF EXISTS {qualified}"))
            .unwrap();
        admin
            .query_drop(format!("DROP DATABASE IF EXISTS {schema}"))
            .unwrap();
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }
}
