// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let statement = Table::create()
            .table(Request::Table)
            .if_not_exists()
            .col(ColumnDef::new(Request::Ip).string().not_null())
            // 64 hex characters, we trim the 0x.
            .col(
                ColumnDef::new(Request::AccountAddress)
                    .char_len(66)
                    .not_null(),
            )
            .col(ColumnDef::new(Request::Amount).big_integer().not_null())
            .col(
                ColumnDef::new(Request::InsertionUnixtimeSecs)
                    .big_integer()
                    .not_null(),
            )
            // We overload this field to store completion in the failure case too,
            // in which case this value will be 0. In other words:
            //   NULL: Request ongoing
            //   = 0: Request failed
            //   > 0: Request succeeded
            .col(ColumnDef::new(Request::CompletedUnixtimeSecs).big_integer())
            // To avoid the pain of using an actual array, this is just a comma
            // separated list of txn hashes.
            .col(ColumnDef::new(Request::TxnHashes).string())
            // We use this to ensure we don't get more than one request per IP
            // per second. Not super important, but good to have, plus it gives
            // us a PK.
            .primary_key(
                Index::create()
                    .col(Request::Ip)
                    .col(Request::InsertionUnixtimeSecs),
            )
            .to_owned();
        manager.create_table(statement).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Request::Table).to_owned())
            .await
    }
}

#[derive(Iden)]
enum Request {
    Table,
    Ip,
    AccountAddress,
    Amount,
    InsertionUnixtimeSecs,
    CompletedUnixtimeSecs,
    TxnHashes,
}
