//! Codex CLI adapter.
//!
//! Assembles `codex exec [extra_args...]` and feeds the prompt via stdin from
//! `prompt_file`. Non-interactive (`exec`) mode only; interactive support is
//! a follow-up.

use super::{AdapterError, BuildContext, Launch};

pub fn build(ctx: &BuildContext<'_>) -> Result<Launch, AdapterError> {
    let mut args: Vec<String> = vec!["exec".to_string()];
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

    #[test]
    fn exec_preceeds_extra_args() {
        let extras = vec![
            "-s".to_string(),
            "danger-full-access".to_string(),
            "-m".to_string(),
            "gpt-5.4".to_string(),
        ];
        let ctx = BuildContext {
            extra_args: &extras,
            prompt_file: None,
            command_string: None,
        };
        let launch = Adapter::Codex.build(&ctx).unwrap();
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
        let ctx = BuildContext {
            extra_args: &[],
            prompt_file: Some(prompt),
            command_string: None,
        };
        let launch = Adapter::Codex.build(&ctx).unwrap();
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
        let ctx = BuildContext {
            extra_args: &extras,
            prompt_file: None,
            command_string: None,
        };
        let launch = Adapter::Codex.build(&ctx).unwrap();
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
}
