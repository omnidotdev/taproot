use std::collections::HashSet;
use std::sync::Arc;

use async_graphql::dynamic::{
    Enum, EnumItem, Field, FieldFuture, FieldValue, InputValue, Object, Scalar, Schema, TypeRef,
};

use crate::db::{Database, RlsContext};
use crate::error::{Error, Result};
use crate::introspect::{ColumnInfo, IntrospectedSchema, TableInfo, UniqueConstraint};
use crate::planner::{condition_to_filter, execute_plan, execute_plan_count, order_to_exprs, FilterExpr, PlanNode};
use crate::plugin::Plugin;

use super::federation;
use super::loader::{LoaderKey, LoaderRegistry, normalize_key};
use super::scalars::map_udt_to_graphql;
use super::types::SchemaOptions;
use super::{connection, filters, mutations, ordering};

/// Schema-level storage for plugins, accessible by resolvers via `ctx.data()`
#[derive(Clone)]
struct PluginStore(Arc<Vec<Arc<dyn Plugin>>>);

/// Apply all plugin `on_plan` hooks to a plan node
fn apply_on_plan_hooks(
    ctx: &async_graphql::dynamic::ResolverContext<'_>,
    plan: &mut PlanNode,
) {
    if let Ok(store) = ctx.data::<PluginStore>() {
        apply_hooks_recursive(plan, &store.0);
    }
}

/// Recursively apply plugin hooks to a plan tree
fn apply_hooks_recursive(plan: &mut PlanNode, plugins: &[Arc<dyn Plugin>]) {
    for plugin in plugins {
        plugin.on_plan(plan);
    }

    // Recurse into child selections
    match plan {
        PlanNode::Scan { selections, .. }
        | PlanNode::LateralJoin { selections, .. }
        | PlanNode::BatchLookup { selections, .. }
        | PlanNode::Mutation {
            returning: selections,
            ..
        } => {
            for child in selections.iter_mut() {
                apply_hooks_recursive(child, plugins);
            }
        }
        PlanNode::Column { .. } => {}
    }
}

