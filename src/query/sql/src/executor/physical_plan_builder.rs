// Copyright 2022 Datafuse Labs.
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

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::sync::Arc;

use common_catalog::catalog::CatalogManager;
use common_catalog::catalog_kind::CATALOG_DEFAULT;
use common_catalog::plan::PrewhereInfo;
use common_catalog::plan::Projection;
use common_catalog::plan::PushDownInfo;
use common_catalog::table_context::TableContext;
use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::type_check::check_function;
use common_expression::types::DataType;
use common_expression::ConstantFolder;
use common_expression::DataBlock;
use common_expression::DataSchemaRefExt;
use common_expression::Expr;
use common_expression::RemoteExpr;
use common_expression::TableSchema;
use common_functions::scalars::BUILTIN_FUNCTIONS;
use itertools::Itertools;

use super::cast_expr_to_non_null_boolean;
use super::AggregateFinal;
use super::AggregateFunctionDesc;
use super::AggregateFunctionSignature;
use super::AggregatePartial;
use super::Exchange as PhysicalExchange;
use super::Filter;
use super::HashJoin;
use super::Limit;
use super::Sort;
use super::TableScan;
use super::Unnest;
use crate::executor::explain::PlanStatsInfo;
use crate::executor::table_read_plan::ToReadDataSourcePlan;
use crate::executor::EvalScalar;
use crate::executor::FragmentKind;
use crate::executor::PhysicalPlan;
use crate::executor::RuntimeFilterSource;
use crate::executor::SortDesc;
use crate::executor::UnionAll;
use crate::optimizer::ColumnSet;
use crate::optimizer::RelExpr;
use crate::optimizer::SExpr;
use crate::plans::AggregateMode;
use crate::plans::AndExpr;
use crate::plans::Exchange;
use crate::plans::RelOperator;
use crate::plans::ScalarExpr;
use crate::plans::Scan;
use crate::BaseTableColumn;
use crate::ColumnEntry;
use crate::DerivedColumn;
use crate::IndexType;
use crate::Metadata;
use crate::MetadataRef;
use crate::DUMMY_COLUMN_INDEX;
use crate::DUMMY_TABLE_INDEX;

pub struct PhysicalPlanBuilder {
    metadata: MetadataRef,
    ctx: Arc<dyn TableContext>,
    next_plan_id: u32,
}

impl PhysicalPlanBuilder {
    pub fn new(metadata: MetadataRef, ctx: Arc<dyn TableContext>) -> Self {
        Self {
            metadata,
            ctx,
            next_plan_id: 0,
        }
    }

    fn next_plan_id(&mut self) -> u32 {
        let id = self.next_plan_id;
        self.next_plan_id += 1;
        id
    }

    fn build_projection(
        metadata: &Metadata,
        schema: &TableSchema,
        columns: &ColumnSet,
        has_inner_column: bool,
    ) -> Projection {
        if !has_inner_column {
            let col_indices = columns
                .iter()
                .map(|index| {
                    let name = match metadata.column(*index) {
                        ColumnEntry::BaseTableColumn(BaseTableColumn { column_name, .. }) => {
                            column_name
                        }
                        ColumnEntry::DerivedColumn(DerivedColumn { alias, .. }) => alias,
                    };
                    schema.index_of(name).unwrap()
                })
                .sorted()
                .collect::<Vec<_>>();
            Projection::Columns(col_indices)
        } else {
            let col_indices = columns
                .iter()
                .map(|index| {
                    let column = metadata.column(*index);
                    match column {
                        ColumnEntry::BaseTableColumn(BaseTableColumn {
                            column_name,
                            path_indices,
                            ..
                        }) => match path_indices {
                            Some(path_indices) => (column.index(), path_indices.to_vec()),
                            None => {
                                let idx = schema.index_of(column_name).unwrap();
                                (column.index(), vec![idx])
                            }
                        },
                        ColumnEntry::DerivedColumn(DerivedColumn { alias, .. }) => {
                            let idx = schema.index_of(alias).unwrap();
                            (column.index(), vec![idx])
                        }
                    }
                })
                .sorted()
                .collect::<BTreeMap<_, Vec<IndexType>>>();
            Projection::InnerColumns(col_indices)
        }
    }

