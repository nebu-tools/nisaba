//! `nisaba COMMAND [SETTINGS]`; see [`nisaba_server::cli::usage`].

fn main() {
    std::process::exit(nisaba_server::cli::main(
        &std::env::args().skip(1).collect::<Vec<_>>(),
    ));
}