/// Build a dynamic `async_graphql` schema from introspected database metadata.
///
/// Each table becomes a `GraphQL` object type, with singular and list root query
/// fields generated automatically based on column types and primary keys.
/// Foreign keys produce belongs-to and has-many relationship fields.
///
/// # Errors
/// Returns an error if the schema fails validation.
#[allow(clippy::too_many_lines)]
pub fn build_schema(
    schemas: &[IntrospectedSchema],
    db: &Arc<dyn Database>,
    options: &SchemaOptions,
    plugins: &[Arc<dyn Plugin>],
) -> Result<Schema> {
    // Clone schemas so plugins can mutate them
    let mut schemas = schemas.to_vec();

    // Run plugin on_introspect hooks
    for plugin in plugins {
        for schema in &mut schemas {
            tracing::debug!(plugin = plugin.name(), schema = %schema.name, "running on_introspect");
            plugin.on_introspect(schema);
        }
    }

    // Run plugin filter_table hooks
    for schema in &mut schemas {
        schema.tables.retain(|table| {
            plugins.iter().all(|p| p.filter_table(table))
        });
    }

    // Store plugins for resolver-time access (on_plan hooks)
    let plugin_store = PluginStore(Arc::new(plugins.to_vec()));

    let mut query = Object::new("Query");

    // Collect all enum type names for lookup during column mapping
    let mut enum_names: HashSet<String> = HashSet::new();
    let mut mutation = Object::new("Mutation");
    let mut has_mutations = false;
    let mut schema_builder = Schema::build("Query", Some("Mutation"), None);
    schema_builder = schema_builder.data(plugin_store);

    let loader_registry = LoaderRegistry::new(Arc::clone(db), None);
    schema_builder = schema_builder.data(loader_registry);

    // Collect all tables across schemas for relationship resolution
    let all_tables: Vec<&TableInfo> = schemas.iter().flat_map(|s| s.tables.iter()).collect();

    // Federation entity map for resolving _entities queries
    let mut entity_map = federation::EntityMap::new();

    // Register enum types
    for introspected in &schemas {
        for enum_info in &introspected.enums {
            let gql_name = to_pascal_case(&enum_info.name);
            enum_names.insert(enum_info.name.clone());

            let mut gql_enum = Enum::new(&gql_name);
            for value in &enum_info.values {
                gql_enum = gql_enum.item(EnumItem::new(value));
            }

            schema_builder = schema_builder.register(gql_enum);
        }
    }

    // Register all custom scalars upfront (condition types reference them)
    for scalar_name in &["UUID", "DateTime", "Date", "JSON", "BigInt", "BigDecimal"] {
        schema_builder = schema_builder.register(Scalar::new(*scalar_name));
    }

    // Register scalar condition types (StringCondition, IntCondition, etc)
    for condition_input in filters::build_scalar_conditions() {
        schema_builder = schema_builder.register(condition_input);
    }

    // Register PageInfo (shared across all connections)
    schema_builder = schema_builder.register(connection::build_page_info_type());

    // Register custom scalars and build object types
    for introspected in &schemas {
        for table in &introspected.tables {
            let type_name = to_pascal_case(&singularize(&table.name));

            // Build the object type with column fields and relationship fields
            let object =
                build_object_type(&type_name, table, &all_tables, &enum_names, db, options);
            schema_builder = schema_builder.register(object);

            // Register federation entity mapping
            if options.federation
                && let Some(pk) = &table.primary_key
            {
                let pk_cols: Vec<(String, String)> = pk
                    .columns
                    .iter()
                    .map(|c| (to_camel_case(c), c.clone()))
                    .collect();
                entity_map.register(
                    &type_name,
                    federation::EntityInfo {
                        schema: introspected.name.clone(),
                        table: table.name.clone(),
                        pk_columns: pk_cols,
                    },
                );
            }

            // Register per-table condition input type
            let condition_input = filters::build_table_condition(table, &type_name);
            schema_builder = schema_builder.register(condition_input);

            // Register per-table order-by enum
            let order_by_enum = ordering::build_table_order_by(table, &type_name);
            schema_builder = schema_builder.register(order_by_enum);

            // Register connection and edge types
            let connection_type = connection::build_connection_type(&type_name);
            schema_builder = schema_builder.register(connection_type);

            let edge_type = connection::build_edge_type(&type_name);
            schema_builder = schema_builder.register(edge_type);

            // Singular query (by single-column primary key)
            if let Some(pk) = &table.primary_key
                && pk.columns.len() == 1
            {
                let pk_col_name = &pk.columns[0];
                if let Some(pk_col) = table.columns.iter().find(|c| &c.name == pk_col_name) {
                    let field =
                        build_singular_query(table, &type_name, pk_col, &enum_names, db);
                    query = query.field(field);
                }
            }

            // Unique constraint queries
            for uc in &table.unique_constraints {
                // Skip if this constraint matches the primary key
                if table
                    .primary_key
                    .as_ref()
                    .is_some_and(|pk| pk.columns == uc.columns)
                {
                    continue;
                }

                let field = build_unique_query(table, &type_name, uc, &enum_names, db);
                query = query.field(field);
            }

            // List query (returns connection) — pass all tables so the
            // recursive lateral builder can discover grandchild relationships
            let all_tables_owned: Vec<TableInfo> = all_tables.iter().map(|t| (*t).clone()).collect();
            let list_field =
                build_list_query(table, &type_name, options, db, &all_tables_owned);
            query = query.field(list_field);

            // Mutations for non-view tables with single-column primary keys
            if !table.is_view
                && let Some(pk) = &table.primary_key
                && pk.columns.len() == 1
            {
                let pk_col_name = &pk.columns[0];
                if let Some(pk_col) =
                    table.columns.iter().find(|c| &c.name == pk_col_name)
                {
                    has_mutations = true;
                    let table_field_name = to_camel_case(&singularize(&table.name));

                    let (sb, mt) = register_mutation_types(
                        schema_builder,
                        mutation,
                        table,
                        &type_name,
                        &table_field_name,
                        pk_col,
                        &enum_names,
                        db,
                    );
                    schema_builder = sb;
                    mutation = mt;
                }
            }
        }
    }

    schema_builder = schema_builder.register(query);

    if !has_mutations {
        // Schema requires at least one field on the mutation type
        mutation = mutation.field(Field::new(
            "_empty",
            TypeRef::named(TypeRef::STRING),
            |_| FieldFuture::new(async { Ok(None::<FieldValue<'_>>) }),
        ));
    }
    schema_builder = schema_builder.register(mutation);

    if options.federation {
        schema_builder = schema_builder.enable_federation();

        let entity_map = Arc::new(entity_map);
        let db_for_entities = Arc::clone(db);

        schema_builder = schema_builder.entity_resolver(move |ctx| {
            let entity_map = Arc::clone(&entity_map);
            let db = Arc::clone(&db_for_entities);

            FieldFuture::new(async move {
                let rls = ctx.data::<RlsContext>().ok();
                let representations = ctx.args.try_get("representations")?.list()?;
                let mut values = Vec::new();

                for item in representations.iter() {
                    let item = item.object()?;
                    let typename = item
                        .try_get("__typename")
                        .and_then(|v| v.string())
                        .map_err(|_| {
                            async_graphql::Error::new("missing __typename in representation")
                        })?;

                    let Some(entity_info) = entity_map.get(typename) else {
                        return Err(async_graphql::Error::new(format!(
                            "unknown entity type: {typename}"
                        )));
                    };

                    // Build filter from key fields
                    let mut filter_parts = Vec::new();
                    for (arg_name, col_name) in &entity_info.pk_columns {
                        let key_value = item.try_get(arg_name).map_err(|_| {
                            async_graphql::Error::new(format!(
                                "missing key field {arg_name} in representation for {typename}"
                            ))
                        })?;
                        let json_value = gql_value_to_json(key_value.as_value());
                        filter_parts.push(FilterExpr::Eq {
                            column: col_name.clone(),
                            value: json_value,
                        });
                    }

                    let filter = if filter_parts.len() == 1 {
                        filter_parts.remove(0)
                    } else {
                        FilterExpr::And(filter_parts)
                    };

                    let mut plan = PlanNode::Scan {
                        schema: entity_info.schema.clone(),
                        table: entity_info.table.clone(),
                        filter: Some(filter),
                        order_by: None,
                        limit: Some(1),
                        offset: None,
                        selections: vec![],
                    };

                    // Apply on_plan hooks
                    if let Ok(store) = ctx.data::<PluginStore>() {
                        for plugin in store.0.iter() {
                            plugin.on_plan(&mut plan);
                        }
                    }

                    let rows = execute_plan(&plan, &db, rls).await.map_err(|e| {
                        async_graphql::Error::new(format!("database error: {e}"))
                    })?;

                    if let Some(row) = rows.into_iter().next() {
                        let obj = row.get("row_to_json").cloned().unwrap_or(row);
                        values.push(
                            FieldValue::value(serde_json_to_gql_value(&obj))
                                .with_type(typename.to_string()),
                        );
                    } else {
                        values.push(FieldValue::NULL.with_type(typename.to_string()));
                    }
                }

                Ok(Some(FieldValue::list(values)))
            })
        });
    }

    schema_builder
        .finish()
        .map_err(|e| Error::Schema(e.to_string()))
}

/// Build a `GraphQL` object type from a table's columns, including relationship
/// fields derived from foreign keys.
///
/// Belongs-to fields are added for FKs on this table (e.g. `userByAuthorId`).
/// Has-many fields are added for FKs on other tables pointing here (e.g.
/// `postsByAuthorId`).
#[allow(clippy::too_many_lines)]
fn build_object_type(
    type_name: &str,
    table: &TableInfo,
    all_tables: &[&TableInfo],
    enum_names: &HashSet<String>,
    db: &Arc<dyn Database>,
    options: &SchemaOptions,
) -> Object {
    let mut object = Object::new(type_name);

    // Column fields
    for col in &table.columns {
        let field_name = to_camel_case(&col.name);
        let type_ref = column_to_type_ref(col, enum_names);

        let col_name = col.name.clone();
        let mut field = Field::new(&field_name, type_ref, move |ctx| {
            let col_name = col_name.clone();
            FieldFuture::new(async move {
                let parent = ctx.parent_value.try_to_value()?;

                let value = if let async_graphql::Value::Object(map) = parent {
                    map.get(col_name.as_str()).cloned()
                } else {
                    None
                };

                match value {
                    Some(async_graphql::Value::Null) | None => Ok(None),
                    Some(v) => Ok(Some(FieldValue::value(v))),
                }
            })
        });

        // @deprecated tag
        if let Some(reason) = col.tags.get("deprecated") {
            let msg = if reason.is_empty() {
                "Deprecated".to_string()
            } else {
                reason.clone()
            };
            field = field.deprecation(Some(&msg));
        }

        object = object.field(field);
    }

    // Belongs-to fields (FK on this table pointing to another table)
    for fk in &table.foreign_keys {
        // Only support single-column FKs
        if fk.columns.len() != 1 || fk.foreign_columns.len() != 1 {
            continue;
        }

        // Skip if the foreign table was filtered out
        if !all_tables.iter().any(|t| t.name == fk.foreign_table && t.schema == fk.foreign_schema) {
            continue;
        }

        let foreign_type_name = to_pascal_case(&singularize(&fk.foreign_table));
        let field_name = format!(
            "{}By{}",
            to_camel_case(&singularize(&fk.foreign_table)),
            to_pascal_case(&fk.columns[0]),
        );

        let fk_col_name = fk.columns[0].clone();
        let foreign_schema = fk.foreign_schema.clone();
        let foreign_table = fk.foreign_table.clone();
        let foreign_col = fk.foreign_columns[0].clone();

        let field = Field::new(
            &field_name,
            TypeRef::named(&foreign_type_name),
            move |ctx| {
                let foreign_schema = foreign_schema.clone();
                let foreign_table = foreign_table.clone();
                let foreign_col = foreign_col.clone();
                let fk_col_name = fk_col_name.clone();

                FieldFuture::new(async move {
                    let parent = ctx.parent_value.try_to_value()?;
                    let fk_value = if let async_graphql::Value::Object(map) = parent {
                        map.get(fk_col_name.as_str()).cloned()
                    } else {
                        None
                    };

                    let Some(fk_value) = fk_value else {
                        return Ok(None);
                    };

                    if fk_value == async_graphql::Value::Null {
                        return Ok(None);
                    }

                    let fk_json = gql_value_to_json(&fk_value);
                    let key = normalize_key(&fk_json);

                    let loader_key =
                        LoaderKey::new(&foreign_schema, &foreign_table, &foreign_col);

                    let registry = ctx.data::<LoaderRegistry>().map_err(|e| {
                        async_graphql::Error::new(format!("loader registry not found: {e:?}"))
                    })?;

                    let loader = registry.get_belongs_to(&loader_key);
                    let row = loader.load_one(key).await.map_err(|e| {
                        async_graphql::Error::new(format!("database error: {e:?}"))
                    })?;

                    Ok(row.map(|r| FieldValue::value(serde_json_to_gql_value(&r))))
                })
            },
        );

        object = object.field(field);
    }

    // Has-many fields (FK on other tables pointing to this table)
    for child_table in all_tables {
        for fk in &child_table.foreign_keys {
            // Only match FKs pointing to this table in the same schema
            if fk.foreign_table != table.name || fk.foreign_schema != table.schema {
                continue;
            }

            // Only support single-column FKs
            if fk.columns.len() != 1 || fk.foreign_columns.len() != 1 {
                continue;
            }

            let child_type_name = to_pascal_case(&singularize(&child_table.name));
            let connection_type_name = format!("{child_type_name}Connection");
            let field_name = format!(
                "{}By{}",
                pluralize(&to_camel_case(&singularize(&child_table.name))),
                to_pascal_case(&fk.columns[0]),
            );

            let child_schema = child_table.schema.clone();
            let child_table_name = child_table.name.clone();
            let child_fk_col = fk.columns[0].clone();
            let parent_pk_col = fk.foreign_columns[0].clone();
            let default_page_size = options.default_page_size;
            let max_page_size = options.max_page_size;
            let db = Arc::clone(db);

            let field = Field::new(
                &field_name,
                TypeRef::named_nn(&connection_type_name),
                move |ctx| {
                    let db = Arc::clone(&db);
                    let child_schema = child_schema.clone();
                    let child_table_name = child_table_name.clone();
                    let child_fk_col = child_fk_col.clone();
                    let parent_pk_col = parent_pk_col.clone();

                    FieldFuture::new(async move {
                        let rls = ctx.data::<RlsContext>().ok();
                        let parent = ctx.parent_value.try_to_value()?;

                        // Check for lateral join data first
                        let lateral_key = format!("__lateral_{child_table_name}");
                        if let async_graphql::Value::Object(map) = parent
                            && let Some(lateral_value) = map.get(lateral_key.as_str())
                        {
                            let lateral_json = gql_value_to_json(lateral_value);
                            let all_rows: Vec<serde_json::Value> =
                                if let serde_json::Value::Array(arr) = lateral_json {
                                    arr
                                } else {
                                    vec![]
                                };

                            // Apply client-side pagination
                            #[allow(
                                clippy::cast_sign_loss,
                                clippy::cast_possible_truncation
                            )]
                            let first = ctx
                                .args
                                .try_get("first")
                                .ok()
                                .and_then(|v| v.i64().ok())
                                .map(|n| n.max(0) as usize);

                            let after = ctx
                                .args
                                .try_get("after")
                                .ok()
                                .and_then(|v| v.string().ok().map(str::to_owned));

                            let offset = after
                                .as_deref()
                                .and_then(connection::decode_cursor)
                                .map_or(0, |n| n + 1);
                            let limit =
                                first.unwrap_or(default_page_size).min(max_page_size);

                            #[allow(clippy::cast_possible_wrap)]
                            let total_count = all_rows.len() as i64;
                            let rows: Vec<serde_json::Value> = all_rows
                                .into_iter()
                                .skip(offset)
                                .take(limit)
                                .collect();

                            let response = connection::build_connection_response(
                                rows,
                                offset,
                                total_count,
                            );

                            return Ok(Some(FieldValue::value(serde_json_to_gql_value(
                                &response,
                            ))));
                        }

                        let pk_value = if let async_graphql::Value::Object(map) = parent {
                            map.get(parent_pk_col.as_str()).cloned()
                        } else {
                            None
                        };

                        let Some(pk_value) = pk_value else {
                            let empty = connection::build_connection_response(vec![], 0, 0);
                            return Ok(Some(FieldValue::value(serde_json_to_gql_value(&empty))));
                        };

                        let pk_json = gql_value_to_json(&pk_value);

                        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                        let first = ctx
                            .args
                            .try_get("first")
                            .ok()
                            .and_then(|v| v.i64().ok())
                            .map(|n| n.max(0) as usize);

                        let after = ctx
                            .args
                            .try_get("after")
                            .ok()
                            .and_then(|v| v.string().ok().map(str::to_owned));

                        let offset = after
                            .as_deref()
                            .and_then(connection::decode_cursor)
                            .map_or(0, |n| n + 1);
                        let limit = first.unwrap_or(default_page_size).min(max_page_size);

                        let filter = FilterExpr::Eq {
                            column: child_fk_col.clone(),
                            value: pk_json,
                        };

                        #[allow(clippy::cast_possible_wrap)]
                        let mut data_plan = PlanNode::Scan {
                            schema: child_schema.clone(),
                            table: child_table_name.clone(),
                            filter: Some(filter.clone()),
                            order_by: None,
                            limit: Some(limit as i64),
                            offset: Some(offset as i64),
                            selections: vec![],
                        };

                        apply_on_plan_hooks(&ctx, &mut data_plan);

                        let mut count_plan = PlanNode::Scan {
                            schema: child_schema.clone(),
                            table: child_table_name.clone(),
                            filter: Some(filter),
                            order_by: None,
                            limit: None,
                            offset: None,
                            selections: vec![],
                        };

                        apply_on_plan_hooks(&ctx, &mut count_plan);

                        let total_count = execute_plan_count(&count_plan, &db, rls)
                            .await
                            .map_err(|e| {
                                async_graphql::Error::new(format!("database error: {e}"))
                            })?;

                        let rows = execute_plan(&data_plan, &db, rls)
                            .await
                            .map_err(|e| {
                                async_graphql::Error::new(format!("database error: {e}"))
                            })?;

                        let rows: Vec<serde_json::Value> = rows
                            .iter()
                            .map(|row| {
                                row.get("row_to_json")
                                    .cloned()
                                    .unwrap_or_else(|| row.clone())
                            })
                            .collect();

                        let response =
                            connection::build_connection_response(rows, offset, total_count);

                        Ok(Some(FieldValue::value(serde_json_to_gql_value(&response))))
                    })
                },
            )
            .argument(InputValue::new("first", TypeRef::named(TypeRef::INT)))
            .argument(InputValue::new("after", TypeRef::named(TypeRef::STRING)));

            object = object.field(field);
        }
    }

    // Mark entity types with @key for federation
    if options.federation
        && let Some(pk) = &table.primary_key
    {
        let key_fields: Vec<String> = pk.columns.iter().map(|c| to_camel_case(c)).collect();
        object = object.key(key_fields.join(" "));
    }

    object
}

/// Build a singular query field: `user(id: UUID!): User`
fn build_singular_query(
    table: &crate::introspect::TableInfo,
    type_name: &str,
    pk_col: &ColumnInfo,
    enum_names: &HashSet<String>,
    db: &Arc<dyn Database>,
) -> Field {
    let field_name = to_camel_case(&singularize(&table.name));
    let (gql_type, _is_array) = resolve_column_type(pk_col, enum_names);
    let pk_arg_type = TypeRef::named_nn(gql_type);
    let pk_arg_name = to_camel_case(&pk_col.name);

    let schema_name = table.schema.clone();
    let table_name = table.name.clone();
    let pk_col_name = pk_col.name.clone();
    let db = Arc::clone(db);
    let arg_name = pk_arg_name.clone();

    Field::new(
        field_name,
        TypeRef::named(type_name),
        move |ctx| {
            let db = Arc::clone(&db);
            let schema_name = schema_name.clone();
            let table_name = table_name.clone();
            let pk_col_name = pk_col_name.clone();
            let arg_name = arg_name.clone();

            FieldFuture::new(async move {
                let rls = ctx.data::<RlsContext>().ok();
                let pk_value = ctx.args.try_get(&arg_name)?;
                let pk_json = value_accessor_to_json(&pk_value);

                let mut plan = PlanNode::Scan {
                    schema: schema_name.clone(),
                    table: table_name.clone(),
                    filter: Some(FilterExpr::Eq {
                        column: pk_col_name.clone(),
                        value: pk_json,
                    }),
                    order_by: None,
                    limit: Some(1),
                    offset: None,
                    selections: vec![],
                };

                apply_on_plan_hooks(&ctx, &mut plan);

                let rows = execute_plan(&plan, &db, rls).await.map_err(|e| {
                    async_graphql::Error::new(format!("database error: {e}"))
                })?;

                Ok(rows
                    .into_iter()
                    .next()
                    .map(|row| row_json_to_field_value(&row)))
            })
        },
    )
    .argument(InputValue::new(pk_arg_name, pk_arg_type))
}

/// Build a singular query field for a unique constraint.
///
/// For example, a UNIQUE on `users.email` produces `userByEmail(email: String!): User`.
/// Composite unique constraints produce multiple arguments:
/// `postBySlugAndLocale(slug: String!, locale: String!): Post`.
fn build_unique_query(
    table: &TableInfo,
    type_name: &str,
    constraint: &UniqueConstraint,
    enum_names: &HashSet<String>,
    db: &Arc<dyn Database>,
) -> Field {
    // Field name: {camelTable}By{PascalCol1}{PascalCol2}...
    let suffix: String = constraint.columns.iter().map(|c| to_pascal_case(c)).collect();
    let field_name = format!("{}By{suffix}", to_camel_case(&singularize(&table.name)));

    // Resolve column metadata for each unique constraint column
    let col_infos: Vec<(&ColumnInfo, String)> = constraint
        .columns
        .iter()
        .filter_map(|col_name| {
            table
                .columns
                .iter()
                .find(|c| &c.name == col_name)
                .map(|col| {
                    let (gql_type, _) = resolve_column_type(col, enum_names);
                    (col, gql_type)
                })
        })
        .collect();

    let schema_name = table.schema.clone();
    let table_name = table.name.clone();
    let constraint_columns: Vec<String> = constraint.columns.clone();
    let arg_names: Vec<String> = constraint.columns.iter().map(|c| to_camel_case(c)).collect();
    let db = Arc::clone(db);

    let mut field = Field::new(field_name, TypeRef::named(type_name), move |ctx| {
        let db = Arc::clone(&db);
        let schema_name = schema_name.clone();
        let table_name = table_name.clone();
        let constraint_columns = constraint_columns.clone();
        let arg_names = arg_names.clone();

        FieldFuture::new(async move {
            let rls = ctx.data::<RlsContext>().ok();
            let mut filter_parts = Vec::with_capacity(constraint_columns.len());
            for (col_name, arg_name) in constraint_columns.iter().zip(&arg_names) {
                let arg_value = ctx.args.try_get(arg_name)?;
                let json_value = value_accessor_to_json(&arg_value);
                filter_parts.push(FilterExpr::Eq {
                    column: col_name.clone(),
                    value: json_value,
                });
            }

            let filter = if filter_parts.len() == 1 {
                filter_parts.remove(0)
            } else {
                FilterExpr::And(filter_parts)
            };

            let mut plan = PlanNode::Scan {
                schema: schema_name.clone(),
                table: table_name.clone(),
                filter: Some(filter),
                order_by: None,
                limit: Some(1),
                offset: None,
                selections: vec![],
            };

            apply_on_plan_hooks(&ctx, &mut plan);

            let rows = execute_plan(&plan, &db, rls).await.map_err(|e| {
                async_graphql::Error::new(format!("database error: {e}"))
            })?;

            Ok(rows
                .into_iter()
                .next()
                .map(|row| row_json_to_field_value(&row)))
        })
    });

    // Add each unique constraint column as a required argument
    for (col, gql_type) in &col_infos {
        let arg_name = to_camel_case(&col.name);
        field = field.argument(InputValue::new(arg_name, TypeRef::named_nn(gql_type.as_str())));
    }

    field
}

/// Build lateral join selections for a table's has-many relationships.
/// Recurses to populate grandchild lateral joins.
fn build_lateral_selections(
    parent_table: &TableInfo,
    all_tables: &[TableInfo],
    visited: &mut HashSet<(String, String)>,
) -> Vec<PlanNode> {
    // Mark this table as visited to prevent cycles
    let key = (parent_table.schema.clone(), parent_table.name.clone());
    if !visited.insert(key.clone()) {
        return vec![];
    }

    let mut selections = Vec::new();

    for child_table in all_tables {
        for fk in &child_table.foreign_keys {
            if fk.foreign_table != parent_table.name
                || fk.foreign_schema != parent_table.schema
                || fk.columns.len() != 1
                || fk.foreign_columns.len() != 1
            {
                continue;
            }

            // Recurse: find grandchild laterals for this child table
            let child_selections = build_lateral_selections(child_table, all_tables, visited);

            selections.push(PlanNode::LateralJoin {
                parent_key: fk.foreign_columns[0].clone(),
                child_schema: child_table.schema.clone(),
                child_table: child_table.name.clone(),
                child_key: fk.columns[0].clone(),
                filter: None,
                order_by: None,
                limit: None,
                offset: None,
                selections: child_selections,
            });
        }
    }

    // Unmark so other paths can visit this table
    visited.remove(&key);

    selections
}

/// Build a list query field that returns a Relay-style connection.
///
/// Accepts `condition`, `orderBy`, `first`, and `after` arguments and returns
/// a `{Type}Connection` with edges, nodes, page info, and total count.
#[allow(clippy::too_many_lines)]
fn build_list_query(
    table: &TableInfo,
    type_name: &str,
    options: &SchemaOptions,
    db: &Arc<dyn Database>,
    all_tables: &[TableInfo],
) -> Field {
    let field_name = pluralize(&to_camel_case(&singularize(&table.name)));
    let connection_type_name = format!("{type_name}Connection");
    let condition_type_name = format!("{type_name}Condition");
    let order_by_type_name = format!("{type_name}OrderBy");

    let schema_name = table.schema.clone();
    let table_name = table.name.clone();
    let table_info = table.clone();
    let default_page_size = options.default_page_size;
    let max_page_size = options.max_page_size;
    let db = Arc::clone(db);
    let all_tables_for_lateral: Vec<TableInfo> = all_tables.to_vec();

    Field::new(
        field_name,
        TypeRef::named_nn(&connection_type_name),
        move |ctx| {
            let db = Arc::clone(&db);
            let schema_name = schema_name.clone();
            let table_name = table_name.clone();
            let table_info = table_info.clone();
            let all_tables_for_lateral = all_tables_for_lateral.clone();

            FieldFuture::new(async move {
                let rls = ctx.data::<RlsContext>().ok();
                let condition_json = extract_json_arg(&ctx, "condition");
                let order_json = extract_json_arg(&ctx, "orderBy");

                #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                let first = ctx
                    .args
                    .try_get("first")
                    .ok()
                    .and_then(|v| v.i64().ok())
                    .map(|n| n.max(0) as usize);

                let after = ctx
                    .args
                    .try_get("after")
                    .ok()
                    .and_then(|v| v.string().ok().map(str::to_owned));

                // Convert condition JSON to FilterExpr
                let filter = condition_json
                    .as_ref()
                    .and_then(|c| condition_to_filter(c, &table_info.columns));

                // Convert orderBy JSON to OrderByExpr
                let order_by = order_json
                    .as_ref()
                    .map(|o| order_to_exprs(o, &table_info.columns))
                    .filter(|v| !v.is_empty());

                // Cursor-based offset
                let offset = after
                    .as_deref()
                    .and_then(connection::decode_cursor)
                    .map_or(0, |n| n + 1);
                let limit = first.unwrap_or(default_page_size).min(max_page_size);

                // Build lateral joins for has-many relationships
                let mut visited = HashSet::new();
                let lateral_selections = build_lateral_selections(&table_info, &all_tables_for_lateral, &mut visited);

                // Build plan for data query
                #[allow(clippy::cast_possible_wrap)]
                let mut data_plan = PlanNode::Scan {
                    schema: schema_name.clone(),
                    table: table_name.clone(),
                    filter: filter.clone(),
                    order_by: order_by.clone(),
                    limit: Some(limit as i64),
                    offset: Some(offset as i64),
                    selections: lateral_selections,
                };

                apply_on_plan_hooks(&ctx, &mut data_plan);

                // Build plan for count query
                let mut count_plan = PlanNode::Scan {
                    schema: schema_name.clone(),
                    table: table_name.clone(),
                    filter,
                    order_by: None,
                    limit: None,
                    offset: None,
                    selections: vec![],
                };

                apply_on_plan_hooks(&ctx, &mut count_plan);

                let total_count = execute_plan_count(&count_plan, &db, rls)
                    .await
                    .map_err(|e| async_graphql::Error::new(format!("database error: {e}")))?;

                let rows = execute_plan(&data_plan, &db, rls)
                    .await
                    .map_err(|e| async_graphql::Error::new(format!("database error: {e}")))?;

                // Unwrap row_to_json wrappers
                let rows: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|row| {
                        row.get("row_to_json")
                            .cloned()
                            .unwrap_or_else(|| row.clone())
                    })
                    .collect();

                let response =
                    connection::build_connection_response(rows, offset, total_count);

                Ok(Some(FieldValue::value(serde_json_to_gql_value(&response))))
            })
        },
    )
    .argument(InputValue::new(
        "condition",
        TypeRef::named(&condition_type_name),
    ))
    .argument(InputValue::new(
        "orderBy",
        TypeRef::named_nn_list(&order_by_type_name),
    ))
    .argument(InputValue::new("first", TypeRef::named(TypeRef::INT)))
    .argument(InputValue::new("after", TypeRef::named(TypeRef::STRING)))
}

