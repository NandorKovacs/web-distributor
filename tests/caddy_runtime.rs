//! Run with CADDY=/path/to/caddy cargo test --test caddy_runtime -- --ignored
//! Requires curl with HTTP/2 support. All listeners and PKI are local to the test.
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct Sandbox(PathBuf);
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn ports() -> [u16; 4] {
    let sockets = [(); 4].map(|_| TcpListener::bind(("127.0.0.1", 0)).unwrap());
    sockets.map(|s| s.local_addr().unwrap().port())
}

fn request(scheme: &str, host: &str, port: u16, path: &str, extra: &[&str]) -> io::Result<String> {
    let out = Command::new("curl")
        .args([
            "-q",
            "-ksiS",
            "--max-time",
            "3",
            "--noproxy",
            "*",
            "--resolve",
            &format!("{host}:{port}:127.0.0.1"),
        ])
        .args(extra)
        .arg(format!("{scheme}://{host}:{port}{path}"))
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn generate(config: &Path, routes: &Path) {
    let out = Command::new(env!("CARGO_BIN_EXE_web-distributor"))
        .arg("-c")
        .arg(config)
        .args(["generate", "--output"])
        .arg(routes)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
#[ignore = "requires real Caddy and curl"]
fn generated_routes_serve_real_requests() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let tmp =
        Sandbox(std::env::temp_dir().join(format!("wd-runtime-{}-{stamp}", std::process::id())));
    fs::create_dir(&tmp.0).unwrap();
    let groups = tmp.0.join("login_groups");
    fs::create_dir(&groups).unwrap();
    fs::write(
        groups.join("staff"),
        format!("tester:{}\n", bcrypt::hash("test-password", 4).unwrap()),
    )
    .unwrap();
    let ws_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let ws_port = ws_listener.local_addr().unwrap().port();
    let grpc_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let grpc_port = grpc_listener.local_addr().unwrap().port();
    let [http, https, backend, outpost] = ports();
    let config = tmp.0.join("config.toml");
    fs::write(
        &config,
        format!(
            r#"
home = {home:?}
acme_redirect_configs = {home:?}
certificate_only = ["cert.example.test"]
[routes]
"app.example.test" = "127.0.0.1:{backend}"
"staff.example.test" = "127.0.0.1:{backend}"
"sso.example.test" = "127.0.0.1:{backend}"
"ws.example.test" = "127.0.0.1:{ws_port}"
[login_groups]
"staff.example.test" = "staff"
[forward_auth]
"sso.example.test" = "127.0.0.1:{outpost}"
[grpc_prefix]
"app.example.test" = "/inventory.v1."
"#,
            home = tmp.0
        ),
    )
    .unwrap();
    generate(&config, &tmp.0.join("routes.caddy"));
    fs::write(tmp.0.join("Caddyfile"),format!(r#"{{
    admin off
    default_bind 127.0.0.1
    local_certs
    skip_install_trust
    http_port {http}
    https_port {https}
    servers {{
        protocols h1 h2 h2c
    }}
}}
import routes.caddy
http://:{backend} {{
    handle /inventory.v1.Stock/List {{
        header Upstream-Protocol {{http.request.proto}}
        reverse_proxy 127.0.0.1:{grpc_port}
    }}
    handle {{
        respond "{{http.request.proto}}|{{http.request.uri}}|{{http.request.header.X-Authentik-Username}}|{{http.request.header.X-Forwarded-Proto}}|{{http.request.method}}|{{http.request.body}}|{{http.request.header.X-Authentik-Groups}}"
    }}
}}
http://:{outpost} {{
    @allow {{
        path /outpost.goauthentik.io/auth/caddy
        method GET
        header Cookie session=ok
        header X-Forwarded-Method POST
        header X-Forwarded-Uri /private?q=1
    }}
    handle @allow {{
        header X-Authentik-Username verified
        header Set-Cookie refresh=yes
        respond "" 200
    }}
    handle /outpost.goauthentik.io/auth/caddy {{
        header Set-Cookie login=yes
        header Location /outpost.goauthentik.io/start?rd=/private
        respond "" 302
    }}
    handle /outpost.goauthentik.io/start {{
        respond "outpost-login"
    }}
    respond 404
}}
"#)).unwrap();
    let logs = tmp.0.join("caddy.log");
    let caddy = std::env::var_os("CADDY").unwrap_or_else(|| "caddy".into());
    let mut server = Server(
        Command::new(caddy)
            .args(["run", "--config", "Caddyfile"])
            .current_dir(&tmp.0)
            .env("HOME", &tmp.0)
            .env("XDG_DATA_HOME", &tmp.0)
            .env("XDG_CONFIG_HOME", &tmp.0)
            .stdout(Stdio::null())
            .stderr(fs::File::create(&logs).unwrap())
            .spawn()
            .unwrap(),
    );
    let host = "app.example.test";
    let mut ready = false;
    for _ in 0..100 {
        if server.0.try_wait().unwrap().is_some() {
            break;
        }
        if request("https", host, https, "/", &[]).is_ok() {
            ready = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(ready, "{}", fs::read_to_string(&logs).unwrap());
    let get = |host, path, args: &[&str]| request("https", host, https, path, args).unwrap();
    let redirect = request("http", host, http, "/a?q=1", &[]).unwrap();
    assert!(
        redirect.contains(&format!("https://{host}/a?q=1")),
        "{redirect}"
    );
    let plain = get(host, "/a%20b?q=1", &[]);
    assert!(plain.contains("HTTP/1.1|/a%20b?q=1||https"), "{plain}");
    assert!(
        plain
            .to_lowercase()
            .contains("strict-transport-security: max-age=63072000"),
        "{plain}"
    );
    // Caddy's h2c test backend translates this chunked response into HTTP/2
    // DATA and trailing HEADERS. Five zero bytes encode an empty gRPC message.
    let grpc_server = thread::spawn(move || {
        let (mut stream, _) = grpc_listener.accept().unwrap();
        let mut reader = BufReader::new(&stream);
        let mut first_line = String::new();
        reader.read_line(&mut first_line).unwrap();
        assert_eq!(first_line, "POST /inventory.v1.Stock/List HTTP/1.1\r\n");
        let mut header = String::new();
        loop {
            header.clear();
            assert!(reader.read_line(&mut header).unwrap() > 0);
            if header == "\r\n" {
                break;
            }
        }
        let mut frame = [0u8; 5];
        reader.read_exact(&mut frame).unwrap();
        assert_eq!(frame, [0; 5]);
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/grpc\r\nTransfer-Encoding: chunked\r\nTrailer: Grpc-Status\r\n\r\n5\r\n\0\0\0\0\0\r\n0\r\nGrpc-Status: 0\r\n\r\n").unwrap();
    });
    let frame = tmp.0.join("message.bin");
    fs::write(&frame, [0; 5]).unwrap();
    let grpc = get(
        host,
        "/inventory.v1.Stock/List",
        &[
            "--http2",
            "--data-binary",
            &format!("@{}", frame.display()),
            "-H",
            "TE: trailers",
            "-H",
            "Content-Type: application/grpc",
        ],
    );
    assert!(
        grpc.starts_with("HTTP/2 200")
            && grpc.to_lowercase().contains("upstream-protocol: http/2.0")
            && grpc.contains("\0\0\0\0\0")
            && grpc.to_lowercase().contains("grpc-status: 0"),
        "{grpc:?}"
    );
    grpc_server.join().unwrap();
    let basic = get("staff.example.test", "/", &[]);
    assert!(basic.contains(" 401"), "{basic}");
    let authed = get("staff.example.test", "/", &["-u", "tester:test-password"]);
    assert!(authed.contains(" 200"), "{authed}");
    let sso = "sso.example.test";
    let signin = get(sso, "/private?q=1", &[]);
    assert!(
        signin.contains(" 302")
            && signin.contains("/outpost.goauthentik.io/start?rd=")
            && signin.contains("login=yes"),
        "{signin}"
    );
    let ok = get(
        sso,
        "/private?q=1",
        &[
            "-H",
            "Cookie: session=ok",
            "-H",
            "X-Authentik-Username: forged",
            "-H",
            "X-Authentik-Groups: forged",
            "--data",
            "payload",
        ],
    );
    assert!(
        ok.ends_with("/private?q=1|verified|https|POST|payload|") && ok.contains("refresh=yes"),
        "{ok}"
    );
    assert!(get(sso, "/outpost.goauthentik.io/start", &[]).contains("outpost-login"));
    assert!(get("cert.example.test", "/", &[]).contains(" 404"));
    // Fixed RFC 6455 example handshake followed by a text frame.
    let ws_server = thread::spawn(move || {
        let (mut stream, _) = ws_listener.accept().unwrap();
        for line in BufReader::new(&stream).lines() {
            if line.unwrap().is_empty() {
                break;
            }
        }
        stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n\x81\x05hello").unwrap();
    });
    let ws = get(
        "ws.example.test",
        "/socket",
        &[
            "--http1.1",
            "-H",
            "Connection: Upgrade",
            "-H",
            "Upgrade: websocket",
            "-H",
            "Sec-WebSocket-Version: 13",
            "-H",
            "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==",
        ],
    );
    assert!(ws.contains(" 101") && ws.ends_with("hello"), "{ws}");
    ws_server.join().unwrap();
}
