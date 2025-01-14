use std::collections::HashMap;
use std::sync::Arc;
use futures_async_stream::try_stream;
use itertools::Itertools;
use crate::catalog::TableName;
use crate::execution::executor::{BoxedExecutor, Executor};
use crate::execution::ExecutorError;
use crate::planner::operator::insert::InsertOperator;
use crate::storage::{Storage, Table};
use crate::types::ColumnId;
use crate::types::tuple::Tuple;
use crate::types::value::{DataValue, ValueRef};

pub struct Insert {
    table_name: TableName,
    input: BoxedExecutor,
    is_overwrite: bool
}

impl From<(InsertOperator, BoxedExecutor)> for Insert {
    fn from((InsertOperator { table_name, is_overwrite }, input): (InsertOperator, BoxedExecutor)) -> Self {
        Insert {
            table_name,
            input,
            is_overwrite,
        }
    }
}

impl<S: Storage> Executor<S> for Insert {
    fn execute(self, storage: &S) -> BoxedExecutor {
        self._execute(storage.clone())
    }
}

impl Insert {
    #[try_stream(boxed, ok = Tuple, error = ExecutorError)]
    pub async fn _execute<S: Storage>(self, storage: S) {
        let Insert { table_name, input, is_overwrite } = self;
        let mut primary_key_index = None;

        if let (Some(table_catalog), Some(mut table)) =
            (storage.table_catalog(&table_name).await, storage.table(&table_name).await)
        {
            #[for_await]
            for tuple in input {
                let Tuple { columns, values, .. } = tuple?;
                let primary_idx = primary_key_index.get_or_insert_with(|| {
                    columns.iter()
                        .find_position(|col| col.desc.is_primary)
                        .map(|(i, _)| i)
                        .unwrap()
                });
                let id = Some(values[*primary_idx].clone());
                let mut tuple_map: HashMap<ColumnId, ValueRef> = values
                    .into_iter()
                    .enumerate()
                    .map(|(i, value)| (columns[i].id, value))
                    .collect();
                let all_columns = table_catalog.all_columns_with_id();

                let mut tuple = Tuple {
                    id,
                    columns: Vec::with_capacity(all_columns.len()),
                    values: Vec::with_capacity(all_columns.len()),
                };

                for (col_id, col) in all_columns {
                    let value = tuple_map.remove(col_id)
                        .unwrap_or_else(|| Arc::new(DataValue::none(col.datatype())));

                    if value.is_null() && !col.nullable {
                        return Err(ExecutorError::InternalError(format!("Non-null fields do not allow null values to be passed in: {:?}", col)));
                    }

                    tuple.columns.push(col.clone());
                    tuple.values.push(value)
                }

                table.append(tuple, is_overwrite)?;
            }
            table.commit().await?;
        }
    }
}