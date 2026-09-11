//! Exercise installer configuration and failure paths without touching the host.
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn installer_is_repeatable_rejects_conflicts_and_propagates_pacman_failure() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let config = root.join("pacman.conf");
    let include = root.join("droidloom.conf");
    fs::write(
        &config,
        "[options]\nArchitecture = auto\n[core]\nServer = https://example.invalid\n",
    )
    .unwrap();
    let script = include_str!("../../../install.sh")
        .replace("/etc/pacman.conf", config.to_str().unwrap())
        .replace("/etc/pacman.d/droidloom.conf", include.to_str().unwrap());
    fs::write(root.join("install.sh"), script).unwrap();
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    executable(&bin.join("uname"), "#!/bin/sh\necho x86_64\n");
    executable(&bin.join("id"), "#!/bin/sh\necho 1000\n");
    executable(&bin.join("sudo"), "#!/bin/sh\nexec \"$@\"\n");
    executable(
        &bin.join("pacman-conf"),
        "#!/bin/sh\nif [ -f \"$FIXTURE/droidloom.conf\" ]; then echo droidloom; fi\n",
    );
    executable(
        &bin.join("pacman"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$FIXTURE/transactions\"\nexit \"${PACMAN_RESULT:-0}\"\n",
    );
    let invoke = |code: &str| {
        Command::new("sh")
            .arg(root.join("install.sh"))
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("FIXTURE", root)
            .env("PACMAN_RESULT", code)
            .output()
            .unwrap()
    };
    assert!(invoke("0").status.success());
    let first = fs::read(&config).unwrap();
    assert!(invoke("0").status.success());
    assert_eq!(fs::read(&config).unwrap(), first);
    assert_eq!(
        fs::read_to_string(root.join("transactions")).unwrap(),
        "-Syu --needed droidloom-runtime droidloom-image\n".repeat(2)
    );
    assert!(!invoke("42").status.success());
    fs::write(&include, "[droidloom]\nServer = https://custom.invalid\n").unwrap();
    let before = fs::read(root.join("transactions")).unwrap();
    let result = invoke("0");
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("existing Droidloom repository differs")
    );
    assert_eq!(fs::read(root.join("transactions")).unwrap(), before);
    assert_eq!(fs::read(&config).unwrap(), first);
}
