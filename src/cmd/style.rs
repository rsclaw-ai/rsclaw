/// ANSI styling helpers for CLI output.
/// Respects `NO_COLOR` env var and `--no-color` flag.

pub fn use_color() -> bool {
    std::env::var("NO_COLOR").is_err()
}

pub fn green(s: &str) -> String {
    if use_color() {
        format!("\x1b[32m{s}\x1b[0m")
    } else {
        s.to_owned()
    }
}

pub fn yellow(s: &str) -> String {
    if use_color() {
        format!("\x1b[33m{s}\x1b[0m")
    } else {
        s.to_owned()
    }
}

pub fn red(s: &str) -> String {
    if use_color() {
        format!("\x1b[31m{s}\x1b[0m")
    } else {
        s.to_owned()
    }
}

pub fn bold(s: &str) -> String {
    if use_color() {
        format!("\x1b[1m{s}\x1b[0m")
    } else {
        s.to_owned()
    }
}

pub fn dim(s: &str) -> String {
    if use_color() {
        format!("\x1b[2m{s}\x1b[0m")
    } else {
        s.to_owned()
    }
}

pub fn cyan(s: &str) -> String {
    if use_color() {
        format!("\x1b[36m{s}\x1b[0m")
    } else {
        s.to_owned()
    }
}

pub fn banner(title: &str) {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    let c = use_color();
    let r = if c { "\x1b[31m" } else { "" }; // red
    let b = if c { "\x1b[1m" } else { "" }; // bold
    let d = if c { "\x1b[2m" } else { "" }; // dim
    let n = if c { "\x1b[0m" } else { "" }; // reset

    println!(
        r#"
    {r}.       .{n}
    {r} \     /{n}        {b} ____  ____   ____ _        _  __        __{n}
    {r}( )---( ){n}       {b}|  _ \/ ___| / ___| |      / \ \ \      / /{n}
    {r} / . . \{n}        {b}| |_) \___ \| |   | |     / _ \ \ \ /\ / /{n}
    {r}| \___/ |{n}       {b}|  _ < ___) | |___| |___ / ___ \ \ V  V /{n}
    {r} \_____/{n}        {b}|_| \_\____/ \____|_____/_/   \_\ \_/\_/{n}
    {r}/       \{n}
    {r}(       ){n}       {d}-- {title} --{n}
"#
    );
    println!("    {d}[{n} {b}core{n} {d}]{n}");
    println!("    {d}>{n} engine:   {title} {d}(Rust 2024 Edition){n}");
    println!("    {d}>{n} platform: {arch} on {os}");
    println!("    {d}>{n} compat:   OpenClaw drop-in replacement");
    println!();
    println!("    {d}{}{n}", "-".repeat(56));
    println!();
}

pub fn kv(key: &str, value: &str) {
    println!("  {:<14} {}", dim(key), value);
}

pub fn ok(msg: &str) {
    println!("  {} {msg}", green("[ok]"));
}

pub fn warn_msg(msg: &str) {
    println!("  {} {msg}", yellow("[warn]"));
}

pub fn err_msg(msg: &str) {
    println!("  {} {msg}", red("[error]"));
}

pub fn item(icon: &str, msg: &str) {
    println!("  {icon} {msg}");
}
