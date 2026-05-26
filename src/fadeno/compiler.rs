use base64::Engine;
use std::{path::Path, process::Command};

use crate::fadeno::{deser::deserialize_compile_result, types::Compiled};

pub struct CompileOutput {
    pub type_str: String,
    pub bytecode: Compiled,
}

impl std::fmt::Debug for CompileOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompileOutput")
            .field("type_str", &self.type_str)
            .field("modules_count", &self.bytecode.module_ranges.len())
            .finish()
    }
}

#[derive(Debug)]
pub enum CompileError {
    ProcessFailed { exit_code: i32, stderr: String },
    TypeError { message: String },
    JsonError(String),
    Base64Error(base64::DecodeError),
    DeserializeError(crate::fadeno::deser::DeError),
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::ProcessFailed { exit_code, stderr } => {
                write!(f, "fadeno-lang exited with code {exit_code}: {stderr}")
            }
            CompileError::TypeError { message } => {
                write!(f, "type error: {message}")
            }
            CompileError::JsonError(e) => write!(f, "JSON parse error: {e}"),
            CompileError::Base64Error(e) => write!(f, "base64 decode error: {e}"),
            CompileError::DeserializeError(e) => write!(f, "bytecode deserialization error: {e}"),
        }
    }
}

impl std::error::Error for CompileError {}

#[must_use]
pub fn find_binary() -> Option<String> {
    if let Ok(s) = std::env::var("FADENO_LANG") {
        if Path::new(&s).exists() {
            return Some(s);
        }
    }
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        let fadeno_root = Path::new(&manifest).join("fadeno-lang");
        if let Ok(entries) = std::fs::read_dir(fadeno_root.join("dist-newstyle/build")) {
            if let Some(path) = search_cabal_output(entries) {
                return Some(path);
            }
        }
    }
    if let Ok(output) = Command::new("which").arg("fadeno-lang").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(path);
            }
        }
    }
    None
}

fn search_cabal_output(entries: std::fs::ReadDir) -> Option<String> {
    let mut best: Option<(String, std::time::SystemTime)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let candidate = path.join("x/fadeno-lang/build/fadeno-lang/fadeno-lang");
            if candidate.is_file() {
                if let Ok(meta) = std::fs::metadata(&candidate) {
                    if let Ok(modified) = meta.modified() {
                        let is_better = best.as_ref().is_none_or(|(_, t)| modified > *t);
                        if is_better {
                            if let Some(s) = candidate.to_str().map(String::from) {
                                best = Some((s, modified));
                            }
                        }
                    }
                }
            }
            if let Ok(sub) = std::fs::read_dir(&path) {
                if let Some(found) = search_cabal_output(sub) {
                    if let Ok(meta) = std::fs::metadata(&found) {
                        if let Ok(modified) = meta.modified() {
                            let is_better = best.as_ref().is_none_or(|(_, t)| modified > *t);
                            if is_better {
                                best = Some((found, modified));
                            }
                        }
                    }
                }
            }
        }
    }
    best.map(|(p, _)| p)
}

#[derive(Debug)]
pub enum CompileResult {
    Ok(CompileOutput),
    Untyped(CompileOutput, CompileError),
    Failed(CompileError),
}

impl CompileResult {
    pub fn ignore_type_error(self) -> Result<CompileOutput, CompileError> {
        match self {
            CompileResult::Ok(x) | CompileResult::Untyped(x, _) => Ok(x),
            CompileResult::Failed(err) => Err(err),
        }
    }

    pub fn require_typed(self) -> Result<CompileOutput, CompileError> {
        match self {
            CompileResult::Ok(x) => Ok(x),
            CompileResult::Untyped(_, err) | CompileResult::Failed(err) => Err(err),
        }
    }
}

#[must_use]
pub fn compile_file(binary: &str, path: &Path) -> CompileResult {
    let output = match Command::new(binary).arg("--emit-stdout").arg(path).output() {
        Ok(x) => x,
        Err(e) => {
            return CompileResult::Failed(CompileError::ProcessFailed {
                exit_code: -1,
                stderr: format!("failed to execute {binary}: {e}"),
            })
        }
    };

    if !output.status.success() {
        return CompileResult::Failed(CompileError::ProcessFailed {
            exit_code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = match serde_json::from_str(&stdout) {
        Ok(v) => v,
        Err(e) => return CompileResult::Failed(CompileError::JsonError(e.to_string())),
    };

    let status = json
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let type_error = if status == "error" {
        let message = json
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("type error (no message)")
            .to_string();
        Some(CompileError::TypeError { message })
    } else {
        None
    };

    let type_str = json
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();

    let bytecode_b64 = if let Some(b64) = json.get("bytecode").and_then(|v| v.as_str()) {
        b64
    } else {
        let err = type_error.unwrap_or_else(|| CompileError::TypeError {
            message: "compiler output missing 'bytecode' field".to_string(),
        });
        return CompileResult::Failed(err);
    };

    let bytecode_raw = match base64::engine::general_purpose::STANDARD.decode(bytecode_b64) {
        Ok(raw) => raw,
        Err(e) => return CompileResult::Failed(CompileError::Base64Error(e)),
    };

    let bytecode = match deserialize_compile_result(&bytecode_raw) {
        Ok(bc) => bc,
        Err(e) => return CompileResult::Failed(CompileError::DeserializeError(e)),
    };

    let compile_output = CompileOutput { type_str, bytecode };

    match type_error {
        None => CompileResult::Ok(compile_output),
        Some(err) => CompileResult::Untyped(compile_output, err),
    }
}
