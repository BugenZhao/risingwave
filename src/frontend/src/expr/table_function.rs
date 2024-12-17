// Copyright 2024 RisingWave Labs
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

use std::sync::{Arc, LazyLock};

use itertools::Itertools;
use risingwave_common::array::arrow::IcebergArrowConvert;
use risingwave_common::types::{DataType, ScalarImpl, StructType};
use risingwave_connector::source::iceberg::{
    extract_bucket_and_file_name, get_parquet_fields, list_s3_directory, new_s3_operator,
};
pub use risingwave_pb::expr::table_function::PbType as TableFunctionType;
use risingwave_pb::expr::PbTableFunction;
use thiserror_ext::AsReport;
use tokio::runtime::Runtime;
use tokio_postgres;
use tokio_postgres::types::Type as TokioPgType;

use super::{infer_type, Expr, ExprImpl, ExprRewriter, Literal, RwResult};
use crate::catalog::function_catalog::{FunctionCatalog, FunctionKind};
use crate::error::ErrorCode::BindError;

static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .thread_name("rw-binder-ext-query")
        .enable_all()
        .build()
        .expect("failed to build external system querying runtime")
});

/// A table function takes a row as input and returns a table. It is also known as Set-Returning
/// Function.
///
/// See also [`TableFunction`](risingwave_expr::table_function::TableFunction) trait in expr crate
/// and [`ProjectSetSelectItem`](risingwave_pb::expr::ProjectSetSelectItem).
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct TableFunction {
    pub args: Vec<ExprImpl>,
    pub return_type: DataType,
    pub function_type: TableFunctionType,
    /// Catalog of user defined table function.
    pub user_defined: Option<Arc<FunctionCatalog>>,
}

impl TableFunction {
    /// Create a `TableFunction` expr with the return type inferred from `func_type` and types of
    /// `inputs`.
    pub fn new(func_type: TableFunctionType, mut args: Vec<ExprImpl>) -> RwResult<Self> {
        let return_type = infer_type(func_type.into(), &mut args)?;
        Ok(TableFunction {
            args,
            return_type,
            function_type: func_type,
            user_defined: None,
        })
    }

    /// Create a user-defined `TableFunction`.
    pub fn new_user_defined(catalog: Arc<FunctionCatalog>, args: Vec<ExprImpl>) -> Self {
        let FunctionKind::Table = &catalog.kind else {
            panic!("not a table function");
        };
        TableFunction {
            args,
            return_type: catalog.return_type.clone(),
            function_type: TableFunctionType::UserDefined,
            user_defined: Some(catalog),
        }
    }

