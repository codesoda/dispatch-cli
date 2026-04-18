//! Generic shell-command adapter.
//!
//! Runs whatever the user wrote in `command = "..."` under `sh -c`. Useful
//! for bash-script / non-LLM workers where the caller wants full control over
//! the launch string.

use super::{AdapterError, BuildContext, Launch};

pub fn build(ctx: &BuildContext<'_>) -> Result<Launch, AdapterError> {
    let cmd = ctx
        .command_string
        .ok_or(AdapterError::MissingCommandString)?;

    Ok(Launch {
        program: "sh".to_string(),
        args: vec!["-c".to_string(), cmd.to_string()],
        wrap_in_shell: true,
        stdin_file: None,
    })
}

#[cfg(test)]
mod tests {
    use super::super::{Adapter, BuildContext};
    use super::*;

    #[test]
    fn wraps_in_sh_dash_c() {
        let ctx = BuildContext {
            extra_args: &[],
            prompt_file: None,
            command_string: Some("./worker.sh --flag"),
        };
        let launch = Adapter::Command.build(&ctx).unwrap();
        assert_eq!(launch.program, "sh");
        assert_eq!(launch.args, vec!["-c", "./worker.sh --flag"]);
        assert!(launch.wrap_in_shell);
        assert!(launch.stdin_file.is_none());
    }

    #[test]
    fn missing_command_string_errors() {
        let ctx = BuildContext {
            extra_args: &[],
            prompt_file: None,
            command_string: None,
        };
        assert!(matches!(
            Adapter::Command.build(&ctx),
            Err(AdapterError::MissingCommandString)
        ));
    }
}
