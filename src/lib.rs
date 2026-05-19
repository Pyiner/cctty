mod args;
mod error;
mod pty;
mod runner;
mod transcript;

pub use error::{CcttyError, Result};

pub async fn run_cli(argv: Vec<String>) -> Result<i32> {
    runner::run(args::Invocation::parse(argv)?).await
}