/// Extract a JSON argument from the resolver context, returning `None` for
/// missing or null values
fn extract_json_arg(
    ctx: &async_graphql::dynamic::ResolverContext<'_>,
    name: &str,
) -> Option<serde_json::Value> {
    ctx.args
        .try_get(name)
        .ok()
        .map(|v| gql_value_to_json(v.as_value()))
        .filter(|v| !v.is_null())
}

/// Resolve a column's `udt_name` to a `GraphQL` type name, accounting for enums.
fn resolve_column_type(col: &ColumnInfo, enum_names: &HashSet<String>) -> (String, bool) {
    let base_udt = col.udt_name.strip_prefix('_').unwrap_or(&col.udt_name);

    if enum_names.contains(base_udt) {
        let gql_name = to_pascal_case(base_udt);
        (gql_name, col.is_array)
    } else {
        let (gql_type, is_array_from_udt) = map_udt_to_graphql(&col.udt_name);
        (gql_type.to_string(), col.is_array || is_array_from_udt)
    }
}

/// Convert a column to a full `TypeRef`, respecting nullability and array status.
fn column_to_type_ref(col: &ColumnInfo, enum_names: &HashSet<String>) -> TypeRef {
    let (gql_type, is_array) = resolve_column_type(col, enum_names);

    if is_array {
        if col.is_nullable {
            TypeRef::named_nn_list(gql_type)
        } else {
            TypeRef::named_nn_list_nn(gql_type)
        }
    } else if col.is_nullable {
        TypeRef::named(gql_type)
    } else {
        TypeRef::named_nn(gql_type)
    }
}

