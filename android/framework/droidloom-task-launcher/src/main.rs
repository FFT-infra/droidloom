//! Android vendor executable for one two-phase Droidloom task launch.

#![forbid(unsafe_code)]

use std::env;

use droidloom_task_launcher::{LaunchRequest, launch, set_display_density};

#[derive(Debug, Eq, PartialEq)]
enum Operation {
    Launch(LaunchRequest),
    SetDensity(u32),
}

fn main() {
    let result = arguments().and_then(|operation| match operation {
        Operation::Launch(request) => launch(&request)
            .map(|binding| {
                format!(
                    "object={} composer_display={} android_display={} task={}",
                    binding.object, binding.composer_display, binding.android_display, binding.task
                )
            })
            .map_err(|error| error.to_string()),
        Operation::SetDensity(dpi) => set_display_density(dpi)
            .map(|()| format!("dpi={dpi}"))
            .map_err(|error| error.to_string()),
    });
    match result {
        Ok(message) => println!("{message}"),
        Err(error) => {
            eprintln!("droidloom-task-launcher: {error}");
            std::process::exit(1);
        }
    }
}

fn arguments() -> Result<Operation, String> {
    arguments_from(env::args().skip(1))
}

fn arguments_from(arguments: impl IntoIterator<Item = String>) -> Result<Operation, String> {
    let mut package = None;
    let mut component = None;
    let mut user = 0;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--set-density" if package.is_none() && component.is_none() && user == 0 => {
                let dpi = arguments
                    .next()
                    .ok_or_else(|| "--set-density requires a DPI value".to_owned())?
                    .parse()
                    .map_err(|_| "--set-density must be an unsigned integer".to_owned())?;
                if arguments.next().is_some() {
                    return Err("--set-density accepts exactly one DPI value".to_owned());
                }
                return Ok(Operation::SetDensity(dpi));
            }
            "--component" => {
                component = Some(
                    arguments
                        .next()
                        .ok_or_else(|| "--component requires PACKAGE/ACTIVITY".to_owned())?,
                );
            }
            "--user" => {
                user = arguments
                    .next()
                    .ok_or_else(|| "--user requires an Android user ID".to_owned())?
                    .parse()
                    .map_err(|_| "--user must be an unsigned integer".to_owned())?;
            }
            option if option.starts_with('-') => {
                return Err(format!("unknown option {option}"));
            }
            value if package.is_none() => package = Some(value.to_owned()),
            _ => return Err("only one Android package may be launched".to_owned()),
        }
    }
    Ok(Operation::Launch(LaunchRequest {
        package: package.ok_or_else(|| {
            "usage: droidloom-task-launcher [--user ID] [--component PACKAGE/ACTIVITY] PACKAGE | --set-density DPI"
                .to_owned()
        })?,
        component,
        user,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Result<Operation, String> {
        arguments_from(values.iter().map(|value| (*value).to_owned()))
    }

    #[test]
    fn parses_persistent_density_operation() {
        assert_eq!(
            arguments(&["--set-density", "175"]),
            Ok(Operation::SetDensity(175))
        );
        assert!(arguments(&["--set-density"]).is_err());
        assert!(arguments(&["--set-density", "175", "extra"]).is_err());
    }

    #[test]
    fn preserves_launch_operation() {
        assert_eq!(
            arguments(&[
                "--user",
                "0",
                "--component",
                "com.android.settings/com.android.settings.Settings",
                "com.android.settings"
            ]),
            Ok(Operation::Launch(LaunchRequest {
                package: "com.android.settings".to_owned(),
                component: Some("com.android.settings/com.android.settings.Settings".to_owned()),
                user: 0,
            }))
        );
    }
}
