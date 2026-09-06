//! Bounded network plumbing for the executable development cell.
//!
//! Linux remains the network implementation. This module only creates a veth
//! pair, assigns the host gateway, and installs narrowly named nftables rules
//! for outbound forwarding/NAT. Android owns `eth0` and its normal networking
//! services inside the private namespace.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::CellSpec;
use crate::development::DevelopmentError;

const HOST_IPV4_CIDR: &str = "10.177.0.1/30";
const CELL_IPV4_CIDR: &str = "10.177.0.2/32";
const IPV4_FORWARD_PATH: &str = "/proc/sys/net/ipv4/ip_forward";
const IPV4_FORWARD_STATE_FILE: &str = "host-ipv4-forward.before";

#[derive(Clone, Debug, Eq, PartialEq)]
struct NetworkNames {
    namespace: String,
    host_interface: String,
    temporary_cell_interface: String,
    filter_table: String,
    nat_table: String,
}

impl NetworkNames {
    fn for_spec(spec: &CellSpec) -> Self {
        let uid = spec.host_uid;
        Self {
            namespace: format!("droidloom-u{uid}"),
            host_interface: format!("dlh{uid}"),
            temporary_cell_interface: format!("dlc{uid}"),
            filter_table: format!("droidloom_u{uid}"),
            nat_table: format!("droidloom_u{uid}_nat"),
        }
    }
}

/// Live host-owned networking objects for one development cell.
pub(crate) struct DevelopmentNetwork {
    names: NetworkNames,
    runtime_dir: PathBuf,
    resources: Vec<NetworkResource>,
    previous_ipv4_forward: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NetworkResource {
    Namespace,
    HostInterface,
    FilterTable,
    NatTable,
    UfwForwardRule,
}

impl DevelopmentNetwork {
    /// Create the private namespace and its single outbound kernel path.
    pub(crate) fn create(spec: &CellSpec) -> Result<Self, DevelopmentError> {
        let mut network = Self {
            names: NetworkNames::for_spec(spec),
            runtime_dir: spec.runtime_dir.clone(),
            resources: Vec::new(),
            previous_ipv4_forward: None,
        };

        if let Err(error) = network.setup() {
            if let Err(cleanup_error) = network.teardown() {
                eprintln!(
                    "droidloom-supervisor: network setup failed: {error}; cleanup also failed: {cleanup_error}"
                );
            }
            return Err(error);
        }
        Ok(network)
    }

    pub(crate) fn namespace(&self) -> &str {
        &self.names.namespace
    }

    fn setup(&mut self) -> Result<(), DevelopmentError> {
        self.setup_links()?;
        self.enable_forwarding()?;
        self.setup_filter()?;
        self.setup_nat()?;
        self.setup_ufw_forwarding()
    }

    fn setup_ufw_forwarding(&mut self) -> Result<(), DevelopmentError> {
        // A separate nftables accept verdict cannot override UFW's later drop.
        // Add only our exact outbound source/interface rule to its user chain.
        // It is transient and never modifies UFW's persistent configuration.
        if ufw_chain_exists()? {
            run("iptables", ufw_rule_arguments("-I", &self.names))?;
            self.resources.push(NetworkResource::UfwForwardRule);
        }
        Ok(())
    }

    fn setup_links(&mut self) -> Result<(), DevelopmentError> {
        run("ip", ["netns", "add", &self.names.namespace])?;
        self.resources.push(NetworkResource::Namespace);

        run(
            "ip",
            [
                "link",
                "add",
                &self.names.host_interface,
                "type",
                "veth",
                "peer",
                "name",
                &self.names.temporary_cell_interface,
            ],
        )?;
        self.resources.push(NetworkResource::HostInterface);
        run(
            "ip",
            [
                "link",
                "set",
                &self.names.temporary_cell_interface,
                "netns",
                &self.names.namespace,
            ],
        )?;
        run(
            "ip",
            [
                "address",
                "add",
                HOST_IPV4_CIDR,
                "dev",
                &self.names.host_interface,
            ],
        )?;
        run("ip", ["link", "set", &self.names.host_interface, "up"])?;
        run_in_namespace(&self.names.namespace, ["link", "set", "lo", "up"])?;
        run_in_namespace(
            &self.names.namespace,
            [
                "link",
                "set",
                &self.names.temporary_cell_interface,
                "name",
                "eth0",
            ],
        )?;
        run_in_namespace(&self.names.namespace, ["link", "set", "eth0", "up"])?;
        Ok(())
    }

