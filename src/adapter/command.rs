//! Generic shell-command adapter.
//!
//! Runs whatever the user wrote in `command = "..."` under `sh -c`. Useful
//! for bash-script / non-LLM workers where the caller wants full control over
//! the launch string.

use super::{shell_arg_quote, AdapterError, BuildContext, Launch};

pub fn build(ctx: &BuildContext<'_>) -> Result<Launch, AdapterError> {
    let cmd = ctx
        .command_string
        .ok_or(AdapterError::MissingCommandString)?;

    if cmd.contains("{prompt_file}") && ctx.prompt_file.is_none() {
        return Err(AdapterError::MissingPromptFile);
    }
    if cmd.contains("{prompt}") && ctx.prompt_inline.is_none() {
        return Err(AdapterError::MissingPromptInline);
    }

    let expanded = substitute(cmd, ctx);

    Ok(Launch {
        program: "sh".to_string(),
        args: vec!["-c".to_string(), expanded],
        wrap_in_shell: true,
        stdin_file: None,
    })
}

/// Replace `{prompt_file}` / `{prompt}` tokens in a shell command template.
/// Each replacement is POSIX-shell-escaped so prompt content with spaces,
/// quotes, or `$(...)` is treated as literal text by `sh -c`, not as shell
/// syntax. Users should NOT wrap the tokens in their own quotes — the
/// adapter does the quoting.
fn substitute(template: &str, ctx: &BuildContext<'_>) -> String {
    let mut out = template.to_string();
    if let Some(path) = ctx.prompt_file {
        out = out.replace(
            "{prompt_file}",
            &shell_arg_quote(&path.display().to_string()),
        );
    }
    if let Some(text) = ctx.prompt_inline {
        out = out.replace("{prompt}", &shell_arg_quote(text));
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
            stream_json: false,
            interactive: false,
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
        let mut c = ctx(Some("echo {prompt}"));
        c.prompt_inline = Some("hello world");
        let launch = Adapter::Command.build(&c).unwrap();
        assert_eq!(launch.args, vec!["-c", "echo 'hello world'"]);
    }

    #[test]
    fn escapes_injection_in_prompt_inline() {
        // Arbitrary shell metacharacters must stay inside single quotes and
        // never execute as shell.
        let mut c = ctx(Some("worker --prompt {prompt}"));
        c.prompt_inline = Some("$(rm -rf /)");
        let launch = Adapter::Command.build(&c).unwrap();
        assert_eq!(
            launch.args,
            vec!["-c", "worker --prompt '$(rm -rf /)'"],
            "injection attempt must be quoted, not executed"
        );
    }

    #[test]
    fn escapes_single_quote_in_prompt_inline() {
        let mut c = ctx(Some("worker {prompt}"));
        c.prompt_inline = Some("it's broken");
        let launch = Adapter::Command.build(&c).unwrap();
        assert_eq!(launch.args, vec!["-c", r#"worker 'it'\''s broken'"#]);
    }

    #[test]
    fn escapes_spaces_in_prompt_file_path() {
        let mut c = ctx(Some("./worker.sh --prompt {prompt_file}"));
        c.prompt_file = Some(Path::new("/tmp/my prompt.md"));
        let launch = Adapter::Command.build(&c).unwrap();
        assert_eq!(
            launch.args,
            vec!["-c", "./worker.sh --prompt '/tmp/my prompt.md'"]
        );
    }

    #[test]
    fn missing_prompt_file_errors_when_token_used() {
        let c = ctx(Some("./worker.sh --prompt {prompt_file}"));
        assert!(matches!(
            Adapter::Command.build(&c),
            Err(AdapterError::MissingPromptFile)
        ));
    }

    #[test]
    fn missing_prompt_inline_errors_when_token_used() {
        let c = ctx(Some("worker --prompt {prompt}"));
        assert!(matches!(
            Adapter::Command.build(&c),
            Err(AdapterError::MissingPromptInline)
        ));
    }
}
