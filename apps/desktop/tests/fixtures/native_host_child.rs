//! Standalone, dependency-free synthetic Core/CLI. Tests compile it with rustc and copy it
//! into a temporary sealed-runtime directory. It never calls a provider or system service.
use std::{
    env, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};
const ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
fn string(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    )
}
fn field(input: &str, key: &str) -> Option<String> {
    let at = input.find(&format!("\"{key}\""))?;
    let mut chars = input[at + key.len() + 2..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?
        .chars();
    let mut value = String::new();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(value),
            '\\' => value.push(match chars.next()? {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                ch => ch,
            }),
            ch => value.push(ch),
        }
    }
    None
}
fn option(args: &[String], key: &str) -> Option<PathBuf> {
    let at = args.iter().position(|arg| arg == key)?;
    args.get(at + 1).map(PathBuf::from)
}
fn private_dir(path: &Path) {
    fs::create_dir_all(path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
}
fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
}
fn inspect(path: &Path) -> String {
    let running = path.join("running").exists();
    let socket = if cfg!(windows) {
        r"\\.\pipe\vibeface-fixture".to_owned()
    } else {
        path.join("fixture.sock").to_string_lossy().into_owned()
    };
    format!(
        r#"{{"data_dir":{},"device_id":{},"status":{},"local_listen":"127.0.0.1:4791","listen":"127.0.0.1:4790","web_enabled":true,"core_version":"2.0.0","error":{},"maintenance_socket":{},"public_url":"http://127.0.0.1:4790","http_enabled":{running},"version":{},"write_state":null,"mesh":{{"enabled":false,"network":"vibeface","records":null,"neighbors":null}},"peers":{}}}"#,
        string(&path.to_string_lossy()),
        string(&fs::read_to_string(path.join("fixture.identity")).unwrap_or_else(|_| ID.into())),
        string(if running { "running" } else { "stopped" }),
        if running {
            "null".into()
        } else {
            string("fixture stopped")
        },
        string(&socket),
        if running {
            string("2.0.0")
        } else {
            "null".into()
        },
        if running { "[]" } else { "null" },
    )
}
fn status(profile: &Path) -> String {
    let installed = profile.join("service.installed").exists();
    let runtime = if installed {
        fs::read_to_string(profile.join("fixture.availability"))
            .unwrap_or_else(|_| "current".into())
    } else {
        "not_installed".into()
    };
    let running = profile.join("node/running").exists();
    let pending = profile.join("fixture.recovery").exists();
    let version = if runtime == "newer_available"
        || runtime == "downgrade_refused"
        || runtime == "immutable_conflict"
    {
        "1.0.0"
    } else {
        "2.0.0"
    };
    let update = format!(
        "{{\"bundled_version\":\"2.0.0\",\"installed_version\":{},\"running_version\":{},\"availability\":{},\"recovery_required\":{pending}}}",
        if installed {
            string(version)
        } else {
            "null".into()
        },
        if installed && running {
            string(version)
        } else {
            "null".into()
        },
        string(&runtime)
    );
    if !installed {
        return format!(
            "{{\"state\":\"not_installed\",\"device_id\":null,\"data_dir\":null,\"service_id\":null,\"managed_root\":null,\"service_running\":null,\"core_status\":null,\"runtime_update\":{update}}}"
        );
    }
    format!(
        "{{\"state\":{},\"device_id\":{},\"data_dir\":{},\"service_id\":\"fixture.service\",\"managed_root\":{},\"service_running\":{running},\"core_status\":{},\"runtime_update\":{update}}}",
        string(if running {
            "installed"
        } else {
            "installed_stopped"
        }),
        string(ID),
        string(&profile.join("node").to_string_lossy()),
        string(&profile.join("managed").to_string_lossy()),
        string(if running { "running" } else { "stopped" })
    )
}
fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--control") {
        let mut input = String::new();
        io::stdin()
            .take(1024 * 1024)
            .read_to_string(&mut input)
            .unwrap();
        let command = field(&input, "command").unwrap();
        let result = match command.as_str() {
            "init" => {
                let path = PathBuf::from(field(&input, "data_dir").unwrap());
                private_dir(&path);
                write(&path.join("config.json"), b"{}");
                write(&path.join("identity.json"), b"{}");
                "{}".into()
            }
            "inspect" => inspect(&PathBuf::from(field(&input, "data_dir").unwrap())),
            "discover_local_nodes" => {
                let profile = PathBuf::from(field(&input, "app_profile_dir").unwrap());
                let node = profile.join("node");
                if node.join("identity.json").exists() {
                    let running = node.join("running").exists();
                    let status = if running { "running" } else { "stopped" };
                    let id = fs::read_to_string(node.join("fixture.identity")).unwrap_or_else(|_| ID.into());
                    format!(
                        "{{\"nodes\":[{{\"status\":{},\"device_id\":{},\"data_dir\":{},\"sources\":[\"app_profile\"],\"local_listen\":\"127.0.0.1:4791\",\"listen\":\"127.0.0.1:4790\",\"web_enabled\":true,\"core_version\":\"2.0.0\"}}],\"warnings\":[],\"user_service_registered\":false}}",
                        string(status), string(&id), string(&node.to_string_lossy())
                    )
                } else {
                    "{\"nodes\":[],\"warnings\":[],\"user_service_registered\":false}".into()
                }
            }
            _ => panic!("unexpected control command"),
        };
        println!("{{\"ok\":true,\"result\":{result}}}");
    } else if args.first().is_some_and(|arg| arg == "desktop-node") {
        let profile = option(&args, "--profile-dir").unwrap();
        let action = args.last().unwrap().as_str();
        let mut calls = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(profile.join("fixture.calls"))
            .unwrap();
        writeln!(calls, "{}", args.join("\t")).unwrap();
        if action == "status" && profile.join("fixture.status").exists() {
            println!(
                "{}",
                fs::read_to_string(profile.join("fixture.status")).unwrap()
            );
            return;
        }
        if profile.join("fixture.overflow").exists() {
            println!("{}", "x".repeat(65537));
            return;
        }
        if action != "status" {
            write(&profile.join("fixture.last-action"), action);
        }
        match action {
            "status" => {}
            "install" => {
                write(&profile.join("service.installed"), b"");
                write(&profile.join("node/running"), b"");
            }
            "update" => {
                write(&profile.join("fixture.availability"), b"current");
                let _ = fs::remove_file(profile.join("fixture.recovery"));
            }
            "start" => write(&profile.join("node/running"), b""),
            "stop" => {
                let _ = fs::remove_file(profile.join("node/running"));
            }
            "uninstall" => {
                let _ = fs::remove_file(profile.join("service.installed"));
                let _ = fs::remove_file(profile.join("node/running"));
            }
            _ => panic!("unexpected CLI action"),
        }
        if action == "update" && profile.join("fixture.timeout-after-mutation").exists() {
            // Publish the PID only after the real mutation above has committed. Keep both
            // output pipes open so the production deadline, kill and wait paths must run.
            let pending = profile.join("fixture.mutation-written.pending");
            write(&pending, std::process::id().to_string());
            fs::rename(&pending, profile.join("fixture.mutation-written")).unwrap();
            loop {
                thread::park();
            }
        }
        if action != "status" && profile.join("fixture.nonzero-mutation").exists() {
            std::process::exit(1);
        }
        if action != "status" && profile.join("fixture.mutation-status").exists() {
            println!(
                "{}",
                fs::read_to_string(profile.join("fixture.mutation-status")).unwrap()
            );
            return;
        }
        if action != "status" && profile.join("fixture.bad-mutation").exists() {
            println!("{{broken");
            return;
        }
        println!("{}", status(&profile));
    } else {
        let node = option(&args, "--data-dir").unwrap();
        assert!(args.iter().any(|arg| arg == "--desktop-parent-stdin"));
        write(&node.join("running"), b"");
        if node.join("fixture.ignore-stdin").exists() {
            loop {
                thread::sleep(Duration::from_millis(25));
            }
        }
        let mut buffer = [0; 1];
        assert_eq!(io::stdin().read(&mut buffer).unwrap(), 0);
        // Publish complete EOF evidence before observers can see running disappear.
        // Creating the final file directly would expose an empty file before write finishes.
        let pending = node.join("fixture.exited.pending");
        write(&pending, b"eof");
        fs::rename(&pending, node.join("fixture.exited"))
            .expect("publish complete synthetic EOF evidence");
        fs::remove_file(node.join("running"))
            .expect("clear synthetic running state after EOF publication");
    }
}