    fn enable_forwarding(&mut self) -> Result<(), DevelopmentError> {
        let forwarding = fs::read_to_string(IPV4_FORWARD_PATH)
            .map_err(|source| io_error("read host IPv4 forwarding state", source))?;
        fs::write(self.forwarding_state_path(), forwarding.as_bytes())
            .map_err(|source| io_error("record host IPv4 forwarding state", source))?;
        self.previous_ipv4_forward = Some(forwarding.clone());
        if forwarding.trim() != "1" {
            fs::write(IPV4_FORWARD_PATH, "1\n")
                .map_err(|source| io_error("enable host IPv4 forwarding", source))?;
        }
        Ok(())
    }

    fn setup_filter(&mut self) -> Result<(), DevelopmentError> {
        run("nft", ["add", "table", "inet", &self.names.filter_table])?;
        self.resources.push(NetworkResource::FilterTable);
        self.setup_input_filter()?;
        self.setup_forward_filter()
    }

    fn setup_input_filter(&self) -> Result<(), DevelopmentError> {
        run(
            "nft",
            [
                "add",
                "chain",
                "inet",
                &self.names.filter_table,
                "input",
                "{",
                "type",
                "filter",
                "hook",
                "input",
                "priority",
                "filter",
                ";",
                "policy",
                "accept",
                ";",
                "}",
            ],
        )?;
        run(
            "nft",
            [
                "add",
                "rule",
                "inet",
                &self.names.filter_table,
                "input",
                "iifname",
                &self.names.host_interface,
                "drop",
            ],
        )
    }

    fn setup_forward_filter(&self) -> Result<(), DevelopmentError> {
        run(
            "nft",
            [
                "add",
                "chain",
                "inet",
                &self.names.filter_table,
                "forward",
                "{",
                "type",
                "filter",
                "hook",
                "forward",
                "priority",
                "filter",
                ";",
                "policy",
                "accept",
                ";",
                "}",
            ],
        )?;
        run(
            "nft",
            [
                "add",
                "rule",
                "inet",
                &self.names.filter_table,
                "forward",
                "iifname",
                &self.names.host_interface,
                "ip",
                "saddr",
                "!=",
                CELL_IPV4_CIDR,
                "drop",
            ],
        )?;
        run(
            "nft",
            [
                "add",
                "rule",
                "inet",
                &self.names.filter_table,
                "forward",
                "iifname",
                &self.names.host_interface,
                "ip",
                "saddr",
                CELL_IPV4_CIDR,
                "accept",
            ],
        )?;
        run(
            "nft",
            [
                "add",
                "rule",
                "inet",
                &self.names.filter_table,
                "forward",
                "oifname",
                &self.names.host_interface,
                "ip",
                "daddr",
                CELL_IPV4_CIDR,
                "ct",
                "state",
                "established,related",
                "accept",
            ],
        )?;
        run(
            "nft",
            [
                "add",
                "rule",
                "inet",
                &self.names.filter_table,
                "forward",
                "oifname",
                &self.names.host_interface,
                "drop",
            ],
        )
    }

    fn setup_nat(&mut self) -> Result<(), DevelopmentError> {
        run("nft", ["add", "table", "ip", &self.names.nat_table])?;
        self.resources.push(NetworkResource::NatTable);
        run(
            "nft",
            [
                "add",
                "chain",
                "ip",
                &self.names.nat_table,
                "postrouting",
                "{",
                "type",
                "nat",
                "hook",
                "postrouting",
                "priority",
                "srcnat",
                ";",
                "policy",
                "accept",
                ";",
                "}",
            ],
        )?;
        run(
            "nft",
            [
                "add",
                "rule",
                "ip",
                &self.names.nat_table,
                "postrouting",
                "ip",
                "saddr",
                CELL_IPV4_CIDR,
                "masquerade",
            ],
        )
    }

