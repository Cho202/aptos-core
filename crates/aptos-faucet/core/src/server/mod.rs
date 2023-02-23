// Copyright © Aptos Foundation

mod generate_openapi;
mod run;
mod server_args;
mod validate_config;

use anyhow::Result;
use clap::Subcommand;
use generate_openapi::GenerateOpenapi;
use run::Run;
pub use run::{FunderKeyEnum, RunConfig};
use validate_config::ValidateConfig;

#[derive(Clone, Debug, Subcommand)]
pub enum Server {
    /// Run the server.
    Run(Run),

    /// Confirm a server config is valid.
    ValidateConfig(ValidateConfig),

    /// Generate the OpenAPI spec.
    GenerateOpenapi(GenerateOpenapi),
}

impl Server {
    pub async fn run_command(&self) -> Result<()> {
        let result: Result<()> = match self {
            Server::Run(args) => args.run().await,
            Server::GenerateOpenapi(args) => args.generate_openapi().await,
            Server::ValidateConfig(args) => args.validate_config().await,
        };
        result
    }
}
