use clap::Parser;

fn main() {
    let cli = lightning::cli::Cli::parse();
    let mut out = std::io::stdout().lock();
    if let Err(e) = lightning::cli::run(cli, &mut out) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