    /// A special table function which would be transformed into `LogicalFileScan` by `TableFunctionToFileScanRule` in the optimizer.
    /// select * from `file_scan`('parquet', 's3', region, ak, sk, location)
    pub fn new_file_scan(mut args: Vec<ExprImpl>) -> RwResult<Self> {
        let return_type = {
            // arguments:
            // file format e.g. parquet
            // storage type e.g. s3
            // s3 region
            // s3 access key
            // s3 secret key
            // file location
            if args.len() != 6 {
                return Err(BindError("file_scan function only accepts 6 arguments: file_scan('parquet', 's3', s3 region, s3 access key, s3 secret key, file location)".to_string()).into());
            }
            let mut eval_args: Vec<String> = vec![];
            for arg in &args {
                if arg.return_type() != DataType::Varchar {
                    return Err(BindError(
                        "file_scan function only accepts string arguments".to_string(),
                    )
                    .into());
                }
                match arg.try_fold_const() {
                    Some(Ok(value)) => {
                        if value.is_none() {
                            return Err(BindError(
                                "file_scan function does not accept null arguments".to_string(),
                            )
                            .into());
                        }
                        match value {
                            Some(ScalarImpl::Utf8(s)) => {
                                eval_args.push(s.to_string());
                            }
                            _ => {
                                return Err(BindError(
                                    "file_scan function only accepts string arguments".to_string(),
                                )
                                .into())
                            }
                        }
                    }
                    Some(Err(err)) => {
                        return Err(err);
                    }
                    None => {
                        return Err(BindError(
                            "file_scan function only accepts constant arguments".to_string(),
                        )
                        .into());
                    }
                }
            }
            if !"parquet".eq_ignore_ascii_case(&eval_args[0]) {
                return Err(BindError(
                    "file_scan function only accepts 'parquet' as file format".to_string(),
                )
                .into());
            }

            if !"s3".eq_ignore_ascii_case(&eval_args[1]) {
                return Err(BindError(
                    "file_scan function only accepts 's3' as storage type".to_string(),
                )
                .into());
            }

            #[cfg(madsim)]
            return Err(crate::error::ErrorCode::BindError(
                "file_scan can't be used in the madsim mode".to_string(),
            )
            .into());

            #[cfg(not(madsim))]
            {
                let files = if eval_args[5].ends_with('/') {
                    let files = tokio::task::block_in_place(|| {
                        RUNTIME.block_on(async {
                            let files = list_s3_directory(
                                eval_args[2].clone(),
                                eval_args[3].clone(),
                                eval_args[4].clone(),
                                eval_args[5].clone(),
                            )
                            .await?;

                            Ok::<Vec<String>, anyhow::Error>(files)
                        })
                    })?;

                    if files.is_empty() {
                        return Err(BindError(
                            "file_scan function only accepts non-empty directory".to_string(),
                        )
                        .into());
                    }

                    Some(files)
                } else {
                    None
                };

                let schema = tokio::task::block_in_place(|| {
                    RUNTIME.block_on(async {
                        let location = match files.as_ref() {
                            Some(files) => files[0].clone(),
                            None => eval_args[5].clone(),
                        };
                        let (bucket, file_name) = extract_bucket_and_file_name(&location)?;
                        let op = new_s3_operator(
                            eval_args[2].clone(),
                            eval_args[3].clone(),
                            eval_args[4].clone(),
                            bucket.clone(),
                        )?;

                        let fields = get_parquet_fields(op, file_name).await?;

                        let mut rw_types = vec![];
                        for field in &fields {
                            rw_types.push((
                                field.name().to_string(),
                                IcebergArrowConvert.type_from_field(field)?,
                            ));
                        }

                        Ok::<risingwave_common::types::DataType, anyhow::Error>(DataType::Struct(
                            StructType::new(rw_types),
                        ))
                    })
                })?;

                if let Some(files) = files {
                    // if the file location is a directory, we need to remove the last argument and add all files in the directory as arguments
                    args.remove(5);
                    for file in files {
                        args.push(ExprImpl::Literal(Box::new(Literal::new(
                            Some(ScalarImpl::Utf8(file.into())),
                            DataType::Varchar,
                        ))));
                    }
                }

                schema
            }
        };

        Ok(TableFunction {
            args,
            return_type,
            function_type: TableFunctionType::FileScan,
            user_defined: None,
        })
    }

