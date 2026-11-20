use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use super::{tests::read_http_request, workflow_test::archived_tunnel, *};

#[test]
#[ignore = "requires local PHP, MariaDB and generated DER files in HARBOR_HTTPS_TLS_FIXTURE_DIR"]
fn queries_archived_php_over_certificate_verified_https() {
    let upstream = archived_tunnel().expect("requires HARBOR_LEGACY_TUNNEL_URL");
    let upstream_port = Url::parse(&upstream.url).unwrap().port().unwrap();
    let database_port = std::env::var("HARBOR_HTTP_FIXTURE_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let keys = PathBuf::from(std::env::var_os("HARBOR_HTTPS_TLS_FIXTURE_DIR").unwrap());
    let root =
        reqwest::Certificate::from_der(&std::fs::read(keys.join("ca.der")).unwrap()).unwrap();
    let certificate = CertificateDer::from(std::fs::read(keys.join("server.der")).unwrap());
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        std::fs::read(keys.join("server-key.der")).unwrap(),
    ));
    let tls = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], private_key)
        .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let relay = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(25);
        // Version probe, connection metadata and one SELECT.
        for _ in 0..3 {
            let connection = loop {
                assert!(
                    Instant::now() < deadline,
                    "HTTPS fixture listener timed out"
                );
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(3));
                    }
                    Err(error) => panic!("HTTPS fixture listener failed: {error}"),
                }
            };
            connection.set_nonblocking(false).unwrap();
            connection
                .set_read_timeout(Some(Duration::from_secs(8)))
                .unwrap();
            connection
                .set_write_timeout(Some(Duration::from_secs(8)))
                .unwrap();
            let mut client = rustls::StreamOwned::new(
                rustls::ServerConnection::new(tls.clone()).unwrap(),
                connection,
            );
            let (header, body) = read_http_request(&mut client);
            let mut php = TcpStream::connect(("127.0.0.1", upstream_port)).unwrap();
            php.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
            php.write_all(header.as_bytes()).unwrap();
            php.write_all(&body).unwrap();
            io::copy(&mut php, &mut client).unwrap();
            client.conn.send_close_notify();
            client.flush().unwrap();
        }
    });
    let remote = Url::parse(&upstream.url).unwrap();
    let config = HttpTunnelConfig {
        url: format!("https://{address}{}", remote.path()),
        timeout: 10,
        ..upstream
    };
    let connection = ConnectionConfig {
        port: database_port,
        ..Default::default()
    };
    let (session, server) =
        Session::open_with_builder(connection, config, Client::builder().tls_certs_only([root]))
            .unwrap();
    assert!(server.http_tunnel);
    let report = session
        .checked_query("SELECT '港湾' AS label, 29 AS item_id", 1)
        .unwrap();
    assert_eq!(report.sets[0].columns, ["label", "item_id"]);
    assert!(matches!(&report.sets[0].rows[0][0], Cell::Text(label) if label == "港湾"));
    assert!(matches!(&report.sets[0].rows[0][1], Cell::Text(id) if id == "29"));
    relay.join().unwrap();
}
