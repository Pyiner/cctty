mod args;
mod error;
mod logging;
mod pty;
mod runner;
mod transcript;

pub use error::{CcttyError, Result};

pub async fn run_cli(argv: Vec<String>) -> Result<i32> {
    let invocation = match args::Invocation::parse(argv) {
        Ok(invocation) => invocation,
        Err(error) => {
            logging::event(format!("parse_error error={error}"));
            return Err(error);
        }
    };
    logging::event(format!(
        "start mode={:?} input={:?} output={:?} permission_prompt_stdio={} include_partial_messages={} passthrough_args={}",
        invocation.mode,
        invocation.input_format,
        invocation.output_format,
        invocation.permission_prompt_tool_stdio,
        invocation.include_partial_messages,
        invocation.passthrough_args.len()
    ));
    let result = runner::run(invocation).await;
    match &result {
        Ok(code) => logging::event(format!("finish exit_code={code}")),
        Err(error) => logging::event(format!("error error={error}")),
    }
    result
}