    /// Remove every object created by this instance in reverse ownership order.
    pub(crate) fn teardown(&mut self) -> Result<(), DevelopmentError> {
        let mut first_error = None;

        while let Some(resource) = self.resources.pop() {
            let result = match resource {
                NetworkResource::UfwForwardRule => remove_ufw_rule(&self.names),
                NetworkResource::Namespace => run("ip", ["netns", "delete", &self.names.namespace]),
                NetworkResource::HostInterface => {
                    run("ip", ["link", "delete", &self.names.host_interface])
                }
                NetworkResource::FilterTable => {
                    run("nft", ["delete", "table", "inet", &self.names.filter_table])
                }
                NetworkResource::NatTable => {
                    run("nft", ["delete", "table", "ip", &self.names.nat_table])
                }
            };
            record_error(&mut first_error, result);
        }
        if let Some(previous) = self.previous_ipv4_forward.take() {
            if previous.trim() != "1" {
                record_error(
                    &mut first_error,
                    fs::write(IPV4_FORWARD_PATH, previous)
                        .map_err(|source| io_error("restore host IPv4 forwarding", source)),
                );
            }
            record_error(
                &mut first_error,
                remove_file_if_present(
                    &self.forwarding_state_path(),
                    "remove host IPv4 forwarding state",
                ),
            );
        }

        first_error.map_or(Ok(()), Err)
    }

    /// Remove exact, deterministically named objects left by an interrupted
    /// daemon. This never enumerates or deletes unrelated host networking.
    pub(crate) fn recover(spec: &CellSpec) -> Result<(), DevelopmentError> {
        let names = NetworkNames::for_spec(spec);
        let mut first_error = None;

        record_error(&mut first_error, remove_ufw_rule(&names));
        if nft_table_exists("ip", &names.nat_table)? {
            record_error(
                &mut first_error,
                run("nft", ["delete", "table", "ip", &names.nat_table]),
            );
        }
        if nft_table_exists("inet", &names.filter_table)? {
            record_error(
                &mut first_error,
                run("nft", ["delete", "table", "inet", &names.filter_table]),
            );
        }
        if link_exists(&names.host_interface)? {
            record_error(
                &mut first_error,
                run("ip", ["link", "delete", &names.host_interface]),
            );
        }
        if namespace_exists(&names.namespace)? {
            record_error(
                &mut first_error,
                run("ip", ["netns", "delete", &names.namespace]),
            );
        }

        let forwarding_state = spec.runtime_dir.join(IPV4_FORWARD_STATE_FILE);
        match fs::read_to_string(&forwarding_state) {
            Ok(previous) => {
                record_error(
                    &mut first_error,
                    fs::write(IPV4_FORWARD_PATH, previous).map_err(|source| {
                        io_error("restore stale host IPv4 forwarding state", source)
                    }),
                );
                record_error(
                    &mut first_error,
                    remove_file_if_present(
                        &forwarding_state,
                        "remove stale host IPv4 forwarding state",
                    ),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                record_error(
                    &mut first_error,
                    Err(io_error("read stale host IPv4 forwarding state", source)),
                );
            }
        }

        first_error.map_or(Ok(()), Err)
    }

    fn forwarding_state_path(&self) -> PathBuf {
        // `runtime_dir` has already been created mode 0700 before networking
        // starts, so this recovery record is not exposed to the Android cell.
        self.runtime_dir.join(IPV4_FORWARD_STATE_FILE)
    }
}

fn ufw_rule_arguments<'a>(operation: &'a str, names: &'a NetworkNames) -> [&'a str; 13] {
    [
        "-w",
        operation,
        "ufw-user-forward",
        "-i",
        &names.host_interface,
        "-s",
        CELL_IPV4_CIDR,
        "-m",
        "comment",
        "--comment",
        &names.namespace,
        "-j",
        "ACCEPT",
    ]
}

fn ufw_chain_exists() -> Result<bool, DevelopmentError> {
    match Command::new("iptables")
        .args(["-w", "-S", "ufw-user-forward"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) => Ok(status.success()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("inspect UFW forwarding chain", error)),
    }
}

fn remove_ufw_rule(names: &NetworkNames) -> Result<(), DevelopmentError> {
    if ufw_chain_exists()? && command_succeeds("iptables", ufw_rule_arguments("-C", names))? {
        run("iptables", ufw_rule_arguments("-D", names))?;
    }
    Ok(())
}

fn command_succeeds<const N: usize>(
    program: &str,
    args: [&str; N],
) -> Result<bool, DevelopmentError> {
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .map_err(|source| io_error(&format!("execute {program}"), source))
}

fn namespace_exists(namespace: &str) -> Result<bool, DevelopmentError> {
    let output = Command::new("ip")
        .args(["netns", "list"])
        .output()
        .map_err(|source| io_error("execute ip", source))?;
    if !output.status.success() {
        return Err(DevelopmentError::Command {
            program: "ip".into(),
            status: output.status,
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .any(|name| name == namespace))
}

fn link_exists(interface: &str) -> Result<bool, DevelopmentError> {
    command_succeeds("ip", ["link", "show", "dev", interface])
}

fn nft_table_exists(family: &str, table: &str) -> Result<bool, DevelopmentError> {
    command_succeeds("nft", ["list", "table", family, table])
}

fn remove_file_if_present(path: &Path, context: &str) -> Result<(), DevelopmentError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(context, source)),
    }
}