/// Unwrap a `row_to_json` result into a `FieldValue` backed by the `GraphQL`
/// value object.
///
/// The row result from `SELECT row_to_json(t.*)` is a JSON object wrapped in
/// another layer — we extract the inner object.
fn row_json_to_field_value(row: &serde_json::Value) -> FieldValue<'static> {
    // `row_to_json` returns the row as a JSON object. When fetched via our
    // `Database::query` / `query_raw`, the result is already a JSON object with
    // a single `row_to_json` key, or the object itself.
    let obj = row
        .get("row_to_json")
        .cloned()
        .unwrap_or_else(|| row.clone());

    FieldValue::value(serde_json_to_gql_value(&obj))
}

/// Convert a `serde_json::Value` to an `async_graphql::Value`.
fn serde_json_to_gql_value(value: &serde_json::Value) -> async_graphql::Value {
    match value {
        serde_json::Value::Null => async_graphql::Value::Null,
        serde_json::Value::Bool(b) => async_graphql::Value::from(*b),
        serde_json::Value::Number(n) => n.as_i64().map_or_else(
            || {
                n.as_f64().map_or_else(
                    || async_graphql::Value::String(n.to_string()),
                    async_graphql::Value::from,
                )
            },
            async_graphql::Value::from,
        ),
        serde_json::Value::String(s) => async_graphql::Value::String(s.clone()),
        serde_json::Value::Array(arr) => {
            async_graphql::Value::List(arr.iter().map(serde_json_to_gql_value).collect())
        }
        serde_json::Value::Object(map) => {
            let obj: async_graphql::indexmap::IndexMap<async_graphql::Name, async_graphql::Value> =
                map.iter()
                    .map(|(k, v)| (async_graphql::Name::new(k), serde_json_to_gql_value(v)))
                    .collect();
            async_graphql::Value::Object(obj)
        }
    }
}

