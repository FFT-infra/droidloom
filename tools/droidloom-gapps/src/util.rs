use std::{
    fs,
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Bound subprocess output and always reap it, including decompression failures.
pub fn capture(command: &mut Command, maximum: u64) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    stream(command, &mut output, maximum)?;
    Ok(output)
}

pub fn stream(command: &mut Command, destination: &mut impl Write, maximum: u64) -> Result<u64> {
    let stderr = tempfile::tempfile()?;
    let mut child = command
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(stderr.try_clone()?)
        .spawn()?;
    let result = std::io::copy(
        &mut child
            .stdout
            .take()
            .ok_or("missing subprocess output")?
            .take(maximum + 1),
        destination,
    );
    if !result.as_ref().is_ok_and(|count| *count <= maximum) {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let count = result?;
    if count > maximum {
        return Err(format!("output exceeds {maximum} bytes: {command:?}").into());
    }
    if !status.success() {
        use std::io::{Seek, SeekFrom};
        let mut stderr = stderr;
        stderr.seek(SeekFrom::Start(0))?;
        let mut error = String::new();
        stderr.take(8192).read_to_string(&mut error)?;
        return Err(format!("{command:?} failed ({status}): {error}").into());
    }
    Ok(count)
}

pub fn text(command: &mut Command) -> Result<String> {
    Ok(String::from_utf8(capture(command, 4 * 1024 * 1024)?)?)
}

pub fn run(command: &mut Command) -> Result<()> {
    let result = text(command)?;
    if !result.trim().is_empty() {
        eprintln!("{}", result.trim());
    }
    Ok(())
}

pub fn write(path: &Path, bytes: impl AsRef<[u8]>) -> Result<()> {
    fs::create_dir_all(path.parent().ok_or("path has no parent")?)?;
    fs::write(path, bytes)?;
    Ok(())
}

pub fn normal_relative(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.starts_with('/')
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/+@".contains(&b))
        && path
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
}

pub fn new_output(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok() {
        return Err(format!("output already exists: {}", path.display()).into());
    }
    fs::create_dir_all(path.parent().ok_or("output has no parent")?)?;
    Ok(())
}
