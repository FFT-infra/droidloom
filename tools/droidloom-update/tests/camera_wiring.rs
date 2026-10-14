//! Follow the complete camera configuration path from the repo's units, cell
//! specifications, init fragments and host assembly recipe to the endpoint the
//! Android producer actually dials. Each link is derived from the artifact that
//! owns it instead of a keyword mirror, so a dropped projection, socket bind or
//! producer source is reported as a broken closure.
use std::{collections::BTreeSet, fs, path::Path, path::PathBuf};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> String {
    let path = repo().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// Every `Key=value` line for `key` in a systemd unit, across all sections.
fn unit_values(unit: &str, key: &str) -> Vec<String> {
    unit.lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(name, _)| *name == key)
        .map(|(_, value)| value.trim().to_string())
        .collect()
}

/// The whitespace-separated items of every directive for `key`.
fn unit_items(unit: &str, key: &str) -> BTreeSet<String> {
    unit_values(unit, key)
        .into_iter()
        .flat_map(|value| {
            value
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The literal exec path of one init service in an `.rc` fragment.
fn service_exec(fragment: &str, service: &str) -> String {
    fragment
        .lines()
        .find_map(|line| {
            let mut words = line.split_whitespace();
            match (words.next(), words.next()) {
                (Some("service"), Some(name)) if name == service => words.next(),
                _ => None,
            }
        })
        .unwrap_or_else(|| panic!("no service {service} in fragment"))
        .to_string()
}

/// A C string literal's value, e.g. `kEndpoint[] = "/dev/socket/..."`.
fn c_string_literal(source: &str, needle: &str) -> String {
    source
        .lines()
        .find(|line| line.contains(needle))
        .and_then(|line| line.split('"').nth(1))
        .unwrap_or_else(|| panic!("no string literal for {needle}"))
        .to_string()
}

#[test]
fn camera_units_start_the_endpoint_before_the_cell_binds_it() {
    let socket = read("packaging/systemd/user/droidloom-camera.socket");
    let listens: BTreeSet<String> = unit_items(&socket, "ListenStream");
    assert_eq!(
        listens,
        BTreeSet::from(["%t/droidloom/camera.sock".to_string()]),
        "the socket unit must own the host camera endpoint"
    );

    // The cell bind-mounts the listener inode, so the socket unit has to be
    // pulled in and started before droidloom.service runs.
    for unit_path in [
        "packaging/systemd/user/droidloom.service",
        "packaging/arch/droidloom.service",
    ] {
        let unit = read(unit_path);
        for key in ["Wants", "After"] {
            let items = unit_items(&unit, key);
            assert!(
                items.contains("droidloom-camera.socket"),
                "{unit_path} {key} must include droidloom-camera.socket, found {items:?}"
            );
        }
    }
}

#[test]
fn cell_specs_project_the_camera_runtime_directory() {
    for spec_path in [
        "packaging/cell-spec-arm64-u1001.json",
        "packaging/cell-spec-x86_64-u1000.json",
    ] {
        let spec: serde_json::Value = serde_json::from_str(&read(spec_path)).unwrap();
        let directories = spec["android_runtime_directories"]
            .as_array()
            .unwrap_or_else(|| panic!("{spec_path} lacks android_runtime_directories"));
        let projected = directories.iter().any(|entry| {
            entry["source"] == "/usr/lib/droidloom/current/camera"
                && entry["target"] == "/droidloom/camera"
        });
        assert!(
            projected,
            "{spec_path} must project the camera runtime directory to /droidloom/camera"
        );
    }
}

#[test]
fn supervisor_binds_the_camera_socket_to_its_guest_endpoint() {
    let source = read("runtime/droidloom-supervisor/src/development.rs");

    // The producer dials this fixed guest endpoint; the supervisor must publish
    // the host listener there and nowhere else.
    let endpoint = c_string_literal(&read("android/hardware/camera/producer.cpp"), "kEndpoint[]");
    assert_eq!(endpoint, "/dev/socket/droidloom/camera");
    let relative = endpoint
        .strip_prefix("/dev/socket/droidloom/")
        .expect("guest endpoint lives below /dev/socket/droidloom");
    // `root.join` takes the guest path without its leading slash, or it would
    // replace the whole cell root instead of appending below it.
    let joined = endpoint.trim_start_matches('/');
    assert!(
        source.contains(&format!("\"{relative}\" => root.join(\"{joined}\")")),
        "supervisor must bind {relative}.sock to {endpoint}"
    );

    // The camera socket is part of the same broker set as audio, while the
    // framework-only channels stay below the presenter's private directory.
    let socket_names = source
        .lines()
        .map(str::trim)
        .find_map(|line| {
            line.strip_prefix("for name in [")
                .filter(|rest| rest.contains("clipboard"))
                .map(|rest| rest.split(']').next().unwrap())
        })
        .expect("supervisor enumerates desktop integration sockets");
    for name in ["clipboard", "notifications", "audio", "camera"] {
        assert!(
            socket_names.contains(&format!("\"{name}\"")),
            "{name} must remain in the integration socket set: {socket_names}"
        );
    }
    assert!(
        source.contains("_ => framework_host.join(name)"),
        "clipboard and notifications must stay in the framework-only host channel"
    );
    for leaked in ["\"clipboard\" =>", "\"notifications\" =>"] {
        assert!(
            !source.contains(leaked),
            "clipboard/notifications must not be remapped to a guest endpoint"
        );
    }
}

#[test]
fn camera_producer_closure_matches_the_vendor_partition_contract() {
    let producer = read("android/hardware/camera/producer.rc");
    let exec = service_exec(&producer, "droidloom-camera-producer");
    assert_eq!(
        exec, "/vendor/bin/droidloom-camera-producer",
        "init starts the producer from its vendor-partition path"
    );
    let vendor_relative = exec.strip_prefix('/').unwrap();

    // The producer declares `vendor: true`, so Soong emits it below vendor/.
    // Assemble must stage it from that exact source, and the bundle must carry
    // the staged copy for the target architecture check.
    let android_bp = read("android/hardware/camera/Android.bp");
    assert!(
        android_bp.contains("vendor: true"),
        "the producer module is a vendor-partition binary"
    );
    let assemble = read("tools/droidloom-update/src/assemble.rs");
    assert!(
        assemble.contains(&format!("\"{vendor_relative}\"")),
        "assemble must copy the producer from {vendor_relative}"
    );
    assert!(
        !assemble.contains("\"system/bin/droidloom-camera-producer\""),
        "assemble must not source the vendor producer from system/bin"
    );
    let bundle = read("tools/droidloom-update/src/bundle.rs");
    assert!(
        bundle.contains("usr/lib/droidloom/runtime/camera/bin/droidloom-camera-producer"),
        "the bundle contract must require the staged producer artifact"
    );
}

#[test]
fn camera_init_import_resolves_through_the_projected_directory() {
    // init imports the fragment from the guest projection, not from a
    // platform partition.
    let import = read("tools/droidloom-update/src/android.rs");
    let imported = import
        .lines()
        .find_map(|line| line.split_once("import /droidloom/").map(|(_, rest)| rest))
        .and_then(|rest| rest.split("\\n").next())
        .map(|rest| format!("/droidloom/{rest}"))
        .expect("android.rs imports the camera fragment");
    assert_eq!(imported, "/droidloom/camera/producer.rc");

    // The cell spec maps the projection root to the package-owned host tree,
    // so the fragment's host source is the runtime `camera/` directory.
    let guest_root = Path::new(&imported).parent().unwrap();
    assert_eq!(guest_root, Path::new("/droidloom/camera"));
    let fragment = guest_root
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(fragment, "camera");
    let file = Path::new(&imported)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    // Assemble stages that exact fragment into the runtime camera directory the
    // specification projects.
    let assemble = read("tools/droidloom-update/src/assemble.rs");
    assert!(
        assemble.contains(&format!(
            "(\"android/hardware/camera/{file}\", \"camera/{file}\")"
        )),
        "assemble must stage android/hardware/camera/{file} into camera/{file}"
    );
}
