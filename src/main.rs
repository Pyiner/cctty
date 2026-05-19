#[tokio::main]
async fn main() {
    let exit_code = match cctty::run_cli(std::env::args().collect()).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("cctty: {error}");
            error.exit_code()
        }
    };
    std::process::exit(exit_code);
}
