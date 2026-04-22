//! Codex CLI adapter.
//!
//! Headless mode (default): assembles `codex exec [extra_args...]` and feeds
//! the prompt via stdin from `prompt_file`.
//!
//! Interactive mode (`interactive = true`): omits the `exec` subcommand so
//! codex opens in its REPL. Extra args still follow.

use super::{AdapterError, BuildContext, Launch};

pub fn build(ctx: &BuildContext<'_>) -> Result<Launch, AdapterError> {
    let mut args: Vec<String> = Vec::new();
    if !ctx.interactive {
        args.push("exec".to_string());
    }
    args.extend(ctx.extra_args.iter().cloned());

    Ok(Launch {
        program: "codex".to_string(),
        args,
        wrap_in_shell: false,
        stdin_file: ctx.prompt_file.map(|p| p.to_path_buf()),
    })
}

#[cfg(test)]
mod tests {
    use super::super::{Adapter, BuildContext};
    use std::path::Path;

    fn ctx<'a>(extras: &'a [String], prompt_file: Option<&'a Path>) -> BuildContext<'a> {
        BuildContext {
            extra_args: extras,
            prompt_file,
            prompt_inline: None,
            command_string: None,
            stream_json: false,
            interactive: false,
        }
    }

    fn ctx_interactive<'a>(
        extras: &'a [String],
        prompt_file: Option<&'a Path>,
    ) -> BuildContext<'a> {
        let mut c = ctx(extras, prompt_file);
        c.interactive = true;
        c
    }

    #[test]
    fn exec_precedes_extra_args() {
        let extras = vec![
            "-s".to_string(),
            "danger-full-access".to_string(),
            "-m".to_string(),
            "gpt-5.4".to_string(),
        ];
        let launch = Adapter::Codex.build(&ctx(&extras, None)).unwrap();
        assert_eq!(launch.program, "codex");
        assert_eq!(
            launch.args,
            vec!["exec", "-s", "danger-full-access", "-m", "gpt-5.4"]
        );
        assert!(!launch.wrap_in_shell);
        assert!(launch.stdin_file.is_none());
    }

    #[test]
    fn prompt_file_becomes_stdin() {
        let prompt = Path::new("/tmp/prompt.md");
        let launch = Adapter::Codex.build(&ctx(&[], Some(prompt))).unwrap();
        assert_eq!(launch.args, vec!["exec"]);
        assert_eq!(launch.stdin_file.as_deref(), Some(prompt));
    }

    #[test]
    fn preserves_flag_ordering() {
        let extras = vec![
            "-c".to_string(),
            "model_reasoning_effort=\"xhigh\"".to_string(),
            "-c".to_string(),
            "service_tier=\"fast\"".to_string(),
        ];
        let launch = Adapter::Codex.build(&ctx(&extras, None)).unwrap();
        assert_eq!(
            launch.args,
            vec![
                "exec",
                "-c",
                "model_reasoning_effort=\"xhigh\"",
                "-c",
                "service_tier=\"fast\"",
            ]
        );
    }

    /// `interactive = true` drops the `exec` subcommand so codex opens
    /// in its REPL. User extras still pass through in the same order.
    #[test]
    fn interactive_omits_exec() {
        let extras = vec!["-m".to_string(), "gpt-5.4".to_string()];
        let launch = Adapter::Codex
            .build(&ctx_interactive(&extras, None))
            .unwrap();
        assert_eq!(launch.args, vec!["-m", "gpt-5.4"]);
        assert!(
            !launch.args.iter().any(|a| a == "exec"),
            "`exec` must be omitted in interactive mode",
        );
    }
}