    pub fn new_postgres_query(args: Vec<ExprImpl>) -> RwResult<Self> {
        let args = {
            if args.len() != 6 {
                return Err(BindError("postgres_query function only accepts 6 arguments: postgres_query(hostname varchar, port varchar, username varchar, password varchar, database_name varchar, postgres_query varchar)".to_string()).into());
            }
            let mut cast_args = Vec::with_capacity(6);
            for arg in args {
                let arg = arg.cast_implicit(DataType::Varchar)?;
                cast_args.push(arg);
            }
            cast_args
        };
        let evaled_args = {
            let mut evaled_args: Vec<String> = Vec::with_capacity(6);
            for arg in &args {
                match arg.try_fold_const() {
                    Some(Ok(value)) => {
                        let Some(scalar) = value else {
                            return Err(BindError(
                                "postgres_query function does not accept null arguments"
                                    .to_string(),
                            )
                            .into());
                        };
                        evaled_args.push(scalar.into_utf8().into());
                    }
                    Some(Err(err)) => {
                        return Err(err);
                    }
                    None => {
                        return Err(BindError(
                            "postgres_query function only accepts constant arguments".to_string(),
                        )
                        .into());
                    }
                }
            }
            evaled_args
        };

        #[cfg(madsim)]
        {
            return Err(crate::error::ErrorCode::BindError(
                "postgres_query can't be used in the madsim mode".to_string(),
            )
            .into());
        }

        #[cfg(not(madsim))]
        {
            let schema = tokio::task::block_in_place(|| {
                RUNTIME.block_on(async {
                    let (client, connection) = tokio_postgres::connect(
                        format!(
                            "host={} port={} user={} password={} dbname={}",
                            evaled_args[0],
                            evaled_args[1],
                            evaled_args[2],
                            evaled_args[3],
                            evaled_args[4]
                        )
                        .as_str(),
                        tokio_postgres::NoTls,
                    )
                    .await?;

                    tokio::spawn(async move {
                        if let Err(e) = connection.await {
                            tracing::error!(
                                "postgres_query_executor: connection error: {:?}",
                                e.as_report()
                            );
                        }
                    });

                    let statement = client.prepare(evaled_args[5].as_str()).await?;

                    let mut rw_types = vec![];
                    for column in statement.columns() {
                        let name = column.name().to_string();
                        let data_type = match *column.type_() {
                            TokioPgType::BOOL => DataType::Boolean,
                            TokioPgType::INT2 => DataType::Int16,
                            TokioPgType::INT4 => DataType::Int32,
                            TokioPgType::INT8 => DataType::Int64,
                            TokioPgType::FLOAT4 => DataType::Float32,
                            TokioPgType::FLOAT8 => DataType::Float64,
                            TokioPgType::NUMERIC => DataType::Decimal,
                            TokioPgType::DATE => DataType::Date,
                            TokioPgType::TIME => DataType::Time,
                            TokioPgType::TIMESTAMP => DataType::Timestamp,
                            TokioPgType::TIMESTAMPTZ => DataType::Timestamptz,
                            TokioPgType::TEXT | TokioPgType::VARCHAR => DataType::Varchar,
                            TokioPgType::INTERVAL => DataType::Interval,
                            TokioPgType::JSONB => DataType::Jsonb,
                            TokioPgType::BYTEA => DataType::Bytea,
                            _ => {
                                return Err(crate::error::ErrorCode::BindError(
                                    format!("unsupported column type: {}", column.type_())
                                        .to_string(),
                                )
                                .into());
                            }
                        };
                        rw_types.push((name, data_type));
                    }
                    Ok::<risingwave_common::types::DataType, anyhow::Error>(DataType::Struct(
                        StructType::new(rw_types),
                    ))
                })
            })?;

            Ok(TableFunction {
                args,
                return_type: schema,
                function_type: TableFunctionType::PostgresQuery,
                user_defined: None,
            })
        }
    }

    pub fn to_protobuf(&self) -> PbTableFunction {
        PbTableFunction {
            function_type: self.function_type as i32,
            args: self.args.iter().map(|c| c.to_expr_proto()).collect_vec(),
            return_type: Some(self.return_type.to_protobuf()),
            udf: self.user_defined.as_ref().map(|c| c.as_ref().into()),
        }
    }

    /// Get the name of the table function.
    pub fn name(&self) -> String {
        match self.function_type {
            TableFunctionType::UserDefined => self.user_defined.as_ref().unwrap().name.clone(),
            t => t.as_str_name().to_lowercase(),
        }
    }

    pub fn rewrite(self, rewriter: &mut impl ExprRewriter) -> Self {
        Self {
            args: self
                .args
                .into_iter()
                .map(|e| rewriter.rewrite_expr(e))
                .collect(),
            ..self
        }
    }
}

impl std::fmt::Debug for TableFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if f.alternate() {
            f.debug_struct("FunctionCall")
                .field("function_type", &self.function_type)
                .field("return_type", &self.return_type)
                .field("args", &self.args)
                .finish()
        } else {
            let func_name = format!("{:?}", self.function_type);
            let mut builder = f.debug_tuple(&func_name);
            self.args.iter().for_each(|child| {
                builder.field(child);
            });
            builder.finish()
        }
    }
}

impl Expr for TableFunction {
    fn return_type(&self) -> DataType {
        self.return_type.clone()
    }

    fn to_expr_proto(&self) -> risingwave_pb::expr::ExprNode {
        unreachable!("Table function should not be converted to ExprNode")
    }
}
