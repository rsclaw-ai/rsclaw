/// ANSI styling helpers for CLI output.
/// Respects `NO_COLOR` env var and `--no-color` flag.
pub fn use_color() -> bool {
    std::env::var("NO_COLOR").is_err()
}

/// Wrap `s` in an ANSI style. Styled text is CLI prose, so it follows the
/// build's brand like the rest of the output (see `cmd/mod.rs`).
fn paint(code: &str, s: &str) -> String {
    let s = rsclaw_brand::rebrand(s);
    if use_color() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.into_owned()
    }
}

pub fn green(s: &str) -> String {
    paint("32", s)
}

pub fn yellow(s: &str) -> String {
    paint("33", s)
}

pub fn red(s: &str) -> String {
    paint("31", s)
}

pub fn bold(s: &str) -> String {
    paint("1", s)
}

pub fn dim(s: &str) -> String {
    paint("2", s)
}

pub fn cyan(s: &str) -> String {
    paint("36", s)
}

pub fn banner(title: &str) {
    let title = &*rsclaw_brand::rebrand(title);
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    let c = use_color();
    let r = if c { "\x1b[31m" } else { "" }; // red
    let b = if c { "\x1b[1m" } else { "" }; // bold
    let d = if c { "\x1b[2m" } else { "" }; // dim
    let n = if c { "\x1b[0m" } else { "" }; // reset

    if !rsclaw_brand::is_default_text() {
        // Other brands get a plain title instead of the RSCLAW word mark.
        println!();
        println!(
            "    {b}{}{n}  {d}-- {title} --{n}",
            rsclaw_brand::PRODUCT_NAME
        );
        println!();
        println!("    {d}>{n} engine:   {title}");
        println!("    {d}>{n} platform: {arch} on {os}");
        println!();
        println!("    {d}{}{n}", "-".repeat(56));
        println!();
        return;
    }
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
    let msg = rsclaw_brand::rebrand(msg);
    println!("  {} {msg}", green("[ok]"));
}

pub fn warn_msg(msg: &str) {
    let msg = rsclaw_brand::rebrand(msg);
    println!("  {} {msg}", yellow("[warn]"));
}

pub fn err_msg(msg: &str) {
    let msg = rsclaw_brand::rebrand(msg);
    println!("  {} {msg}", red("[error]"));
}

pub fn item(icon: &str, msg: &str) {
    let msg = rsclaw_brand::rebrand(msg);
    println!("  {icon} {msg}");
}
