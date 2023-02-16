// Copyright 2023 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use common_catalog::table_context::TableContext;
use common_exception::Result;
use common_expression::DataBlock;
use common_meta_store::MetaStore;
use common_storage::DataOperator;
use opendal::Operator;

use crate::common::gen_common_key;
use crate::common::gen_result_cache_meta_key;
use crate::common::read_blocks_from_buffer;
use crate::meta_manager::ResultCacheMetaManager;

pub struct ResultCacheReader {
    meta_mgr: ResultCacheMetaManager,

    operator: Operator,
    partitions_sha: String,
}

impl ResultCacheReader {
    pub fn create(ctx: Arc<dyn TableContext>, kv_store: Arc<MetaStore>) -> Self {
        let sql = ctx.get_query_str();
        let tenant = ctx.get_tenant();
        let key = gen_common_key(&sql);
        let meta_key = gen_result_cache_meta_key(&tenant, &key);
        let partitions_sha = ctx.get_partitions_sha().unwrap();

        Self {
            meta_mgr: ResultCacheMetaManager::create(kv_store, meta_key, 0),
            partitions_sha,
            operator: DataOperator::instance().operator(),
        }
    }

    pub async fn try_read_cached_result(&self) -> Result<Option<Vec<DataBlock>>> {
        match self.meta_mgr.get().await? {
            Some(value) => {
                if value.partitions_sha == self.partitions_sha {
                    if value.num_rows == 0 {
                        Ok(Some(vec![DataBlock::empty()]))
                    } else {
                        Ok(Some(self.read_result_from_cache(&value.location).await?))
                    }
                } else {
                    // The cache is invalid (due to data update or other reasons).
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }

    async fn read_result_from_cache(&self, location: &str) -> Result<Vec<DataBlock>> {
        let object = self.operator.object(location);
        let data = object.read().await?;
        read_blocks_from_buffer(&mut data.as_slice())
    }
}
