//! Bridge to JPEXS Free Flash Decompiler (FFDec).
//!
//! FFDec is only ever run as a separate process through its command line
//! interface; nothing is linked in.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;
use url::Url;

/// Don't hang forever on a pathological class.
const EXPORT_TIMEOUT_SECS: &str = "120";

/// Decompiles one AS3 class and returns its source.
///
/// Exports are cached under the temp directory, keyed by SWF path and
/// modification time, so opening the same class again is instant.
pub fn export_class_source(
    ffdec_path: &str,
    swf_url: &str,
    class_name: &str,
) -> Result<String, String> {
    let ffdec = ffdec_executable(ffdec_path)?;
    let swf = swf_path(swf_url)?;
    if class_name.is_empty() || class_name.contains([',', '+', '"']) {
        return Err(format!("Invalid class name: {class_name}"));
    }

    let out_dir = cache_dir(&swf)?.join(sanitize(class_name));
    let expected = expected_script_path(&out_dir, class_name);
    if let Ok(source) = std::fs::read_to_string(&expected) {
        return Ok(source);
    }

    let output = ffdec_command(&ffdec)
        .arg("-exportTimeout")
        .arg(EXPORT_TIMEOUT_SECS)
        .arg("-selectclass")
        .arg(class_name)
        .arg("-export")
        .arg("script")
        .arg(&out_dir)
        .arg(&swf)
        .output()
        .map_err(|e| format!("Couldn't run FFDec ({}): {e}", ffdec.display()))?;

    if let Ok(source) = std::fs::read_to_string(&expected) {
        return Ok(source);
    }
    // FFDec may place the file elsewhere (e.g. renamed identifiers).
    let file_name = format!("{}.as", class_name.rsplit('.').next().unwrap_or(class_name));
    if let Some(found) = find_file(&out_dir, &file_name) {
        return std::fs::read_to_string(&found).map_err(|e| e.to_string());
    }

    let log = strip_ansi(&String::from_utf8_lossy(&output.stdout))
        + &strip_ansi(&String::from_utf8_lossy(&output.stderr));
    let tail: Vec<&str> = log.lines().rev().take(8).collect();
    Err(format!(
        "FFDec didn't export {class_name} (exit status {}).\n{}",
        output.status,
        tail.into_iter().rev().collect::<Vec<_>>().join("\n")
    ))
}

/// Opens the SWF in the FFDec GUI (which lives next to the CLI).
pub fn open_gui(ffdec_path: &str, swf_url: &str) -> Result<(), String> {
    let cli = ffdec_executable(ffdec_path)?;
    let swf = swf_path(swf_url)?;
    let dir = cli.parent().unwrap_or(Path::new("."));
    let gui = ["ffdec.exe", "ffdec.sh", "ffdec.bat", "ffdec.jar"]
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
        .unwrap_or(cli);
    let mut command = if is_jar(&gui) {
        let mut command = Command::new("java");
        command.arg("-jar").arg(&gui);
        command
    } else {
        Command::new(&gui)
    };
    command
        .arg(&swf)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Couldn't start {}: {e}", gui.display()))
}

fn ffdec_executable(ffdec_path: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(ffdec_path.trim().trim_matches('"'));
    if ffdec_path.trim().is_empty() {
        return Err(
            "Set the FFDec path in Modkit settings (ffdec-cli.exe, ffdec.sh or ffdec.jar)."
                .to_string(),
        );
    }
    if path.is_dir() {
        return ["ffdec-cli.exe", "ffdec.sh", "ffdec.bat", "ffdec.jar"]
            .iter()
            .map(|name| path.join(name))
            .find(|path| path.is_file())
            .ok_or_else(|| format!("No FFDec executable found in {}", path.display()));
    }
    if !path.is_file() {
        return Err(format!("FFDec not found at {}", path.display()));
    }
    Ok(path)
}

fn ffdec_command(ffdec: &Path) -> Command {
    let mut command = if is_jar(ffdec) {
        let mut command = Command::new("java");
        command.arg("-jar").arg(ffdec);
        command
    } else {
        Command::new(ffdec)
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

fn is_jar(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
}

/// The file a movie URL points to, if the movie was loaded from disk.
pub(super) fn swf_path(swf_url: &str) -> Result<PathBuf, String> {
    let url = Url::parse(swf_url).map_err(|e| format!("Bad movie URL {swf_url}: {e}"))?;
    if url.scheme() != "file" {
        return Err(
            "this movie was loaded from the network, not from a local SWF file".to_string(),
        );
    }
    url.to_file_path()
        .map_err(|_| format!("Bad file URL: {swf_url}"))
}

fn cache_dir(swf: &Path) -> Result<PathBuf, String> {
    let modified = std::fs::metadata(swf)
        .and_then(|m| m.modified())
        .map_err(|e| format!("Can't read {}: {e}", swf.display()))?;
    let mut hasher = DefaultHasher::new();
    swf.hash(&mut hasher);
    modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);
    Ok(std::env::temp_dir()
        .join("ruffle-modkit-ffdec")
        .join(format!("{:016x}", hasher.finish())))
}

/// FFDec writes `com.foo.Bar` to `<out>/scripts/com/foo/Bar.as`.
fn expected_script_path(out_dir: &Path, class_name: &str) -> PathBuf {
    let mut path = out_dir.join("scripts");
    for part in class_name.split('.') {
        path.push(part);
    }
    path.set_extension("as");
    path
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn find_file(dir: &Path, file_name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, file_name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n == file_name) {
            return Some(path);
        }
    }
    None
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip `ESC [ ... <letter>`.
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_path_follows_package() {
        let out = Path::new("out");
        assert_eq!(
            expected_script_path(out, "com.foo.Bar"),
            Path::new("out")
                .join("scripts")
                .join("com")
                .join("foo")
                .join("Bar.as")
        );
        assert_eq!(
            expected_script_path(out, "Stats"),
            Path::new("out").join("scripts").join("Stats.as")
        );
    }

    #[test]
    fn rejects_non_file_urls() {
        assert!(swf_path("https://example.com/game.swf").is_err());
        assert!(swf_path("not a url").is_err());
    }

    #[test]
    fn strips_ansi_codes() {
        assert_eq!(strip_ansi("\u{1b}[1;4mUsage\u{1b}[m: x"), "Usage: x");
    }

    #[test]
    fn missing_ffdec_path_is_reported() {
        let err = export_class_source("", "file:///x.swf", "Stats").unwrap_err();
        assert!(err.contains("FFDec path"));
    }
}
