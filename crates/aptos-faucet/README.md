# Aptos Faucet

## Subdirectories
This is a brief overview of the subdirectories to help you find what you're looking for. For more information on each of these, see the README in that subdirectory.

- `core/`: All core logic, including the server, endpoint handlers, bypassers, checkers, funders, etc.
- `service/`: The entrypoint for running the faucet as a service.
- `cli/`: CLI for executing the core MintFunder code from the service.
- `move_scripts/`: Move scripts necessary for the MintFunder.
- `metrics-server/`: The metrics server for the faucet service.
- `doc/`: OpenAPI spec generated from the server definition.
- `ts-client/`: Typescript client generated from the OpenAPI spec.
- `migration/`: DB migrations for the PostgresRatelimitChecker.
- `entity/`: Generated ORM code for the PostgresRatelimitChecker.
- `scripts/`: Scripts for helping with faucet development.

In all cases, if a directory holds a crate, the name of that crate is `aptos-faucet-<directory>`. For example the name of the crate in `metrics-server/` is `aptos-faucet-metrics-server`.
