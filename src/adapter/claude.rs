//! Claude Code CLI adapter.
//!
//! Assembles `claude [extra_args...] -p` and feeds the prompt via stdin from
//! `prompt_file`. Non-interactive (`-p`) mode only; interactive support is a
//! follow-up.
//!
//! Issue #43: when `stream_json` is set, appends
//! `--output-format stream-json --verbose` so per-tool-use entries appear
//! in the agent log. Required because claude's default `-p` output only
//! shows the final assistant message — making a hallucinated `dispatch
//! register` call visually identical to a real one in the log.

use super::{AdapterError, BuildContext, Launch};

pub fn build(ctx: &BuildContext<'_>) -> Result<Launch, AdapterError> {
    let mut args: Vec<String> = ctx.extra_args.to_vec();
    if ctx.stream_json {
        // Claude Code requires --verbose when combining -p with stream-json.
        args.push("--output-format".to_string());
        args.push("stream-json".to_string());
        args.push("--verbose".to_string());
    }
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

    fn ctx<'a>(extras: &'a [String], prompt_file: Option<&'a Path>) -> BuildContext<'a> {
        BuildContext {
            extra_args: extras,
            prompt_file,
            prompt_inline: None,
            command_string: None,
            stream_json: false,
        }
    }

    fn ctx_stream<'a>(extras: &'a [String], prompt_file: Option<&'a Path>) -> BuildContext<'a> {
        let mut c = ctx(extras, prompt_file);
        c.stream_json = true;
        c
    }

    #[test]
    fn extra_args_precede_dash_p() {
        let extras = vec![
            "--dangerously-skip-permissions".to_string(),
            "--model".to_string(),
            "sonnet".to_string(),
        ];
        let launch = Adapter::Claude.build(&ctx(&extras, None)).unwrap();
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
        let launch = Adapter::Claude.build(&ctx(&[], Some(prompt))).unwrap();
        assert_eq!(launch.args, vec!["-p"]);
        assert_eq!(launch.stdin_file.as_deref(), Some(prompt));
    }

    /// Issue #43: when stream_json is set, claude is launched with
    /// `--output-format stream-json --verbose -p` so per-tool-use entries
    /// appear in the agent log (the verification mechanism for whether
    /// pre-register actually fixed the hallucination).
    #[test]
    fn stream_json_appends_output_format_and_verbose() {
        let launch = Adapter::Claude.build(&ctx_stream(&[], None)).unwrap();
        assert_eq!(
            launch.args,
            vec!["--output-format", "stream-json", "--verbose", "-p"],
            "stream_json must add --output-format stream-json --verbose before -p",
        );
    }

    /// Default off: stream_json=false preserves the legacy argv shape
    /// bit-for-bit, so users not opted in see no log change.
    #[test]
    fn stream_json_off_preserves_legacy_argv() {
        let extras = vec!["--model".to_string(), "sonnet".to_string()];
        let launch = Adapter::Claude.build(&ctx(&extras, None)).unwrap();
        assert_eq!(launch.args, vec!["--model", "sonnet", "-p"]);
    }

    /// User extra_args still come first; stream-json flags slot between
    /// them and `-p`. Keeps user-supplied --model / etc. consistent with
    /// the legacy argv.
    #[test]
    fn stream_json_preserves_extra_args_order() {
        let extras = vec!["--model".to_string(), "sonnet".to_string()];
        let launch = Adapter::Claude.build(&ctx_stream(&extras, None)).unwrap();
        assert_eq!(
            launch.args,
            vec![
                "--model",
                "sonnet",
                "--output-format",
                "stream-json",
                "--verbose",
                "-p",
            ],
        );
    }
}
