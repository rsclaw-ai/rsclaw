//! Brand the generated `--help` text.
//!
//! Doc comments and help attributes are written with the default brand's
//! words (`~/.rsclaw`, `rsclaw.json5`, `rsclaw skills install …`). The root
//! [`crate::Cli`] runs every argument and (recursively) every subcommand
//! through [`rsclaw_brand::rebrand`] once the command tree is built. With the
//! default brand nothing is rewritten, so the help output is unchanged.

use std::borrow::Cow;

use clap::{Arg, Command, builder::StyledStr};

/// `mut_args` hook for the root command.
pub fn brand_arg(arg: Arg) -> Arg {
    brand_arg_with(arg, &rsclaw_brand::rebrand)
}

/// `mut_subcommands` hook for the root command; recurses into nested
/// subcommands.
pub fn brand_command(cmd: Command) -> Command {
    brand_command_with(cmd, &rsclaw_brand::rebrand)
}

type Rebrand = dyn for<'a> Fn(&'a str) -> Cow<'a, str>;

/// `Some(new)` when `rebrand` changed the text.
fn rewrite(text: Option<&StyledStr>, rebrand: &Rebrand) -> Option<String> {
    let text = text?.to_string();
    match rebrand(&text) {
        Cow::Owned(s) => Some(s),
        Cow::Borrowed(_) => None,
    }
}

fn brand_arg_with(mut arg: Arg, rebrand: &Rebrand) -> Arg {
    if let Some(s) = rewrite(arg.get_help(), rebrand) {
        arg = arg.help(s);
    }
    if let Some(s) = rewrite(arg.get_long_help(), rebrand) {
        arg = arg.long_help(s);
    }
    arg
}

fn brand_command_with(mut cmd: Command, rebrand: &Rebrand) -> Command {
    if let Some(s) = rewrite(cmd.get_about(), rebrand) {
        cmd = cmd.about(s);
    }
    if let Some(s) = rewrite(cmd.get_long_about(), rebrand) {
        cmd = cmd.long_about(s);
    }
    if let Some(s) = rewrite(cmd.get_before_help(), rebrand) {
        cmd = cmd.before_help(s);
    }
    if let Some(s) = rewrite(cmd.get_before_long_help(), rebrand) {
        cmd = cmd.before_long_help(s);
    }
    if let Some(s) = rewrite(cmd.get_after_help(), rebrand) {
        cmd = cmd.after_help(s);
    }
    if let Some(s) = rewrite(cmd.get_after_long_help(), rebrand) {
        cmd = cmd.after_long_help(s);
    }
    cmd.mut_args(|a| brand_arg_with(a, rebrand))
        .mut_subcommands(|c| brand_command_with(c, rebrand))
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;
    use crate::Cli;

    fn acme(text: &str) -> Cow<'_, str> {
        if text.contains("rsclaw") {
            Cow::Owned(
                text.replace("~/.rsclaw", "~/.acme")
                    .replace("rsclaw ", "acme "),
            )
        } else {
            Cow::Borrowed(text)
        }
    }

    fn render_help(cmd: &mut Command, path: &[&str]) -> String {
        let mut cur = cmd;
        for name in path {
            cur = cur.find_subcommand_mut(name).expect("subcommand exists");
        }
        cur.render_long_help().to_string()
    }

    #[test]
    fn default_brand_leaves_help_unchanged() {
        // The default build's rebrand is the identity, so the root help still
        // carries the historical wording.
        assert!(rsclaw_brand::is_default_text());
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("~/.rsclaw-dev"));
    }

    #[test]
    fn nested_subcommand_help_is_rebranded() {
        let mut cmd =
            brand_command_with(Cli::command(), &acme).mut_args(|a| brand_arg_with(a, &acme));
        let root = render_help(&mut cmd, &[]);
        assert!(root.contains("~/.acme-dev"), "{root}");
        assert!(!root.contains("~/.rsclaw"), "{root}");
        let install = render_help(&mut cmd, &["skills", "install"]);
        assert!(
            install.contains("acme skills install web-search"),
            "{install}"
        );
        assert!(!install.contains("rsclaw skills"), "{install}");
    }
}
