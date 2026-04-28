//! Syntax checking and error parsing for script files.
//!
//! Provides:
//! - Pre-execution syntax validation to catch errors early
//! - Runtime error parsing for clearer diagnostic messages

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

// ---------------------------------------------------------------------------
// Runtime error parsing (for execution failures)
// ---------------------------------------------------------------------------

/// Parse Python runtime error from stderr output.
/// Handles both syntax errors and runtime errors (KeyError, ValueError, etc.)
/// Returns a structured error object for clearer diagnostics.
pub fn parse_python_runtime_error(stderr: &str) -> Value {
    // Python error format (traceback):
    // Traceback (most recent call last):
    //   File "script.py", line 5, in <module>
    //     result = conn.execute("SELECT ...")
    // duckdb.duckdb.ProgrammingError: Binder Error: Referenced column "ts_code" not found

    let mut line_num: Option<usize> = None;
    let mut file_name: Option<String> = None;
    let mut error_type: Option<String> = None;
    let mut error_msg: Option<String> = None;
    let mut error_line_content: Option<String> = None;

    for line in stderr.lines() {
        // Extract location: File "script.py", line 5, in <module>
        if line.contains("File \"") && line.contains(", line ") {
            // Extract file name
            if let Some(start) = line.find("File \"") {
                if let Some(end) = line[start + 6..].find("\"") {
                    file_name = Some(line[start + 6..start + 6 + end].to_string());
                }
            }
            // Extract line number
            if let Some(pos) = line.find(", line ") {
                let rest = &line[pos + 7..];
                if let Some(num_str) = rest.split(',').next() {
                    line_num = num_str.trim().parse().ok();
                }
            }
        }
        // Extract error line content (the actual code that caused error)
        if line_num.is_some() && line.trim().starts_with("    ") {
            error_line_content = Some(line.trim().to_string());
        }
        // Extract error type and message (last non-empty line usually)
        // Formats: "ErrorType: message" or "module.ErrorType: message"
        if line.contains(": ") && !line.contains("File \"") && !line.contains("Traceback") {
            // Skip lines that look like traceback location
            let trimmed = line.trim();
            if !trimmed.starts_with("File") && !trimmed.starts_with("Traceback") {
                // This might be the error line
                if let Some(pos) = trimmed.rfind(": ") {
                    // Error type might have module prefix like "duckdb.duckdb.ProgrammingError"
                    let before_colon = &trimmed[..pos];
                    let after_colon = &trimmed[pos + 2..];
                    // Check if before_colon looks like an error type (contains dots or capital letters)
                    if before_colon.contains('.') || before_colon.chars().any(|c| c.is_uppercase()) {
                        error_type = Some(before_colon.to_string());
                        error_msg = Some(after_colon.to_string());
                    }
                }
            }
        }
    }

    // Build hint based on error type
    let hint = match error_type.as_deref() {
        Some("SyntaxError") | Some("IndentationError") => {
            if let Some(ln) = line_num {
                format!("Syntax error at line {}. Check for missing parenthesis, mismatched quotes, or indentation issues.", ln)
            } else {
                "Check for syntax errors in the code.".to_string()
            }
        }
        Some("KeyError") => {
            if let Some(msg) = &error_msg {
                format!("Key not found: {}. Check if the key exists in the data structure.", msg)
            } else {
                "A key was not found. Check the data structure for the correct key name.".to_string()
            }
        }
        Some("ValueError") => {
            if let Some(msg) = &error_msg {
                format!("Invalid value: {}. Check the input values are correct.", msg)
            } else {
                "Invalid value provided. Check input format and constraints.".to_string()
            }
        }
        Some("NameError") => {
            if let Some(msg) = &error_msg {
                format!("Name '{}' is not defined. Check if the variable/function exists.", msg)
            } else {
                "A name is not defined. Check for typo or missing import.".to_string()
            }
        }
        Some("ProgrammingError") | Some("duckdb.ProgrammingError") | Some("duckdb.duckdb.ProgrammingError") => {
            if let Some(msg) = &error_msg {
                if msg.contains("not found") || msg.contains("does not exist") {
                    format!("Database error: {}. Check column/table names are correct.", msg)
                } else {
                    format!("Database error: {}. Check your SQL query syntax.", msg)
                }
            } else {
                "Database error. Check SQL query syntax and table/column names.".to_string()
            }
        }
        Some("OperationalError") | Some("sqlite3.OperationalError") => {
            "Database operational error. Check table/column names and SQL syntax.".to_string()
        }
        Some("TypeError") => {
            if let Some(msg) = &error_msg {
                format!("Type error: {}. Check function arguments and types.", msg)
            } else {
                "Type mismatch. Check function arguments and variable types.".to_string()
            }
        }
        Some("ImportError") | Some("ModuleNotFoundError") => {
            "Module not found. Install the required package or check the import name.".to_string()
        }
        _ => {
            if let Some(ln) = line_num {
                format!("Error at line {}. Review the traceback for details.", ln)
            } else {
                "Review the error traceback for details.".to_string()
            }
        }
    };

    json!({
        "error": true,
        "error_type": error_type,
        "message": error_msg,
        "file": file_name,
        "line": line_num,
        "line_content": error_line_content,
        "raw_output": stderr.trim(),
        "hint": hint
    })
}

/// Parse shell runtime error from stderr output.
pub fn parse_shell_runtime_error(stderr: &str) -> Value {
    // Shell error format varies. Common patterns:
    // /bin/bash: line 5: command: command not found
    // python: can't open file 'script.py': [Errno 2] No such file or directory

    let mut line_num: Option<usize> = None;
    let mut error_msg: Option<String> = None;

    for line in stderr.lines() {
        if line.contains(": line ") {
            if let Some(pos) = line.find(": line ") {
                let rest = &line[pos + 7..];
                if let Some(end) = rest.find(':') {
                    line_num = rest[..end].trim().parse().ok();
                    error_msg = Some(rest[end + 1..].trim().to_string());
                }
            }
        }
        // Python file not found error
        if line.contains("can't open file") || line.contains("No such file or directory") {
            error_msg = Some(line.trim().to_string());
        }
    }

    json!({
        "error": true,
        "message": error_msg,
        "line": line_num,
        "raw_output": stderr.trim(),
        "hint": if let Some(ln) = line_num {
            format!("Error at line {}. Check command syntax and arguments.", ln)
        } else {
            "Check the command syntax and file paths.".to_string()
        }
    })
}

/// Parse runtime error based on command type (python, bash, etc.)
pub fn parse_runtime_error(command: &str, stderr: &str) -> Value {
    if stderr.is_empty() {
        return json!({"error": false, "raw_output": ""});
    }

    // Detect command type
    if command.contains("python") || command.contains("python3") {
        parse_python_runtime_error(stderr)
    } else if command.contains("bash") || command.contains("sh") || command.contains("shell") {
        parse_shell_runtime_error(stderr)
    } else {
        // Generic fallback
        json!({
            "error": true,
            "raw_output": stderr.trim(),
            "hint": "Check the command output for errors.".to_string()
        })
    }
}