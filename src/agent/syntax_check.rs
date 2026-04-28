//! Syntax checking for script files before execution.
//!
//! Provides pre-execution syntax validation to catch errors early,
//! giving the LLM clear diagnostic messages instead of confusing runtime errors.

use std::path::Path;
use std::process::Command;
use serde_json::{json, Value};

/// Check syntax of a script file based on its extension/shebang.
/// Returns None if no check is available, or Some(result) with diagnostics.
pub fn check_syntax(path: &Path, content: &str) -> Option<Value> {
    // Determine language from extension
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

    // Also check shebang for shell scripts without .sh extension
    let shebang = content.lines().next().unwrap_or("");
    let is_shell_script = ext == "sh" || ext == "bash" || ext == "zsh"
        || shebang.starts_with("#!/bin/bash")
        || shebang.starts_with("#!/bin/sh")
        || shebang.starts_with("#!/usr/bin/env bash")
        || shebang.starts_with("#!/usr/bin/env sh");

    match ext {
        "py" => check_python(path),
        "js" | "mjs" | "cjs" => check_javascript(path),
        "ts" | "tsx" => check_typescript(path),
        _ => if is_shell_script { check_shell(path) } else { None },
    }
}

/// Check Python syntax using `python -m py_compile`.
/// This only checks syntax, does not execute the code.
fn check_python(path: &Path) -> Option<Value> {
    let path_str = path.to_string_lossy();
    let output = Command::new("python")
        .arg("-m")
        .arg("py_compile")
        .arg(&path_str as &str)
        .output();

    match output {
        Ok(o) => {
            // py_compile exits with 0 on success, non-zero on syntax error
            if o.status.success() {
                return None; // No syntax errors
            }
            // Parse stderr for error details
            let stderr = String::from_utf8_lossy(&o.stderr);
            Some(parse_python_error(&stderr, path_str.as_ref()))
        }
        Err(e) => {
            // python not found or other error
            Some(json!({
                "warning": "Python syntax check failed",
                "reason": e.to_string(),
                "hint": "Install Python or skip syntax check"
            }))
        }
    }
}

/// Parse Python syntax error output into structured format.
fn parse_python_error(stderr: &str, path: &str) -> Value {
    // Typical Python syntax error format:
    // File "script.py", line 5
    //   print("hello"
    //               ^
    // SyntaxError: unexpected EOF while parsing

    let mut line_num: Option<usize> = None;
    let mut error_type: Option<String> = None;
    let mut error_msg: Option<String> = None;

    for line in stderr.lines() {
        // Extract line number: File "script.py", line 5
        if line.starts_with("  File") || line.starts_with("File") {
            if let Some(pos) = line.find("line ") {
                let rest = &line[pos + 5..];
                if let Some(num_str) = rest.split(',').next() {
                    line_num = num_str.trim().parse().ok();
                }
            }
        }
        // Extract error type: SyntaxError: ...
        if line.contains("SyntaxError") || line.contains("IndentationError") || line.contains("NameError") {
            if let Some(pos) = line.find(':') {
                error_type = Some(line[..pos].trim().to_string());
                error_msg = Some(line[pos + 1..].trim().to_string());
            }
        }
    }

    json!({
        "syntax_error": true,
        "file": path,
        "line": line_num,
        "error_type": error_type,
        "message": error_msg,
        "raw_output": stderr.trim(),
        "hint": if let Some(ln) = line_num {
            format!("Check line {} for syntax issues. Common causes: missing parenthesis, mismatched quotes, indentation errors.", ln)
        } else {
            "Review the file for syntax errors before executing.".to_string()
        }
    })
}

/// Check shell script syntax using `bash -n`.
/// This performs a syntax check without executing the script.
fn check_shell(path: &Path) -> Option<Value> {
    let path_str = path.to_string_lossy();
    let output = Command::new("bash")
        .arg("-n")
        .arg(&path_str as &str)
        .output();

    match output {
        Ok(o) => {
            if o.status.success() {
                return None; // No syntax errors
            }
            let stderr = String::from_utf8_lossy(&o.stderr);
            Some(parse_shell_error(&stderr, path_str.as_ref()))
        }
        Err(e) => {
            Some(json!({
                "warning": "Shell syntax check failed",
                "reason": e.to_string(),
                "hint": "bash not available for syntax check"
            }))
        }
    }
}