/// Convert a `ValueAccessor` argument to a `serde_json::Value` for parameter
/// binding.
fn value_accessor_to_json(
    accessor: &async_graphql::dynamic::ValueAccessor<'_>,
) -> serde_json::Value {
    gql_value_to_json(accessor.as_value())
}

/// Convert an `async_graphql::Value` to a `serde_json::Value`.
fn gql_value_to_json(value: &async_graphql::Value) -> serde_json::Value {
    match value {
        async_graphql::Value::Null | async_graphql::Value::Binary(_) => serde_json::Value::Null,
        async_graphql::Value::Number(n) => {
            serde_json::Value::Number(serde_json::Number::from(n.as_i64().unwrap_or_default()))
        }
        async_graphql::Value::String(s) => serde_json::Value::String(s.clone()),
        async_graphql::Value::Boolean(b) => serde_json::Value::Bool(*b),
        async_graphql::Value::Enum(name) => serde_json::Value::String(name.to_string()),
        async_graphql::Value::List(list) => {
            serde_json::Value::Array(list.iter().map(gql_value_to_json).collect())
        }
        async_graphql::Value::Object(map) => {
            let obj: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.to_string(), gql_value_to_json(v)))
                .collect();
            serde_json::Value::Object(obj)
        }
    }
}

/// Convert `snake_case` to `PascalCase`.
#[must_use]
pub fn to_pascal_case(s: &str) -> String {
    s.split('_')
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |c| {
                let mut result = c.to_uppercase().to_string();
                result.extend(chars);
                result
            })
        })
        .collect()
}

