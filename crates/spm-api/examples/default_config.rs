//! Prints the default `config.toml` exactly as the miner writes it on the first start, comments
//! included. The default-file blocks of docs/en/CONFIGURATION.md and docs/pt-BR/CONFIGURACAO.md are
//! this output:
//!
//! ```text
//! cargo run -q --release -p spm-api --example default_config
//! ```

fn main() {
    print!("{}", spm_api::config::Config::default().to_toml());
}