fn run_in_namespace<const N: usize>(
    namespace: &str,
    ip_arguments: [&str; N],
) -> Result<(), DevelopmentError> {
    let mut arguments = vec![
        OsString::from("netns"),
        OsString::from("exec"),
        OsString::from(namespace),
        OsString::from("ip"),
    ];
    arguments.extend(ip_arguments.into_iter().map(OsString::from));
    run_os("ip", arguments)
}

fn run<const N: usize>(program: &str, args: [&str; N]) -> Result<(), DevelopmentError> {
    run_os(program, args.into_iter().map(OsString::from))
}

fn run_os<I>(program: &str, args: I) -> Result<(), DevelopmentError>
where
    I: IntoIterator<Item = OsString>,
{
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|source| io_error(&format!("execute {program}"), source))?;
    if status.success() {
        Ok(())
    } else {
        Err(DevelopmentError::Command {
            program: program.into(),
            status,
        })
    }
}

fn record_error(slot: &mut Option<DevelopmentError>, result: Result<(), DevelopmentError>) {
    if let Err(error) = result
        && slot.is_none()
    {
        *slot = Some(error);
    }
}

fn io_error(context: &str, source: std::io::Error) -> DevelopmentError {
    DevelopmentError::Io {
        context: context.into(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IdRange;
    use std::path::PathBuf;

    fn spec(host_uid: u32) -> CellSpec {
        CellSpec {
            host_uid,
            subordinate_uids: IdRange {
                start: 100_000,
                count: 100_000,
            },
            subordinate_gids: IdRange {
                start: 200_000,
                count: 100_000,
            },
            image_dir: PathBuf::from("/images"),
            vendor_image: PathBuf::from("/images/vendor.img"),
            android_init: None,
            android_file_overrides: Vec::new(),
            android_runtime_directories: Vec::new(),
            shared_storage_directories: Vec::new(),
            data_dir: PathBuf::from("/data"),
            runtime_dir: PathBuf::from("/run/droidloom/cells/test"),
            render_node: PathBuf::from("/dev/dri/renderD128"),
            denial_socket: PathBuf::from("/run/user/1001/denial.sock"),
        }
    }

    #[test]
    fn network_objects_are_stable_and_interface_names_fit_linux() {
        let names = NetworkNames::for_spec(&spec(1001));
        assert_eq!(names.namespace, "droidloom-u1001");
        assert_eq!(names.host_interface, "dlh1001");
        assert_eq!(names.temporary_cell_interface, "dlc1001");
        assert!(names.host_interface.len() < libc::IF_NAMESIZE);
        assert!(names.temporary_cell_interface.len() < libc::IF_NAMESIZE);
    }

    #[test]
    fn cell_and_gateway_use_the_same_bounded_subnet() {
        assert_eq!(HOST_IPV4_CIDR, "10.177.0.1/30");
        assert_eq!(CELL_IPV4_CIDR, "10.177.0.2/32");
    }
}