/// Convert `snake_case` to `camelCase`.
#[must_use]
pub fn to_camel_case(s: &str) -> String {
    let pascal = to_pascal_case(s);
    let mut chars = pascal.chars();
    chars.next().map_or_else(String::new, |c| {
        let mut result = c.to_lowercase().to_string();
        result.extend(chars);
        result
    })
}

/// Naive singularization — strip trailing "s" or handle common plural suffixes.
#[must_use]
#[allow(clippy::option_if_let_else)]
pub fn singularize(s: &str) -> String {
    if let Some(base) = s.strip_suffix("ies") {
        // categories -> category, but not "series" -> "ser"
        if base.len() > 1 {
            format!("{base}y")
        } else {
            s.to_string()
        }
    } else if let Some(base) = s.strip_suffix("shes") {
        // crashes -> crash
        format!("{base}sh")
    } else if let Some(base) = s.strip_suffix("ches") {
        // matches -> match
        format!("{base}ch")
    } else if let Some(base) = s.strip_suffix("xes") {
        // boxes -> box
        format!("{base}x")
    } else if let Some(base) = s.strip_suffix("sses") {
        // classes -> class, addresses -> address
        format!("{base}ss")
    } else if let Some(base) = s.strip_suffix("ses") {
        // statuses -> status
        format!("{base}s")
    } else if let Some(base) = s.strip_suffix('s') {
        // users -> user, posts -> post
        // But not "ss" words like "class" -> "clas"
        if base.ends_with('s') {
            s.to_string()
        } else {
            base.to_string()
        }
    } else {
        s.to_string()
    }
}

