// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fmt::Debug;

use itertools::Itertools;
use risingwave_common::catalog::{Field, Schema};
use risingwave_common::error::{ErrorCode, Result};
use risingwave_common::types::DataType;
use risingwave_sqlparser::ast::{Expr, Select, SelectItem};

use super::bind_context::{Clause, ColumnBinding};
use super::UNNAMED_COLUMN;
use crate::binder::{Binder, Relation};
use crate::catalog::check_valid_column_name;
use crate::expr::{CorrelatedId, Expr as _, ExprImpl, InputRef};

#[derive(Debug, Clone)]
pub struct BoundSelect {
    pub distinct: bool,
    pub select_items: Vec<ExprImpl>,
    pub aliases: Vec<Option<String>>,
    pub from: Option<Relation>,
    pub where_clause: Option<ExprImpl>,
    pub group_by: Vec<ExprImpl>,
    pub having: Option<ExprImpl>,
    schema: Schema,
}

impl BoundSelect {
    /// The schema returned by this [`BoundSelect`].
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn is_correlated(&self) -> bool {
        self.select_items
            .iter()
            .chain(self.group_by.iter())
            .chain(self.where_clause.iter())
            .any(|expr| expr.has_correlated_input_ref_by_depth())
            || match self.from.as_ref() {
                Some(relation) => relation.is_correlated(),
                None => false,
            }
    }

    pub fn collect_correlated_indices_by_depth_and_assign_id(
        &mut self,
        correlated_id: CorrelatedId,
    ) -> Vec<usize> {
        let mut correlated_indices = vec![];
        self.select_items
            .iter_mut()
            .chain(self.group_by.iter_mut())
            .chain(self.where_clause.iter_mut())
            .for_each(|expr| {
                correlated_indices
                    .extend(expr.collect_correlated_indices_by_depth_and_assign_id(correlated_id));
            });

        if let Some(relation) = self.from.as_mut() {
            correlated_indices
                .extend(relation.collect_correlated_indices_by_depth_and_assign_id(correlated_id));
        }

        correlated_indices
    }
}

impl Binder {
    pub(super) fn bind_select(&mut self, select: Select) -> Result<BoundSelect> {
        // Bind FROM clause.
        let from = self.bind_vec_table_with_joins(select.from)?;

        // Bind WHERE clause.
        self.context.clause = Some(Clause::Where);
        let selection = select
            .selection
            .map(|expr| self.bind_expr(expr))
            .transpose()?;
        self.context.clause = None;

        Self::require_bool_clause(&selection, "WHERE")?;

        // Bind GROUP BY clause.
        let group_by = select
            .group_by
            .into_iter()
            .map(|expr| self.bind_expr(expr))
            .try_collect()?;

        // Bind HAVING clause.
        let having = select.having.map(|expr| self.bind_expr(expr)).transpose()?;
        Self::require_bool_clause(&having, "HAVING")?;

        // Bind SELECT clause.
        let (select_items, aliases) = self.bind_select_list(select.projection)?;

        // Store field from `ExprImpl` to support binding `field_desc` in `subquery`.
        let fields = select_items
            .iter()
            .zip_eq(aliases.iter())
            .map(|(s, a)| {
                let name = a.clone().unwrap_or_else(|| UNNAMED_COLUMN.to_string());
                self.expr_to_field(s, name)
            })
            .collect::<Result<Vec<Field>>>()?;

        Ok(BoundSelect {
            distinct: select.distinct,
            select_items,
            aliases,
            from,
            where_clause: selection,
            group_by,
            having,
            schema: Schema { fields },
        })
    }

    pub fn bind_select_list(
        &mut self,
        select_items: Vec<SelectItem>,
    ) -> Result<(Vec<ExprImpl>, Vec<Option<String>>)> {
        let mut select_list = vec![];
        let mut aliases = vec![];
        for item in select_items {
            match item {
                SelectItem::UnnamedExpr(expr) => {
                    let (select_expr, alias) = match &expr.clone() {
                        Expr::Identifier(ident) => {
                            (self.bind_expr(expr)?, Some(ident.real_value()))
                        }
                        Expr::CompoundIdentifier(idents) => (
                            self.bind_expr(expr)?,
                            idents.last().map(|ident| ident.real_value()),
                        ),
                        Expr::FieldIdentifier(field_expr, idents) => (
                            self.bind_single_field_column(*field_expr.clone(), idents)?,
                            idents.last().map(|ident| ident.real_value()),
                        ),
                        _ => (self.bind_expr(expr)?, None),
                    };
                    select_list.push(select_expr);
                    aliases.push(alias);
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    check_valid_column_name(&alias.real_value())?;

                    let expr = self.bind_expr(expr)?;
                    select_list.push(expr);
                    aliases.push(Some(alias.real_value()));
                }
                SelectItem::QualifiedWildcard(obj_name) => {
                    let table_name = &obj_name.0.last().unwrap().real_value();
                    let (mut begin, end) =
                        self.context.range_of.get(table_name).ok_or_else(|| {
                            ErrorCode::ItemNotFound(format!("relation \"{}\"", table_name))
                        })?;
                    // Here we make the assumption that the hidden columns are always at the
                    // beginning.
                    while begin < *end && self.context.columns[begin].is_hidden {
                        begin += 1;
                    }
                    let (exprs, names) =
                        Self::iter_bound_columns(self.context.columns[begin..*end].iter());
                    select_list.extend(exprs);
                    aliases.extend(names);
                }
                SelectItem::ExprQualifiedWildcard(expr, idents) => {
                    let (exprs, names) = self.bind_wildcard_field_column(expr, &idents.0)?;
                    select_list.extend(exprs);
                    aliases.extend(names);
                }
                SelectItem::Wildcard => {
                    let (exprs, names) = Self::iter_bound_columns(
                        self.context.columns[..].iter().filter(|c| !c.is_hidden),
                    );
                    select_list.extend(exprs);
                    aliases.extend(names);
                }
            }
        }
        Ok((select_list, aliases))
    }

    pub fn iter_bound_columns<'a>(
        column_binding: impl Iterator<Item = &'a ColumnBinding>,
    ) -> (Vec<ExprImpl>, Vec<Option<String>>) {
        column_binding
            .map(|c| {
                (
                    InputRef::new(c.index, c.field.data_type.clone()).into(),
                    Some(c.field.name.clone()),
                )
            })
            .unzip()
    }

    fn require_bool_clause(expr: &Option<ExprImpl>, clause: &str) -> Result<()> {
        if let Some(expr) = expr {
            let return_type = expr.return_type();
            if return_type != DataType::Boolean {
                return Err(ErrorCode::InternalError(format!(
                    "argument of {} must be boolean, not type {:?}",
                    clause, return_type
                ))
                .into());
            }
        }
        Ok(())
    }
}
