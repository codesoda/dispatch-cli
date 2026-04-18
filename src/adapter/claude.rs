//! Claude Code CLI adapter.
//!
//! Assembles `claude [extra_args...] -p` and feeds the prompt via stdin from
//! `prompt_file`. Non-interactive (`-p`) mode only; interactive support is a
//! follow-up.

use super::{AdapterError, BuildContext, Launch};

pub fn build(ctx: &BuildContext<'_>) -> Result<Launch, AdapterError> {
    let mut args: Vec<String> = ctx.extra_args.to_vec();
    args.push("-p".to_string());

    Ok(Launch {
        program: "claude".to_string(),
        args,
        wrap_in_shell: false,
        stdin_file: ctx.prompt_file.map(|p| p.to_path_buf()),
    })
}

#[cfg(test)]
mod tests {
    use super::super::{Adapter, BuildContext};
    use std::path::Path;

    #[test]
    fn extra_args_precede_dash_p() {
        let extras = vec![
            "--dangerously-skip-permissions".to_string(),
            "--model".to_string(),
            "sonnet".to_string(),
        ];
        let ctx = BuildContext {
            extra_args: &extras,
            prompt_file: None,
            command_string: None,
        };
        let launch = Adapter::Claude.build(&ctx).unwrap();
        assert_eq!(launch.program, "claude");
        assert_eq!(
            launch.args,
            vec!["--dangerously-skip-permissions", "--model", "sonnet", "-p"]
        );
        assert!(!launch.wrap_in_shell);
        assert!(launch.stdin_file.is_none());
    }

    #[test]
    fn prompt_file_becomes_stdin() {
        let prompt = Path::new("/tmp/prompt.md");
        let ctx = BuildContext {
            extra_args: &[],
            prompt_file: Some(prompt),
            command_string: None,
        };
        let launch = Adapter::Claude.build(&ctx).unwrap();
        assert_eq!(launch.args, vec!["-p"]);
        assert_eq!(launch.stdin_file.as_deref(), Some(prompt));
    }
}