/// Parse shell syntax error output into structured format.
fn parse_shell_error(stderr: &str, path: &str) -> Value {
    // Typical bash syntax error format:
    // script.sh: line 5: syntax error: unexpected end of file
    // script.sh: line 3: syntax error near unexpected token `fi'

    let mut line_num: Option<usize> = None;
    let mut error_msg: Option<String> = None;

    for line in stderr.lines() {
        if line.contains(": line ") {
            // Extract line number
            if let Some(pos) = line.find(": line ") {
                let rest = &line[pos + 7..];
                if let Some(end) = rest.find(':') {
                    line_num = rest[..end].trim().parse().ok();
                    error_msg = Some(rest[end + 1..].trim().to_string());
                }
            }
        }
    }

    json!({
        "syntax_error": true,
        "file": path,
        "line": line_num,
        "message": error_msg,
        "raw_output": stderr.trim(),
        "hint": if let Some(ln) = line_num {
            format!("Check line {} for shell syntax issues. Common causes: missing 'then'/'fi', unbalanced quotes, missing semicolon.", ln)
        } else {
            "Review the shell script for syntax errors before executing.".to_string()
        }
    })
}

/// Check JavaScript syntax using `node --check`.
/// Node.js --check only validates syntax, does not execute.
fn check_javascript(path: &Path) -> Option<Value> {
    let path_str = path.to_string_lossy();
    let output = Command::new("node")
        .arg("--check")
        .arg(&path_str as &str)
        .output();

    match output {
        Ok(o) => {
            if o.status.success() {
                return None;
            }
            let stderr = String::from_utf8_lossy(&o.stderr);
            Some(parse_js_error(&stderr, path_str.as_ref()))
        }
        Err(e) => {
            Some(json!({
                "warning": "JavaScript syntax check failed",
                "reason": e.to_string(),
                "hint": "Node.js not available for syntax check"
            }))
        }
    }
}

/// Parse JavaScript/Node.js syntax error output.
fn parse_js_error(stderr: &str, path: &str) -> Value {
    // Node.js syntax error format:
    // script.js:5
    //   print("hello"
    //             ^
    // SyntaxError: Unexpected end of input

    let mut line_num: Option<usize> = None;
    let mut error_msg: Option<String> = None;

    for line in stderr.lines() {
        // Line number: script.js:5 or script.js:5:10
        if line.contains(path) {
            if let Some(pos) = line.find(':') {
                let rest = &line[pos + 1..];
                if let Some(num_str) = rest.split(':').next() {
                    line_num = num_str.trim().parse().ok();
                }
            }
        }
        // Error message
        if line.contains("SyntaxError") {
            if let Some(pos) = line.find(':') {
                error_msg = Some(line[pos + 1..].trim().to_string());
            }
        }
    }

    json!({
        "syntax_error": true,
        "file": path,
        "line": line_num,
        "message": error_msg,
        "raw_output": stderr.trim(),
        "hint": if let Some(ln) = line_num {
            format!("Check line {} for JavaScript syntax issues. Common causes: missing bracket, mismatched quotes, missing semicolon.", ln)
        } else {
            "Review the JavaScript file for syntax errors before executing.".to_string()
        }
    })
}

/// Check TypeScript syntax using `tsc --noEmit`.
/// Requires TypeScript compiler to be installed.
fn check_typescript(path: &Path) -> Option<Value> {
    let path_str = path.to_string_lossy();
    let output = Command::new("tsc")
        .arg("--noEmit")
        .arg(&path_str as &str)
        .output();

    match output {
        Ok(o) => {
            if o.status.success() {
                return None;
            }
            let stderr = String::from_utf8_lossy(&o.stderr);
            Some(parse_ts_error(&stderr, path_str.as_ref()))
        }
        Err(e) => {
            Some(json!({
                "warning": "TypeScript syntax check failed",
                "reason": e.to_string(),
                "hint": "TypeScript compiler (tsc) not available"
            }))
        }
    }
}

/// Parse TypeScript compiler error output.
fn parse_ts_error(stderr: &str, path: &str) -> Value {
    let mut line_num: Option<usize> = None;
    let mut error_msg: Option<String> = None;

    for line in stderr.lines() {
        // TypeScript error format: script.ts(5,10): error TS1005: ...
        if line.contains(path) && line.contains("error") {
            // Extract line number from (line,col) format
            if let Some(start) = line.find('(') {
                if let Some(end) = line.find(',') {
                    let num_str = &line[start + 1..end];
                    line_num = num_str.trim().parse().ok();
                }
            }
            // Extract message after "error TS..."
            if let Some(pos) = line.find("error") {
                error_msg = Some(line[pos..].trim().to_string());
            }
        }
    }

    json!({
        "syntax_error": true,
        "file": path,
        "line": line_num,
        "message": error_msg,
        "raw_output": stderr.trim(),
        "hint": if let Some(ln) = line_num {
            format!("Check line {} for TypeScript issues. Run `tsc --noEmit` for full diagnostics.", ln)
        } else {
            "Review TypeScript errors and fix before executing.".to_string()
        }
    })
}