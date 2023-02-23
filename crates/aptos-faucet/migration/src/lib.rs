// Copyright © Aptos Foundation

pub use sea_orm_migration::prelude::*;

mod m20220922_190315_create_requests_table;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m20220922_190315_create_requests_table::Migration)]
    }
}
