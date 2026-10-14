use std::path::Path;

/// Apply span replacements to one file at once, then reformat with
/// nixfmt. Spans come from the static scanner, so they index the original file
/// text; they are applied from the end backwards so earlier offsets stay valid.
pub fn set_values_in_file(file: &str, edits: &[(usize, usize, String)]) -> Result<(), String> {
    let mut text = std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))?;
    let mut ordered: Vec<&(usize, usize, String)> = edits.iter().collect();
    ordered.sort_by(|a, b| b.0.cmp(&a.0));
    for (byte_start, byte_end, value) in ordered {
        if byte_start > byte_end || *byte_end > text.len() {
            return Err(format!("span {byte_start}..{byte_end} out of range for {file}"));
        }
        text.replace_range(*byte_start..*byte_end, value);
    }
    let formatted = nixfmt(&text).unwrap_or(text);
    write_atomic(file, &formatted)
}

fn nixfmt(input: &str) -> Option<String> {
    let mut child = std::process::Command::new("nixfmt")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    use std::io::Write;
    child.stdin.as_mut()?.write_all(input.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if out.status.success() {
        String::from_utf8(out.stdout).ok()
    } else {
        None
    }
}

fn write_atomic(file: &str, contents: &str) -> Result<(), String> {
    let path = Path::new(file);
    let tmp = path.with_extension("nix-regedit.tmp");
    std::fs::write(&tmp, contents).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {file}: {e}"))?;
    Ok(())
}

/// Whether `file` lives inside the flake root and is safe to edit.
pub fn editable(file: &str, root: &str) -> bool {
    !file.starts_with("/nix/store") && Path::new(file).starts_with(root)
}