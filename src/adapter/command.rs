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

    let expanded = substitute(cmd, ctx);

    Ok(Launch {
        program: "sh".to_string(),
        args: vec!["-c".to_string(), expanded],
        wrap_in_shell: true,
        stdin_file: None,
    })
}

/// Replace `{prompt_file}` / `{prompt}` tokens in a shell command template.
/// Useful for bash-script workers that need the prompt path as an argument
/// rather than on stdin.
fn substitute(template: &str, ctx: &BuildContext<'_>) -> String {
    let mut out = template.to_string();
    if let Some(path) = ctx.prompt_file {
        out = out.replace("{prompt_file}", &path.display().to_string());
    }
    if let Some(text) = ctx.prompt_inline {
        out = out.replace("{prompt}", text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::{Adapter, BuildContext};
    use super::*;
    use std::path::Path;

    fn ctx<'a>(command: Option<&'a str>) -> BuildContext<'a> {
        BuildContext {
            extra_args: &[],
            prompt_file: None,
            prompt_inline: None,
            command_string: command,
        }
    }

    #[test]
    fn wraps_in_sh_dash_c() {
        let launch = Adapter::Command
            .build(&ctx(Some("./worker.sh --flag")))
            .unwrap();
        assert_eq!(launch.program, "sh");
        assert_eq!(launch.args, vec!["-c", "./worker.sh --flag"]);
        assert!(launch.wrap_in_shell);
        assert!(launch.stdin_file.is_none());
    }

    #[test]
    fn missing_command_string_errors() {
        assert!(matches!(
            Adapter::Command.build(&ctx(None)),
            Err(AdapterError::MissingCommandString)
        ));
    }

    #[test]
    fn substitutes_prompt_file_token() {
        let mut c = ctx(Some("./worker.sh --prompt {prompt_file}"));
        c.prompt_file = Some(Path::new("/tmp/prompt.md"));
        let launch = Adapter::Command.build(&c).unwrap();
        assert_eq!(
            launch.args,
            vec!["-c", "./worker.sh --prompt /tmp/prompt.md"]
        );
    }

    #[test]
    fn substitutes_prompt_inline_token() {
        let mut c = ctx(Some("echo '{prompt}'"));
        c.prompt_inline = Some("hello world");
        let launch = Adapter::Command.build(&c).unwrap();
        assert_eq!(launch.args, vec!["-c", "echo 'hello world'"]);
    }
}