    #[async_recursion::async_recursion]
    pub async fn build(&mut self, s_expr: &SExpr) -> Result<PhysicalPlan> {
        // Build stat info
        let stat_info = self.build_plan_stat_info(s_expr)?;

        match s_expr.plan() {
            RelOperator::Scan(scan) => {
                let mut has_inner_column = false;
                let mut name_mapping = BTreeMap::new();
                let metadata = self.metadata.read().clone();
                for index in scan.columns.iter() {
                    let column = metadata.column(*index);
                    if let ColumnEntry::BaseTableColumn(BaseTableColumn { path_indices, .. }) =
                        column
                    {
                        if path_indices.is_some() {
                            has_inner_column = true;
                        }
                    }

                    let name = match column {
                        ColumnEntry::BaseTableColumn(BaseTableColumn { column_name, .. }) => {
                            column_name
                        }
                        ColumnEntry::DerivedColumn(DerivedColumn { alias, .. }) => alias,
                    };
                    if let Some(prewhere) = &scan.prewhere {
                        // if there is a prewhere optimization,
                        // we can prune `PhysicalScan`'s output schema.
                        if prewhere.output_columns.contains(index) {
                            name_mapping.insert(name.to_string(), *index);
                        }
                    } else {
                        name_mapping.insert(name.to_string(), *index);
                    }
                }

                let table_entry = metadata.table(scan.table_index);
                let table = table_entry.table();
                let table_schema = table.schema();

                let push_downs = self.push_downs(scan, &table_schema, has_inner_column)?;

                let source = table
                    .read_plan_with_catalog(
                        self.ctx.clone(),
                        table_entry.catalog().to_string(),
                        Some(push_downs),
                    )
                    .await?;

                Ok(PhysicalPlan::TableScan(TableScan {
                    plan_id: self.next_plan_id(),
                    name_mapping,
                    source: Box::new(source),
                    table_index: scan.table_index,

                    stat_info: Some(stat_info),
                }))
            }
            RelOperator::DummyTableScan(_) => {
                let catalogs = CatalogManager::instance();
                let table = catalogs
                    .get_catalog(CATALOG_DEFAULT)?
                    .get_table(self.ctx.get_tenant().as_str(), "system", "one")
                    .await?;
                let source = table
                    .read_plan_with_catalog(self.ctx.clone(), CATALOG_DEFAULT.to_string(), None)
                    .await?;
                Ok(PhysicalPlan::TableScan(TableScan {
                    plan_id: self.next_plan_id(),
                    name_mapping: BTreeMap::from([("dummy".to_string(), DUMMY_COLUMN_INDEX)]),
                    source: Box::new(source),
                    table_index: DUMMY_TABLE_INDEX,

                    stat_info: Some(PlanStatsInfo {
                        estimated_rows: 1.0,
                    }),
                }))
            }
            RelOperator::Join(join) => {
                let build_side = self.build(s_expr.child(1)?).await?;
                let probe_side = self.build(s_expr.child(0)?).await?;
                let build_schema = build_side.output_schema()?;
                let probe_schema = probe_side.output_schema()?;
                let merged_schema = DataSchemaRefExt::create(
                    probe_schema
                        .fields()
                        .iter()
                        .chain(build_schema.fields())
                        .cloned()
                        .collect::<Vec<_>>(),
                );
                Ok(PhysicalPlan::HashJoin(HashJoin {
                    plan_id: self.next_plan_id(),
                    build: Box::new(build_side),
                    probe: Box::new(probe_side),
                    join_type: join.join_type.clone(),
                    build_keys: join
                        .right_conditions
                        .iter()
                        .map(|scalar| {
                            let expr =
                                scalar
                                    .as_expr_with_col_index()?
                                    .project_column_ref(|index| {
                                        build_schema.index_of(&index.to_string()).unwrap()
                                    });
                            let (expr, _) = ConstantFolder::fold(
                                &expr,
                                self.ctx.get_function_context()?,
                                &BUILTIN_FUNCTIONS,
                            );
                            Ok(expr.as_remote_expr())
                        })
                        .collect::<Result<_>>()?,
                    probe_keys: join
                        .left_conditions
                        .iter()
                        .map(|scalar| {
                            let expr =
                                scalar
                                    .as_expr_with_col_index()?
                                    .project_column_ref(|index| {
                                        probe_schema.index_of(&index.to_string()).unwrap()
                                    });
                            let (expr, _) = ConstantFolder::fold(
                                &expr,
                                self.ctx.get_function_context()?,
                                &BUILTIN_FUNCTIONS,
                            );
                            Ok(expr.as_remote_expr())
                        })
                        .collect::<Result<_>>()?,
                    non_equi_conditions: join
                        .non_equi_conditions
                        .iter()
                        .map(|scalar| {
                            let expr =
                                scalar
                                    .as_expr_with_col_index()?
                                    .project_column_ref(|index| {
                                        merged_schema.index_of(&index.to_string()).unwrap()
                                    });
                            let (expr, _) = ConstantFolder::fold(
                                &expr,
                                self.ctx.get_function_context()?,
                                &BUILTIN_FUNCTIONS,
                            );
                            Ok(expr.as_remote_expr())
                        })
                        .collect::<Result<_>>()?,
                    marker_index: join.marker_index,
                    from_correlated_subquery: join.from_correlated_subquery,

                    contain_runtime_filter: join.contain_runtime_filter,
                    stat_info: Some(stat_info),
                }))
            }

            RelOperator::EvalScalar(eval_scalar) => {
                let input = Box::new(self.build(s_expr.child(0)?).await?);
                let input_schema = input.output_schema()?;
                // The begin offset of the eval scalar columns.
                let offset = input_schema.fields().len();

                // 1. Collect unnest scalars.
                let mut unnest_scalars = vec![];
                let exprs = eval_scalar
                    .items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
                        let scalar = match &item.scalar {
                            ScalarExpr::Unnest(crate::plans::Unnest { argument, .. }) => {
                                unnest_scalars.push(i + offset);
                                argument
                            }
                            _ => &item.scalar,
                        };

                        let expr = scalar
                            .as_expr_with_col_index()?
                            .project_column_ref(|index| {
                                input_schema.index_of(&index.to_string()).unwrap()
                            });
                        let (expr, _) = ConstantFolder::fold(
                            &expr,
                            self.ctx.get_function_context()?,
                            &BUILTIN_FUNCTIONS,
                        );
                        Ok((expr.as_remote_expr(), item.index))
                    })
                    .collect::<Result<_>>()?;

                // 2. There are unnest scalars,
                // add Unnest operator after EvalScalar operator.
                let eval_scalar_plan = PhysicalPlan::EvalScalar(EvalScalar {
                    plan_id: self.next_plan_id(),
                    input,
                    exprs,
                    stat_info: Some(stat_info.clone()),
                });

                Ok(if unnest_scalars.is_empty() {
                    eval_scalar_plan
                } else {
                    PhysicalPlan::Unnest(Unnest {
                        plan_id: self.next_plan_id(),
                        input: Box::new(eval_scalar_plan),
                        offsets: unnest_scalars,
                        stat_info: Some(stat_info),
                    })
                })
            }

