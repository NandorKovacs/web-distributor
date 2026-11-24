use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;

struct Installation {
    root: TempDir,
    config: PathBuf,
    output: PathBuf,
}

impl Installation {
    fn new(inventory: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        fs::write(
            &config,
            format!("home = {:?}\n{inventory}", root.path().join("home")),
        )
        .unwrap();
        let output = root.path().join("caddy").join("generated.caddy");
        Self {
            root,
            config,
            output,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_web-distributor"));
        command
            .arg("-c")
            .arg(&self.config)
            .arg("--output")
            .arg(&self.output);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn succeeds(&self, args: &[&str]) {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn fragment(&self) -> String {
        fs::read_to_string(&self.output).unwrap()
    }
}

#[test]
fn generates_deterministically_from_legacy_config_without_touching_acme() {
    let install = Installation::new(
        r#"
acme_redirect_configs = "legacy-acme"
certificate_only = ["mail.example.test"]
[routes]
"z.example.test" = "upstream.incus:8443"
"a.example.test" = "127.0.0.1:8000"
[login_groups]
"removed.example.test" = "missing"
[forward_auth]
"a.example.test" = "192.0.2.4:9000"
[grpc_prefix]
"z.example.test" = "/Service.v1."
"removed.example.test" = "/old."
"#,
    );
    let legacy_dir = install.root.path().join("legacy-acme");
    fs::create_dir(&legacy_dir).unwrap();
    let legacy_file = legacy_dir.join("web-distributor.example.test.conf");
    fs::write(&legacy_file, "legacy contents").unwrap();

    assert!(install
        .command()
        .current_dir(install.root.path())
        .arg("generate")
        .status()
        .unwrap()
        .success());
    let fragment = install.fragment();
    assert!(fragment.find("a.example.test").unwrap() < fragment.find("z.example.test").unwrap());
    assert!(fragment.contains(
        "mail.example.test {\n\theader Strict-Transport-Security max-age=63072000\n\trespond 404"
    ));
    assert!(fragment.contains("reverse_proxy @grpc h2c://upstream.incus:8443"));
    assert!(fragment.contains(r"path_regexp ^/Service\.v1\."));
    assert!(fragment.contains("/outpost.goauthentik.io/auth/caddy"));
    assert!(!fragment.contains("removed.example.test"));
    assert_eq!(fs::read_to_string(legacy_file).unwrap(), "legacy contents");
    assert_eq!(
        fs::metadata(&install.output).unwrap().permissions().mode() & 0o777,
        0o640
    );
    install.succeeds(&["generate"]);
    assert_eq!(install.fragment(), fragment);
}

#[test]
fn invalid_edits_leave_existing_source_and_output_readable() {
    let install = Installation::new("[routes]\n\"example.test\" = \"backend\"\n");
    install.succeeds(&["generate"]);
    let original = fs::read(&install.config).unwrap();
    let fragment = install.fragment();
    assert!(!install
        .run(&["add", "other.example.test", "backend\nrespond 200"])
        .status
        .success());
    assert!(!install
        .run(&["forward-auth", "apply", "example.test", "backend:99999"])
        .status
        .success());
    assert!(!install
        .run(&[
            "add",
            "--grpc-prefix",
            "/service*",
            "other.example.test",
            "backend"
        ])
        .status
        .success());
    assert!(!install
        .command()
        .arg("generate")
        .arg("--output")
        .arg(&install.config)
        .output()
        .unwrap()
        .status
        .success());
    assert!(!install
        .command()
        .arg("generate")
        .arg("--config")
        .arg(install.root.path().join("typo.toml"))
        .output()
        .unwrap()
        .status
        .success());
    assert_eq!(fs::read(&install.config).unwrap(), original);
    assert_eq!(install.fragment(), fragment);
}

#[test]
fn credential_edits_use_exact_usernames_and_protection_survives_route_removal() {
    let install = Installation::new("[routes]\n\"example.test\" = \"backend\"\n");
    install.succeeds(&["login-group", "create", "writers"]);
    install.succeeds(&["login-group", "apply", "example.test", "writers"]);
    assert!(install
        .fragment()
        .contains("basic_auth bcrypt login {\n\t\t}"));
    let group = install.root.path().join("home/login_groups/writers");
    let hash = bcrypt::hash("test password", 4).unwrap();
    fs::write(&group, format!("\nanna:{hash}\n")).unwrap();
    install.succeeds(&[
        "login-group",
        "add-login",
        "writers",
        "ann",
        "test password",
    ]);
    assert!(install.fragment().contains("\"ann\" "));
    assert!(install.fragment().contains(&format!("\"anna\" {hash}")));
    install.succeeds(&["login-group", "revoke-login", "writers", "ann"]);
    assert_eq!(
        fs::read_to_string(&group).unwrap(),
        format!("anna:{hash}\n")
    );
    assert!(!install.fragment().contains("\"ann\" "));
    install.succeeds(&["forward-auth", "apply", "example.test", "outpost:9000"]);
    install.succeeds(&["remove", "example.test"]);
    assert!(!install.fragment().contains("example.test"));
    install.succeeds(&["add", "example.test", "backend"]);
    assert!(install.fragment().contains("basic_auth bcrypt login"));
    assert!(install
        .fragment()
        .contains("reverse_proxy @outpost outpost:9000"));
}

#[test]
fn concurrent_route_edits_do_not_drop_updates() {
    let install = Installation::new("[routes]\n");
    let domains = (0..5)
        .map(|i| format!("example{i}.test"))
        .collect::<Vec<_>>();
    let children = domains
        .iter()
        .map(|domain| {
            install
                .command()
                .args(["add", domain, "backend"])
                .spawn()
                .unwrap()
        })
        .collect::<Vec<_>>();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    for domain in domains {
        assert!(install.fragment().contains(&domain));
    }
}