/// Naive pluralization — append "s" or handle common suffixes.
#[must_use]
pub fn pluralize(s: &str) -> String {
    if s.ends_with('s') || s.ends_with("sh") || s.ends_with("ch") || s.ends_with('x') {
        format!("{s}es")
    } else if s.ends_with('y')
        && !s.ends_with("ey")
        && !s.ends_with("ay")
        && !s.ends_with("oy")
    {
        format!("{}ies", &s[..s.len() - 1])
    } else {
        format!("{s}s")
    }
}


/// Register all mutation input types, payload types, and mutation fields
/// for a single table.
#[allow(clippy::too_many_arguments)]
fn register_mutation_types(
    mut schema_builder: async_graphql::dynamic::SchemaBuilder,
    mut mutation: Object,
    table: &TableInfo,
    type_name: &str,
    table_field_name: &str,
    pk_col: &ColumnInfo,
    enum_names: &HashSet<String>,
    db: &Arc<dyn Database>,
) -> (async_graphql::dynamic::SchemaBuilder, Object) {
    // Check @omit tag for operation-specific exclusions
    let omit_ops = table
        .tags
        .get("omit")
        .map(String::as_str)
        .unwrap_or_default();
    let omit_create = omit_ops.contains("create");
    let omit_update = omit_ops.contains("update");
    let omit_delete = omit_ops.contains("delete");

    if !omit_create {
        schema_builder =
            schema_builder.register(mutations::build_create_input(table, type_name, enum_names));
        schema_builder = schema_builder.register(build_mutation_payload(
            &format!("Create{type_name}"),
            table_field_name,
            type_name,
        ));
        mutation = mutation.field(build_create_mutation(table, type_name, table_field_name, db));
    }

    if !omit_update {
        schema_builder = schema_builder
            .register(mutations::build_update_input(table, type_name, pk_col, enum_names));
        schema_builder = schema_builder
            .register(mutations::build_update_patch(table, type_name, pk_col, enum_names));
        schema_builder = schema_builder.register(build_mutation_payload(
            &format!("Update{type_name}"),
            table_field_name,
            type_name,
        ));
        mutation =
            mutation.field(build_update_mutation(table, type_name, table_field_name, pk_col, db));
    }

    if !omit_delete {
        schema_builder = schema_builder
            .register(mutations::build_delete_input(table, type_name, pk_col, enum_names));
        schema_builder = schema_builder.register(build_mutation_payload(
            &format!("Delete{type_name}"),
            table_field_name,
            type_name,
        ));
        mutation =
            mutation.field(build_delete_mutation(table, type_name, table_field_name, pk_col, db));
    }

    (schema_builder, mutation)
}

/// Build a mutation payload object type with `clientMutationId` and the entity field.
fn build_mutation_payload(
    prefix: &str,
    table_field_name: &str,
    entity_type_name: &str,
) -> Object {
    let payload_name = format!("{prefix}Payload");
    let field_name = table_field_name.to_string();
    let entity_type = entity_type_name.to_string();

    let field_name_clone = field_name.clone();

    let mut payload = Object::new(&payload_name);

    payload = payload.field(Field::new(
        "clientMutationId",
        TypeRef::named(TypeRef::STRING),
        |ctx| {
            FieldFuture::new(async move {
                let parent = ctx.parent_value.try_to_value()?;
                if let async_graphql::Value::Object(map) = parent {
                    Ok(map.get("clientMutationId").cloned().map(FieldValue::value))
                } else {
                    Ok(None)
                }
            })
        },
    ));

    payload = payload.field(Field::new(
        field_name,
        TypeRef::named(&entity_type),
        move |ctx| {
            let field_name = field_name_clone.clone();
            FieldFuture::new(async move {
                let parent = ctx.parent_value.try_to_value()?;
                if let async_graphql::Value::Object(map) = parent {
                    match map.get(field_name.as_str()).cloned() {
                        Some(async_graphql::Value::Null) | None => Ok(None),
                        Some(v) => Ok(Some(FieldValue::value(v))),
                    }
                } else {
                    Ok(None)
                }
            })
        },
    ));

    payload
}

/// Build a `create{Type}` mutation field.
#[allow(clippy::too_many_lines)]
fn build_create_mutation(
    table: &TableInfo,
    type_name: &str,
    table_field_name: &str,
    db: &Arc<dyn Database>,
) -> Field {
    let field_name = format!("create{type_name}");
    let input_type_name = format!("Create{type_name}Input");
    let payload_type_name = format!("Create{type_name}Payload");

    let schema_name = table.schema.clone();
    let table_info = table.clone();
    let table_field = table_field_name.to_string();
    let db = Arc::clone(db);

    Field::new(
        &field_name,
        TypeRef::named(&payload_type_name),
        move |ctx| {
            let db = Arc::clone(&db);
            let schema_name = schema_name.clone();
            let table_info = table_info.clone();
            let table_field = table_field.clone();

            FieldFuture::new(async move {
                let rls = ctx.data::<RlsContext>().ok();
                let input_value = ctx.args.try_get("input")?;
                let input_json = gql_value_to_json(input_value.as_value());

                let client_mutation_id = input_json
                    .get("clientMutationId")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                let (sql, params) =
                    mutations::create_to_sql(&input_json, &table_info, &schema_name);

                let rows = db.query_with_rls(&sql, &params, rls).await.map_err(|e| {
                    async_graphql::Error::new(format!("database error: {e}"))
                })?;

                let row = rows
                    .into_iter()
                    .next()
                    .and_then(|r| {
                        r.get("row_to_json").cloned().or(Some(r))
                    })
                    .unwrap_or(serde_json::Value::Null);

                let mut payload = serde_json::Map::new();
                payload.insert("clientMutationId".to_string(), client_mutation_id);
                payload.insert(table_field, row);

                Ok(Some(FieldValue::value(serde_json_to_gql_value(
                    &serde_json::Value::Object(payload),
                ))))
            })
        },
    )
    .argument(InputValue::new("input", TypeRef::named_nn(&input_type_name)))
}

