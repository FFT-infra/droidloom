// Compile the cleanup and completion methods from the supplied libcamera source,
// keeping this regression independent of camera hardware and build dependencies.
use std::{env, error::Error, fs, path::PathBuf, process::Command};

fn declaration<'a>(source: &'a str, name: &str) -> Result<&'a str, Box<dyn Error>> {
    let start = source.find(name).ok_or_else(|| format!("missing {name}"))?;
    let end = source[start..]
        .find("\n};")
        .ok_or("unterminated declaration")?;
    Ok(&source[start..start + end + 3])
}

fn function<'a>(source: &'a str, name: &str) -> Result<&'a str, Box<dyn Error>> {
    let start = source.find(name).ok_or_else(|| format!("missing {name}"))?;
    let end = source[start..].find("\n}").ok_or("unterminated function")?;
    Ok(&source[start..start + end + 2])
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err("usage: stop-regression SIMPLE_CPP NEW_OUTPUT_DIRECTORY".into());
    }
    let source = fs::read_to_string(&args[0])?;
    let output = PathBuf::from(&args[1]);
    fs::create_dir(&output)?;
    let declarations = [
        declaration(&source, "struct SimpleFrameInfo {")?,
        declaration(&source, "class SimpleFrames\n{")?,
    ]
    .join("\n\n");
    let methods = [
        "void SimpleFrames::create(",
        "void SimpleFrames::destroy(",
        "void SimpleFrames::clear(",
        "SimpleFrameInfo *SimpleFrames::find(",
        "void SimpleCameraData::clearIncompleteRequests(",
        "void SimpleCameraData::tryCompleteRequest(",
        "void SimpleCameraData::conversionOutputDone(",
        "void SimpleCameraData::metadataReady(",
        "void SimplePipelineHandler::stopDevice(",
    ]
    .iter()
    .map(|name| function(&source, name))
    .collect::<Result<Vec<_>, _>>()?
    .join("\n\n");
    let test = include_str!("stop-regression.cc.in")
        .replace("@DECLARATIONS@", &declarations)
        .replace("@METHODS@", &methods);
    let cpp = output.join("stop-regression.cpp");
    let binary = output.join("stop-regression");
    fs::write(&cpp, test)?;
    let compiler = env::var_os("CXX").unwrap_or_else(|| "c++".into());
    let status = Command::new(compiler)
        .args(["-std=c++17", "-Wall", "-Wextra", "-Werror", "-pedantic"])
        .arg(&cpp)
        .arg("-o")
        .arg(&binary)
        .status()?;
    if !status.success() {
        return Err("regression compilation failed".into());
    }
    let status = Command::new(&binary).status()?;
    if !status.success() {
        return Err("pending-request cleanup regression failed".into());
    }
    Ok(())
}
