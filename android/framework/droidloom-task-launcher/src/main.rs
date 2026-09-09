//! Android vendor executable for one two-phase Droidloom task launch.

#![forbid(unsafe_code)]

use std::env;

use droidloom_task_launcher::{LaunchRequest, bind_existing_task, launch, set_display_density};

#[derive(Debug, Eq, PartialEq)]
enum Operation {
    Launch(LaunchRequest),
    SetDensity(u32),
    BindTask {
        package: String,
        user: u32,
        task: u64,
    },
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
        Operation::BindTask {
            package,
            user,
            task,
        } => bind_existing_task(&package, user, task)
            .map(|binding| format!("object={} task={}", binding.object, binding.task))
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
    let mut task = None;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--set-density"
                if package.is_none() && component.is_none() && user == 0 && task.is_none() =>
            {
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
            "--bind-task" if task.is_none() => {
                task = Some(
                    arguments
                        .next()
                        .ok_or("--bind-task requires a task ID")?
                        .parse::<u64>()
                        .map_err(|_| "--bind-task must be an unsigned integer")?,
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
    let package = package.ok_or_else(|| {
            "usage: droidloom-task-launcher [--user ID] [--component PACKAGE/ACTIVITY | --bind-task ID] PACKAGE | --set-density DPI"
                .to_owned()
        })?;
    if let Some(task) = task {
        if component.is_some() {
            return Err("--bind-task cannot start a component".into());
        }
        return Ok(Operation::BindTask {
            package,
            user,
            task,
        });
    }
    Ok(Operation::Launch(LaunchRequest {
        package,
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
    fn binding_an_existing_task_never_becomes_a_launch() {
        assert_eq!(
            arguments(&[
                "--bind-task",
                "26",
                "--user",
                "0",
                "com.zhiliaoapp.musically"
            ]),
            Ok(Operation::BindTask {
                package: "com.zhiliaoapp.musically".into(),
                user: 0,
                task: 26
            })
        );
        for args in [
            vec!["--bind-task"],
            vec!["--bind-task", "x", "org.example.app"],
            vec![
                "--bind-task",
                "26",
                "--component",
                "org.example.app/.Main",
                "org.example.app",
            ],
            vec!["--bind-task", "26", "--set-density", "175"],
        ] {
            assert!(arguments(&args).is_err(), "{args:?}");
        }
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