            RelOperator::Filter(filter) => {
                let input = Box::new(self.build(s_expr.child(0)?).await?);
                let input_schema = input.output_schema()?;
                Ok(PhysicalPlan::Filter(Filter {
                    plan_id: self.next_plan_id(),
                    input,
                    predicates: filter
                        .predicates
                        .iter()
                        .map(|scalar| {
                            let expr =
                                scalar
                                    .as_expr_with_col_index()?
                                    .project_column_ref(|index| {
                                        input_schema.index_of(&index.to_string()).unwrap()
                                    });
                            let expr = cast_expr_to_non_null_boolean(expr)?;
                            let (expr, _) = ConstantFolder::fold(
                                &expr,
                                self.ctx.get_function_context()?,
                                &BUILTIN_FUNCTIONS,
                            );
                            Ok(expr.as_remote_expr())
                        })
                        .collect::<Result<_>>()?,

                    stat_info: Some(stat_info),
                }))
            }
            RelOperator::Aggregate(agg) => {
                let input = self.build(s_expr.child(0)?).await?;
                let input_schema = input.output_schema()?;
                let group_items = agg.group_items.iter().map(|v| v.index).collect::<Vec<_>>();

                let result = match &agg.mode {
                    AggregateMode::Partial => {
                        let agg_funcs: Vec<AggregateFunctionDesc> = agg.aggregate_functions.iter().map(|v| {
                            if let ScalarExpr::AggregateFunction(agg) = &v.scalar {
                                Ok(AggregateFunctionDesc {
                                    sig: AggregateFunctionSignature {
                                        name: agg.func_name.clone(),
                                        args: agg.args.iter().map(|s| {
                                            s.data_type()
                                        }).collect(),
                                        params: agg.params.clone(),
                                        return_type: *agg.return_type.clone(),
                                    },
                                    output_column: v.index,
                                    args: agg.args.iter().map(|arg| {
                                        if let ScalarExpr::BoundColumnRef(col) = arg {
                                            let col_index = input_schema.index_of(&col.column.index.to_string())?;
                                            Ok(col_index)
                                        } else {
                                            Err(ErrorCode::Internal(
                                                "Aggregate function argument must be a BoundColumnRef".to_string()
                                            ))
                                        }
                                    }).collect::<Result<_>>()?,
                                    arg_indices: agg.args.iter().map(|arg| {
                                        if let ScalarExpr::BoundColumnRef(col) = arg {
                                            Ok(col.column.index)
                                        } else {
                                            Err(ErrorCode::Internal(
                                                "Aggregate function argument must be a BoundColumnRef".to_string()
                                            ))
                                        }
                                    }).collect::<Result<_>>()?,
                                })
                            } else {
                                Err(ErrorCode::Internal("Expected aggregate function".to_string()))
                            }
                        }).collect::<Result<_>>()?;

                        match input {
                            PhysicalPlan::Exchange(PhysicalExchange { input, kind, .. }) => {
                                let aggregate_partial = AggregatePartial {
                                    plan_id: self.next_plan_id(),
                                    input,
                                    agg_funcs,
                                    group_by: group_items,
                                    stat_info: Some(stat_info),
                                };

                                let group_by_key_index =
                                    aggregate_partial.output_schema()?.num_fields() - 1;
                                let group_by_key_data_type =
                                    DataBlock::choose_hash_method_with_types(
                                        &agg.group_items
                                            .iter()
                                            .map(|v| v.scalar.data_type())
                                            .collect::<Vec<_>>(),
                                    )?
                                    .data_type();

                                PhysicalPlan::Exchange(PhysicalExchange {
                                    kind,
                                    input: Box::new(PhysicalPlan::AggregatePartial(
                                        aggregate_partial,
                                    )),
                                    keys: vec![RemoteExpr::ColumnRef {
                                        span: None,
                                        id: group_by_key_index,
                                        data_type: group_by_key_data_type,
                                        display_name: "_group_by_key".to_string(),
                                    }],
                                })
                            }
                            _ => PhysicalPlan::AggregatePartial(AggregatePartial {
                                plan_id: self.next_plan_id(),
                                agg_funcs,
                                group_by: group_items,
                                input: Box::new(input),

                                stat_info: Some(stat_info),
                            }),
                        }
                    }

                    // Hack to get before group by schema, we should refactor this
                    AggregateMode::Final => {
                        let input_schema = match input {
                            PhysicalPlan::AggregatePartial(ref agg) => agg.input.output_schema()?,

                            PhysicalPlan::Exchange(PhysicalExchange {
                                input: box PhysicalPlan::AggregatePartial(ref agg),
                                ..
                            }) => agg.input.output_schema()?,

                            _ => {
                                return Err(ErrorCode::Internal(format!(
                                    "invalid input physical plan: {}",
                                    input.name(),
                                )));
                            }
                        };

                        let agg_funcs: Vec<AggregateFunctionDesc> = agg.aggregate_functions.iter().map(|v| {
                            if let ScalarExpr::AggregateFunction(agg) = &v.scalar {
                                Ok(AggregateFunctionDesc {
                                    sig: AggregateFunctionSignature {
                                        name: agg.func_name.clone(),
                                        args: agg.args.iter().map(|s| {
                                            s.data_type()
                                        }).collect(),
                                        params: agg.params.clone(),
                                        return_type: *agg.return_type.clone(),
                                    },
                                    output_column: v.index,
                                    args: agg.args.iter().map(|arg| {
                                        if let ScalarExpr::BoundColumnRef(col) = arg {
                                            input_schema.index_of(&col.column.index.to_string())
                                        } else {
                                            Err(ErrorCode::Internal(
                                                "Aggregate function argument must be a BoundColumnRef".to_string()
                                            ))
                                        }
                                    }).collect::<Result<_>>()?,
                                    arg_indices: agg.args.iter().map(|arg| {
                                        if let ScalarExpr::BoundColumnRef(col) = arg {
                                            Ok(col.column.index)
                                        } else {
                                            Err(ErrorCode::Internal(
                                                "Aggregate function argument must be a BoundColumnRef".to_string()
                                            ))
                                        }
                                    }).collect::<Result<_>>()?,
                                })
                            } else {
                                Err(ErrorCode::Internal("Expected aggregate function".to_string()))
                            }
                        }).collect::<Result<_>>()?;

                        match input {
                            PhysicalPlan::AggregatePartial(ref partial) => {
                                let before_group_by_schema = partial.input.output_schema()?;
                                let limit = agg.limit;
                                PhysicalPlan::AggregateFinal(AggregateFinal {
                                    plan_id: self.next_plan_id(),
                                    input: Box::new(input),
                                    group_by: group_items,
                                    agg_funcs,
                                    before_group_by_schema,

                                    stat_info: Some(stat_info),
                                    limit,
                                })
                            }

                            PhysicalPlan::Exchange(PhysicalExchange {
                                input: box PhysicalPlan::AggregatePartial(ref partial),
                                ..
                            }) => {
                                let before_group_by_schema = partial.input.output_schema()?;
                                let limit = agg.limit;

                                PhysicalPlan::AggregateFinal(AggregateFinal {
                                    plan_id: self.next_plan_id(),
                                    input: Box::new(input),
                                    group_by: group_items,
                                    agg_funcs,
                                    before_group_by_schema,

                                    stat_info: Some(stat_info),
                                    limit,
                                })
                            }

                            _ => {
                                return Err(ErrorCode::Internal(format!(
                                    "invalid input physical plan: {}",
                                    input.name(),
                                )));
                            }
                        }
                    }
                    AggregateMode::Initial => {
                        return Err(ErrorCode::Internal("Invalid aggregate mode: Initial"));
                    }
                };

                Ok(result)
            }
            RelOperator::Sort(sort) => Ok(PhysicalPlan::Sort(Sort {
                plan_id: self.next_plan_id(),
                input: Box::new(self.build(s_expr.child(0)?).await?),
                order_by: sort
                    .items
                    .iter()
                    .map(|v| SortDesc {
                        asc: v.asc,
                        nulls_first: v.nulls_first,
                        order_by: v.index,
                    })
                    .collect(),
                limit: sort.limit,

                stat_info: Some(stat_info),
            })),
            RelOperator::Limit(limit) => Ok(PhysicalPlan::Limit(Limit {
                plan_id: self.next_plan_id(),
                input: Box::new(self.build(s_expr.child(0)?).await?),
                limit: limit.limit,
                offset: limit.offset,

                stat_info: Some(stat_info),
            })),
            RelOperator::Exchange(exchange) => {
                let input = Box::new(self.build(s_expr.child(0)?).await?);
                let input_schema = input.output_schema()?;
                let mut keys = vec![];
                let kind = match exchange {
                    Exchange::Random => FragmentKind::Init,
                    Exchange::Hash(scalars) => {
                        for scalar in scalars {
                            let expr =
                                scalar
                                    .as_expr_with_col_index()?
                                    .project_column_ref(|index| {
                                        input_schema.index_of(&index.to_string()).unwrap()
                                    });
                            let (expr, _) = ConstantFolder::fold(
                                &expr,
                                self.ctx.get_function_context()?,
                                &BUILTIN_FUNCTIONS,
                            );
                            keys.push(expr.as_remote_expr());
                        }
                        FragmentKind::Normal
                    }
                    Exchange::Broadcast => FragmentKind::Expansive,
                    Exchange::Merge => FragmentKind::Merge,
                };
                Ok(PhysicalPlan::Exchange(PhysicalExchange {
                    input,
                    kind,
                    keys,
                }))
            }
            RelOperator::UnionAll(op) => {
                let left = self.build(s_expr.child(0)?).await?;
                let left_schema = left.output_schema()?;
                let pairs = op
                    .pairs
                    .iter()
                    .map(|(l, r)| (l.to_string(), r.to_string()))
                    .collect::<Vec<_>>();
                let fields = pairs
                    .iter()
                    .map(|(left, _)| Ok(left_schema.field_with_name(left)?.clone()))
                    .collect::<Result<Vec<_>>>()?;
                Ok(PhysicalPlan::UnionAll(UnionAll {
                    plan_id: self.next_plan_id(),
                    left: Box::new(left),
                    right: Box::new(self.build(s_expr.child(1)?).await?),
                    pairs,
                    schema: DataSchemaRefExt::create(fields),

                    stat_info: Some(stat_info),
                }))
            }
            RelOperator::RuntimeFilterSource(op) => {
                let left_side = Box::new(self.build(s_expr.child(0)?).await?);
                let left_schema = left_side.output_schema()?;
                let right_side = Box::new(self.build(s_expr.child(1)?).await?);
                let right_schema = right_side.output_schema()?;
                let mut left_runtime_filters = BTreeMap::new();
                let mut right_runtime_filters = BTreeMap::new();
                for (left, right) in op
                    .left_runtime_filters
                    .iter()
                    .zip(op.right_runtime_filters.iter())
                {
                    left_runtime_filters.insert(
                        left.0.clone(),
                        left.1
                            .as_expr_with_col_index()?
                            .project_column_ref(|index| {
                                left_schema.index_of(&index.to_string()).unwrap()
                            })
                            .as_remote_expr(),
                    );
                    right_runtime_filters.insert(
                        right.0.clone(),
                        right
                            .1
                            .as_expr_with_col_index()?
                            .project_column_ref(|index| {
                                right_schema.index_of(&index.to_string()).unwrap()
                            })
                            .as_remote_expr(),
                    );
                }
                Ok(PhysicalPlan::RuntimeFilterSource(RuntimeFilterSource {
                    plan_id: self.next_plan_id(),
                    left_side,
                    right_side,
                    left_runtime_filters,
                    right_runtime_filters,
                }))
            }
            _ => Err(ErrorCode::Internal(format!(
                "Unsupported physical plan: {:?}",
                s_expr.plan()
            ))),
        }
    }

    fn push_downs(
        &self,
        scan: &Scan,
        table_schema: &TableSchema,
        has_inner_column: bool,
    ) -> Result<PushDownInfo> {
        let metadata = self.metadata.read().clone();
        let projection =
            Self::build_projection(&metadata, table_schema, &scan.columns, has_inner_column);
        let _project_schema = projection.project_schema(table_schema);

        let push_down_filter = scan
            .push_down_predicates
            .as_ref()
            .filter(|p| !p.is_empty())
            .map(|predicates| -> Result<RemoteExpr<String>> {
                let predicates: Result<Vec<Expr<String>>> = predicates
                    .iter()
                    .map(|p| p.as_expr_with_col_name())
                    .collect();

                let predicates = predicates?;
                let expr = predicates
                    .into_iter()
                    .reduce(|lhs, rhs| {
                        check_function(None, "and", &[], &[lhs, rhs], &BUILTIN_FUNCTIONS).unwrap()
                    })
                    .unwrap();

                let expr = cast_expr_to_non_null_boolean(expr)?;

                let (expr, _) = ConstantFolder::fold(
                    &expr,
                    self.ctx.get_function_context()?,
                    &BUILTIN_FUNCTIONS,
                );
                Ok(expr.as_remote_expr())
            })
            .transpose()?;

        let prewhere_info = scan
            .prewhere
            .as_ref()
            .map(|prewhere| -> Result<PrewhereInfo> {
                let remain_columns = scan
                    .columns
                    .difference(&prewhere.prewhere_columns)
                    .copied()
                    .collect::<HashSet<usize>>();

                let output_columns = Self::build_projection(
                    &metadata,
                    table_schema,
                    &prewhere.output_columns,
                    has_inner_column,
                );
                let prewhere_columns = Self::build_projection(
                    &metadata,
                    table_schema,
                    &prewhere.prewhere_columns,
                    has_inner_column,
                );
                let remain_columns = Self::build_projection(
                    &metadata,
                    table_schema,
                    &remain_columns,
                    has_inner_column,
                );

                let predicate = prewhere
                    .predicates
                    .iter()
                    .cloned()
                    .reduce(|lhs, rhs| {
                        ScalarExpr::AndExpr(AndExpr {
                            left: Box::new(lhs),
                            right: Box::new(rhs),
                            return_type: Box::new(DataType::Boolean),
                        })
                    })
                    .expect("there should be at least one predicate in prewhere");
                let expr = cast_expr_to_non_null_boolean(predicate.as_expr_with_col_name()?)?;
                let (filter, _) = ConstantFolder::fold(
                    &expr,
                    self.ctx.get_function_context()?,
                    &BUILTIN_FUNCTIONS,
                );
                let filter = filter.as_remote_expr();

                Ok::<PrewhereInfo, ErrorCode>(PrewhereInfo {
                    output_columns,
                    prewhere_columns,
                    remain_columns,
                    filter,
                })
            })
            .transpose()?;

        let order_by = scan
            .order_by
            .clone()
            .map(|items| {
                items
                    .into_iter()
                    .map(|item| {
                        let metadata = self.metadata.read();
                        let column = metadata.column(item.index);
                        let (name, data_type) = match column {
                            ColumnEntry::BaseTableColumn(BaseTableColumn {
                                column_name,
                                data_type,
                                ..
                            }) => (column_name.clone(), DataType::from(data_type)),
                            ColumnEntry::DerivedColumn(DerivedColumn {
                                alias, data_type, ..
                            }) => (alias.clone(), data_type.clone()),
                        };

                        // sort item is already a column
                        let scalar = RemoteExpr::ColumnRef {
                            span: None,
                            id: name.clone(),
                            data_type,
                            display_name: name,
                        };

                        Ok((scalar, item.asc, item.nulls_first))
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?;

        Ok(PushDownInfo {
            projection: Some(projection),
            filter: push_down_filter,
            prewhere: prewhere_info,
            limit: scan.limit,
            order_by: order_by.unwrap_or_default(),
        })
    }

    fn build_plan_stat_info(&self, s_expr: &SExpr) -> Result<PlanStatsInfo> {
        let rel_expr = RelExpr::with_s_expr(s_expr);
        let prop = rel_expr.derive_relational_prop()?;

        Ok(PlanStatsInfo {
            estimated_rows: prop.cardinality,
        })
    }
}
