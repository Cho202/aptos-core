// Copyright © Aptos Foundation

use super::traits::Bypasser;
use crate::{
    checkers::CheckerData,
    common::{IpRangeManager, IpRangeManagerConfig},
};
use anyhow::Result;
use async_trait::async_trait;

pub struct IpAllowlistBypasser {
    manager: IpRangeManager,
}

impl IpAllowlistBypasser {
    pub fn new(config: IpRangeManagerConfig) -> Result<Self> {
        Ok(Self {
            manager: IpRangeManager::new(config)?,
        })
    }
}

#[async_trait]
impl Bypasser for IpAllowlistBypasser {
    async fn request_can_bypass(&self, data: CheckerData) -> Result<bool> {
        Ok(self.manager.contains_ip(&data.source_ip))
    }
}