/// Build an `update{Type}` mutation field.
#[allow(clippy::too_many_lines)]
fn build_update_mutation(
    table: &TableInfo,
    type_name: &str,
    table_field_name: &str,
    pk_col: &ColumnInfo,
    db: &Arc<dyn Database>,
) -> Field {
    let field_name = format!("update{type_name}");
    let input_type_name = format!("Update{type_name}Input");
    let payload_type_name = format!("Update{type_name}Payload");

    let schema_name = table.schema.clone();
    let table_info = table.clone();
    let table_field = table_field_name.to_string();
    let pk_col_name = pk_col.name.clone();
    let pk_field_name = to_camel_case(&pk_col.name);
    let db = Arc::clone(db);

    Field::new(
        &field_name,
        TypeRef::named(&payload_type_name),
        move |ctx| {
            let db = Arc::clone(&db);
            let schema_name = schema_name.clone();
            let table_info = table_info.clone();
            let table_field = table_field.clone();
            let pk_col_name = pk_col_name.clone();
            let pk_field_name = pk_field_name.clone();

            FieldFuture::new(async move {
                let rls = ctx.data::<RlsContext>().ok();
                let input_value = ctx.args.try_get("input")?;
                let input_json = gql_value_to_json(input_value.as_value());

                let client_mutation_id = input_json
                    .get("clientMutationId")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                let pk_value = input_json
                    .get(&pk_field_name)
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                let patch = input_json
                    .get("patch")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                let (sql, params) = mutations::update_to_sql(
                    &pk_col_name,
                    &pk_value,
                    &patch,
                    &table_info,
                    &schema_name,
                );

                let rows = db.query_with_rls(&sql, &params, rls).await.map_err(|e| {
                    async_graphql::Error::new(format!("database error: {e}"))
                })?;

                let row = rows
                    .into_iter()
                    .next()
                    .and_then(|r| {
                        r.get("row_to_json").cloned().or(Some(r))
                    })
                    .unwrap_or(serde_json::Value::Null);

                let mut payload = serde_json::Map::new();
                payload.insert("clientMutationId".to_string(), client_mutation_id);
                payload.insert(table_field, row);

                Ok(Some(FieldValue::value(serde_json_to_gql_value(
                    &serde_json::Value::Object(payload),
                ))))
            })
        },
    )
    .argument(InputValue::new("input", TypeRef::named_nn(&input_type_name)))
}

/// Build a `delete{Type}` mutation field.
fn build_delete_mutation(
    table: &TableInfo,
    type_name: &str,
    table_field_name: &str,
    pk_col: &ColumnInfo,
    db: &Arc<dyn Database>,
) -> Field {
    let field_name = format!("delete{type_name}");
    let input_type_name = format!("Delete{type_name}Input");
    let payload_type_name = format!("Delete{type_name}Payload");

    let schema_name = table.schema.clone();
    let table_info = table.clone();
    let table_field = table_field_name.to_string();
    let pk_col_name = pk_col.name.clone();
    let pk_field_name = to_camel_case(&pk_col.name);
    let db = Arc::clone(db);

    Field::new(
        &field_name,
        TypeRef::named(&payload_type_name),
        move |ctx| {
            let db = Arc::clone(&db);
            let schema_name = schema_name.clone();
            let table_info = table_info.clone();
            let table_field = table_field.clone();
            let pk_col_name = pk_col_name.clone();
            let pk_field_name = pk_field_name.clone();

            FieldFuture::new(async move {
                let rls = ctx.data::<RlsContext>().ok();
                let input_value = ctx.args.try_get("input")?;
                let input_json = gql_value_to_json(input_value.as_value());

                let client_mutation_id = input_json
                    .get("clientMutationId")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                let pk_value = input_json
                    .get(&pk_field_name)
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                let (sql, params) = mutations::delete_to_sql(
                    &pk_col_name,
                    &pk_value,
                    &table_info,
                    &schema_name,
                );

                let rows = db.query_with_rls(&sql, &params, rls).await.map_err(|e| {
                    async_graphql::Error::new(format!("database error: {e}"))
                })?;

                let row = rows
                    .into_iter()
                    .next()
                    .and_then(|r| {
                        r.get("row_to_json").cloned().or(Some(r))
                    })
                    .unwrap_or(serde_json::Value::Null);

                let mut payload = serde_json::Map::new();
                payload.insert("clientMutationId".to_string(), client_mutation_id);
                payload.insert(table_field, row);

                Ok(Some(FieldValue::value(serde_json_to_gql_value(
                    &serde_json::Value::Object(payload),
                ))))
            })
        },
    )
    .argument(InputValue::new("input", TypeRef::named_nn(&input_type_name)))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn pascal_case_conversion() {
        assert_eq!(to_pascal_case("user_account"), "UserAccount");
        assert_eq!(to_pascal_case("id"), "Id");
        assert_eq!(to_pascal_case("created_at"), "CreatedAt");
    }

    #[test]
    fn camel_case_conversion() {
        assert_eq!(to_camel_case("user_account"), "userAccount");
        assert_eq!(to_camel_case("id"), "id");
        assert_eq!(to_camel_case("created_at"), "createdAt");
    }

    #[test]
    fn singularization_rules() {
        assert_eq!(singularize("users"), "user");
        assert_eq!(singularize("posts"), "post");
        assert_eq!(singularize("comments"), "comment");
        assert_eq!(singularize("statuses"), "status");
        assert_eq!(singularize("categories"), "category");
        assert_eq!(singularize("addresses"), "address");
        assert_eq!(singularize("class"), "class"); // no-op
        assert_eq!(singularize("person"), "person"); // no-op
    }

    #[test]
    fn pluralization_rules() {
        assert_eq!(pluralize("user"), "users");
        assert_eq!(pluralize("status"), "statuses");
        assert_eq!(pluralize("category"), "categories");
        assert_eq!(pluralize("key"), "keys");
        assert_eq!(pluralize("day"), "days");
    }

    #[test]
    fn singularize_then_pluralize_roundtrip() {
        // Ensure singularize + pluralize = original for common table names
        assert_eq!(pluralize(&singularize("users")), "users");
        assert_eq!(pluralize(&singularize("posts")), "posts");
        assert_eq!(pluralize(&singularize("comments")), "comments");
        assert_eq!(pluralize(&singularize("statuses")), "statuses");
        assert_eq!(pluralize(&singularize("categories")), "categories");
    }

    #[test]
    fn json_to_gql_roundtrip() {
        let json = serde_json::json!({
            "name": "Alice",
            "age": 30,
            "active": true,
        });

        let gql = serde_json_to_gql_value(&json);
        assert!(matches!(gql, async_graphql::Value::Object(_)));
    }

    #[test]
    fn resolve_enum_column() {
        let mut enum_names = HashSet::new();
        enum_names.insert("status".to_string());

        let col = ColumnInfo {
            name: "status".to_string(),
            data_type: "USER-DEFINED".to_string(),
            udt_name: "status".to_string(),
            is_nullable: false,
            has_default: false,
            is_generated: false,
            is_array: false,
            comment: None,
            tags: HashMap::new(),
        };

        let (gql_type, is_array) = resolve_column_type(&col, &enum_names);
        assert_eq!(gql_type, "Status");
        assert!(!is_array);
    }
}
