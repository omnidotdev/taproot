use std::collections::HashSet;
use std::fmt::Write;
use std::sync::Arc;

use async_graphql::dynamic::{
    Enum, EnumItem, Field, FieldFuture, FieldValue, InputValue, Object, Scalar, Schema, TypeRef,
};

use crate::db::{Database, RlsContext};
use crate::error::{Error, Result};
use crate::introspect::{ColumnInfo, FunctionInfo, IntrospectedSchema, TableInfo, UniqueConstraint};
use crate::planner::{condition_to_filter, execute_plan, execute_plan_count, order_to_exprs, FilterExpr, PlanNode};
use crate::plugin::Plugin;
use crate::subscription::ChangeBroadcaster;

use super::federation;
use super::loader::{LoaderKey, LoaderRegistry, normalize_key};
use super::scalars::map_udt_to_graphql;
use super::types::SchemaOptions;
use super::{aggregates, connection, filters, mutations, ordering};

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

/// Parse the Relay pagination arguments from a resolver context, decoding the
/// `after`/`before` cursors into absolute offsets. Negative or malformed
/// numeric values become `None`.
fn parse_page_args(ctx: &async_graphql::dynamic::ResolverContext<'_>) -> connection::PageArgs {
    let uint = |name: &str| {
        ctx.args
            .try_get(name)
            .ok()
            .and_then(|v| v.i64().ok())
            .and_then(|n| usize::try_from(n).ok())
    };
    let cursor = |name: &str| {
        ctx.args
            .try_get(name)
            .ok()
            .and_then(|v| v.string().ok().map(str::to_owned))
            .as_deref()
            .and_then(connection::decode_cursor)
    };
    connection::PageArgs {
        first: uint("first"),
        after: cursor("after"),
        last: uint("last"),
        before: cursor("before"),
        offset: uint("offset"),
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
    broadcaster: Option<&ChangeBroadcaster>,
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
    let subscription_type = if options.subscriptions {
        Some("Subscription")
    } else {
        None
    };
    let mut schema_builder = Schema::build("Query", Some("Mutation"), subscription_type);
    schema_builder = schema_builder.data(plugin_store);
    // Expose the pooled database handle so plugin-contributed field resolvers
    // can run SQL (alongside the per-request RlsContext added by the server)
    schema_builder = schema_builder.data(Arc::clone(db));

    let loader_registry = LoaderRegistry::new(Arc::clone(db), None);
    schema_builder = schema_builder.data(loader_registry);

    if let Some(broadcaster) = broadcaster {
        schema_builder = schema_builder.data(broadcaster.clone());
    }

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
            let object = build_object_type(
                &type_name,
                table,
                &all_tables,
                &enum_names,
                db,
                options,
                &introspected.functions,
                &introspected.name,
            );
            schema_builder = schema_builder.register(object);

            // Register federation entity mapping
            if options.federation
                && let Some(pk) = &table.primary_key
            {
                let pk_cols: Vec<(String, String)> = pk
                    .columns
                    .iter()
                    .map(|c| (crate::introspect::column_field_name(c), c.clone()))
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

            // Register aggregate types
            let agg_type = aggregates::build_aggregates_type(&type_name, &table.columns);
            schema_builder = schema_builder.register(agg_type);

            if let Some(sum_type) = aggregates::build_sum_type(&type_name, &table.columns) {
                schema_builder = schema_builder.register(sum_type);
            }
            if let Some(avg_type) = aggregates::build_avg_type(&type_name, &table.columns) {
                schema_builder = schema_builder.register(avg_type);
            }
            if let Some(min_type) = aggregates::build_min_type(&type_name, &table.columns) {
                schema_builder = schema_builder.register(min_type);
            }
            if let Some(max_type) = aggregates::build_max_type(&type_name, &table.columns) {
                schema_builder = schema_builder.register(max_type);
            }

            // Register connection and edge types (with aggregates field)
            let agg_type_name = format!("{type_name}Aggregates");
            let connection_type = connection::build_connection_type(&type_name)
                .field(Field::new(
                    "aggregates",
                    TypeRef::named_nn(&agg_type_name),
                    |ctx| {
                        FieldFuture::new(async move {
                            let parent = ctx.parent_value.try_to_value()?;
                            if let async_graphql::Value::Object(map) = parent
                                && let Some(v) = map.get("aggregates")
                            {
                                return Ok(Some(FieldValue::value(v.clone())));
                            }

                            Ok(Some(FieldValue::value(async_graphql::Value::Object(
                                async_graphql::indexmap::IndexMap::default(),
                            ))))
                        })
                    },
                ));
            schema_builder = schema_builder.register(connection_type);

            let edge_type = connection::build_edge_type(&type_name);
            schema_builder = schema_builder.register(edge_type);

            // Singular query (by primary key)
            if let Some(pk) = &table.primary_key {
                let pk_cols: Vec<&ColumnInfo> = pk
                    .columns
                    .iter()
                    .filter_map(|name| table.columns.iter().find(|c| &c.name == name))
                    .collect();

                if pk_cols.len() == pk.columns.len() {
                    let field =
                        build_singular_query(table, &type_name, &pk_cols, &enum_names, db);
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

            // Mutations for non-view tables with primary keys
            if !table.is_view
                && let Some(pk) = &table.primary_key
            {
                let pk_cols: Vec<ColumnInfo> = pk
                    .columns
                    .iter()
                    .filter_map(|name| table.columns.iter().find(|c| &c.name == name).cloned())
                    .collect();

                if pk_cols.len() == pk.columns.len() {
                    has_mutations = true;
                    let table_field_name = to_camel_case(&singularize(&table.name));

                    let (sb, mt) = register_mutation_types(
                        schema_builder,
                        mutation,
                        table,
                        &type_name,
                        &table_field_name,
                        &pk_cols,
                        &enum_names,
                        db,
                        &all_tables,
                    );
                    schema_builder = sb;
                    mutation = mt;
                }
            }
        }
    }

    // Expose non-computed STABLE/IMMUTABLE SQL functions as root query fields
    let table_names: HashSet<String> = all_tables.iter().map(|t| t.name.clone()).collect();

    for introspected in &schemas {
        for func in &introspected.functions {
            // Skip computed fields — they are already registered on their parent type
            if func.computed_table.is_some() {
                continue;
            }

            // Only expose STABLE and IMMUTABLE functions
            if func.volatility != 's' && func.volatility != 'i' {
                continue;
            }

            let field = build_custom_function_query(
                func,
                &table_names,
                db,
                options,
            );
            query = query.field(field);
        }
    }

    // Expose non-computed VOLATILE SQL functions as root mutation fields
    for introspected in &schemas {
        for func in &introspected.functions {
            // Skip computed fields — they are already registered on their parent type
            if func.computed_table.is_some() {
                continue;
            }

            // Only expose VOLATILE functions as mutations
            if func.volatility != 'v' {
                continue;
            }

            has_mutations = true;
            let field = build_custom_function_query(
                func,
                &table_names,
                db,
                options,
            );
            mutation = mutation.field(field);
        }
    }

    // Custom root fields contributed by plugins (Postgraphile
    // makeExtendSchemaPlugin parity)
    for plugin in plugins {
        for field in plugin.query_fields(&schemas) {
            query = query.field(field);
        }
    }

    // Relay-classic nested `query` root: `query: Query!` returns the Query type
    // itself, so clients can re-enter the root from any point in a document
    query = query.field(Field::new(
        "query",
        TypeRef::named_nn("Query"),
        |_ctx| {
            FieldFuture::new(async {
                Ok(Some(FieldValue::value(async_graphql::Value::Object(
                    async_graphql::indexmap::IndexMap::default(),
                ))))
            })
        },
    ));

    schema_builder = schema_builder.register(query);

    let mut has_plugin_mutations = false;
    for plugin in plugins {
        for field in plugin.mutation_fields(&schemas) {
            mutation = mutation.field(field);
            has_plugin_mutations = true;
        }
    }

    if !has_mutations && !has_plugin_mutations {
        // Schema requires at least one field on the mutation type
        mutation = mutation.field(Field::new(
            "_empty",
            TypeRef::named(TypeRef::STRING),
            |_| FieldFuture::new(async { Ok(None::<FieldValue<'_>>) }),
        ));
    }
    schema_builder = schema_builder.register(mutation);

    if options.subscriptions && broadcaster.is_some() {
        use async_graphql::dynamic::{Subscription, SubscriptionField, SubscriptionFieldFuture};

        let mut subscription = Subscription::new("Subscription");

        for introspected in &schemas {
            for table in &introspected.tables {
                let type_name = to_pascal_case(&singularize(&table.name));

                // Only tables with single-column PK
                let Some(pk) = &table.primary_key else {
                    continue;
                };
                if pk.columns.len() != 1 {
                    continue;
                }

                let field_name = format!("on{type_name}Changed");
                let schema_name = table.schema.clone();
                let table_name = table.name.clone();
                let pk_col = pk.columns[0].clone();
                let db_for_sub = Arc::clone(db);

                let sub_field = SubscriptionField::new(
                    &field_name,
                    TypeRef::named(&type_name),
                    move |ctx| {
                        let db = Arc::clone(&db_for_sub);
                        let schema_name = schema_name.clone();
                        let table_name = table_name.clone();
                        let pk_col = pk_col.clone();

                        SubscriptionFieldFuture::new(async move {
                            let broadcaster =
                                ctx.data::<ChangeBroadcaster>().map_err(|_| {
                                    async_graphql::Error::new(
                                        "subscription broadcaster not available",
                                    )
                                })?;

                            let mut rx = broadcaster.subscribe();

                            let stream = async_stream::stream! {
                                while let Ok(notif) = rx.recv().await {
                                    if notif.table != table_name {
                                        continue;
                                    }

                                    let rls = ctx.data::<RlsContext>().ok();

                                    let plan = PlanNode::Scan {
                                        schema: schema_name.clone(),
                                        table: table_name.clone(),
                                        filter: Some(FilterExpr::Eq {
                                            column: pk_col.clone(),
                                            value: serde_json::json!(notif.id), cast: None,
                                        }),
                                        order_by: None,
                                        limit: Some(1),
                                        offset: None,
                                        selections: vec![],
                                    };

                                    match execute_plan(&plan, &db, rls).await {
                                        Ok(rows) => {
                                            if let Some(row) = rows.into_iter().next() {
                                                yield Ok(FieldValue::value(
                                                    serde_json_to_gql_value(&row),
                                                ));
                                            }
                                        }
                                        Err(e) => {
                                            yield Err(async_graphql::Error::new(format!(
                                                "subscription fetch error: {e}"
                                            )));
                                        }
                                    }
                                }
                            };

                            Ok(stream)
                        })
                    },
                );

                subscription = subscription.field(sub_field);
            }
        }

        schema_builder = schema_builder.register(subscription);
    }

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
                            value: json_value, cast: None,
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

    if options.max_depth > 0 {
        schema_builder = schema_builder.limit_depth(options.max_depth);
    }

    if options.max_complexity > 0 {
        schema_builder = schema_builder.limit_complexity(options.max_complexity);
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
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn build_object_type(
    type_name: &str,
    table: &TableInfo,
    all_tables: &[&TableInfo],
    enum_names: &HashSet<String>,
    db: &Arc<dyn Database>,
    options: &SchemaOptions,
    functions: &[FunctionInfo],
    schema_name: &str,
) -> Object {
    let mut object = Object::new(type_name);

    // Column fields
    for col in &table.columns {
        let field_name = crate::introspect::column_field_name(&col.name);
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
                            let page_args = parse_page_args(&ctx);

                            #[allow(clippy::cast_possible_wrap)]
                            let total_count = all_rows.len() as i64;

                            let window = connection::compute_window(page_args, default_page_size, max_page_size, total_count);
                            let rows: Vec<serde_json::Value> = all_rows
                                .into_iter()
                                .skip(window.skip)
                                .take(window.take)
                                .collect();

                            let response = connection::build_connection_response(
                                rows,
                                window.skip,
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

                        let page_args = parse_page_args(&ctx);

                        let filter = FilterExpr::Eq {
                            column: child_fk_col.clone(),
                            value: pk_json, cast: None,
                        };

                        let mut count_plan = PlanNode::Scan {
                            schema: child_schema.clone(),
                            table: child_table_name.clone(),
                            filter: Some(filter.clone()),
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

                        let window = connection::compute_window(page_args, default_page_size, max_page_size, total_count);

                        #[allow(clippy::cast_possible_wrap)]
                        let mut data_plan = PlanNode::Scan {
                            schema: child_schema.clone(),
                            table: child_table_name.clone(),
                            filter: Some(filter),
                            order_by: None,
                            limit: Some(window.take as i64),
                            offset: Some(window.skip as i64),
                            selections: vec![],
                        };

                        apply_on_plan_hooks(&ctx, &mut data_plan);

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
                            connection::build_connection_response(rows, window.skip, total_count);

                        Ok(Some(FieldValue::value(serde_json_to_gql_value(&response))))
                    })
                },
            )
            .argument(InputValue::new("first", TypeRef::named(TypeRef::INT)))
            .argument(InputValue::new("after", TypeRef::named(TypeRef::STRING)))
            .argument(InputValue::new("last", TypeRef::named(TypeRef::INT)))
            .argument(InputValue::new("before", TypeRef::named(TypeRef::STRING)))
            .argument(InputValue::new("offset", TypeRef::named(TypeRef::INT)));

            object = object.field(field);
        }
    }

    // Computed fields from SQL functions
    if let Some(pk) = &table.primary_key {
        for func in functions {
            // Only expose functions that are computed fields for this table
            if func.computed_table.as_deref() != Some(&table.name) {
                continue;
            }

            // Only STABLE and IMMUTABLE functions for query fields
            if func.volatility == 'v' {
                continue;
            }

            let field = build_computed_field(func, schema_name, &table.name, pk, db);
            object = object.field(field);
        }
    }

    // Mark entity types with @key for federation
    if options.federation
        && let Some(pk) = &table.primary_key
    {
        let key_fields: Vec<String> = pk.columns.iter().map(|c| crate::introspect::column_field_name(c)).collect();
        object = object.key(key_fields.join(" "));
    }

    object
}

/// Derive a GraphQL field name from a computed field function name.
///
/// If the function follows the `table_name_field_name` convention, strip the
/// table name prefix before camel-casing. Otherwise camel-case the full name.
fn computed_field_name(func_name: &str, table_name: &str) -> String {
    let prefix = format!("{table_name}_");
    let base = func_name.strip_prefix(&prefix).unwrap_or(func_name);
    to_camel_case(base)
}

/// Build a computed field resolver for a SQL function whose first parameter is
/// a table's row type.
///
/// The resolver re-fetches the row by PK and passes it to the function:
/// `SELECT "schema"."func"(t.*) FROM "schema"."table" t WHERE t."pk" = $1`
#[allow(clippy::too_many_lines)]
fn build_computed_field(
    func: &FunctionInfo,
    schema_name: &str,
    table_name: &str,
    pk: &crate::introspect::PrimaryKey,
    db: &Arc<dyn Database>,
) -> Field {
    let field_name = computed_field_name(&func.name, table_name);

    // Determine GraphQL return type
    let (gql_type, is_array) = map_udt_to_graphql(&func.return_type);
    let type_ref = if func.returns_set || is_array {
        TypeRef::named_nn_list(gql_type)
    } else {
        TypeRef::named(gql_type)
    };

    let func_name = func.name.clone();
    let func_schema = schema_name.to_string();
    let func_table = table_name.to_string();
    let pk_cols = pk.columns.clone();
    let returns_set = func.returns_set;
    let extra_args: Vec<(String, String)> = func.arguments.clone();
    let db = Arc::clone(db);

    let mut field = Field::new(field_name, type_ref, move |ctx| {
        let db = Arc::clone(&db);
        let func_name = func_name.clone();
        let func_schema = func_schema.clone();
        let func_table = func_table.clone();
        let pk_cols = pk_cols.clone();
        let extra_args = extra_args.clone();
        let returns_set = returns_set;

        FieldFuture::new(async move {
            let rls = ctx.data::<RlsContext>().ok();
            let parent = ctx.parent_value.try_to_value()?;

            let async_graphql::Value::Object(map) = parent else {
                return Ok(None);
            };

            // Build WHERE clause from PK columns
            let mut where_parts = Vec::new();
            let mut params: Vec<serde_json::Value> = Vec::new();
            for (i, col) in pk_cols.iter().enumerate() {
                // The parent row is keyed by raw column names, not GraphQL names
                let Some(pk_val) = map.get(col.as_str()) else {
                    return Ok(None);
                };
                if *pk_val == async_graphql::Value::Null {
                    return Ok(None);
                }
                params.push(gql_value_to_json(pk_val));
                where_parts.push(format!("t.\"{}\" = ${}", col, i + 1));
            }

            // Collect extra function arguments from GraphQL field args
            for (arg_name, _) in &extra_args {
                if arg_name.is_empty() {
                    continue;
                }
                let camel_arg = to_camel_case(arg_name);
                if let Ok(val) = ctx.args.try_get(&camel_arg) {
                    params.push(gql_value_to_json(val.as_value()));
                }
            }

            let where_clause = where_parts.join(" AND ");

            // Build extra parameter placeholders for the function call
            let mut func_args = String::from("t.*");
            let base_idx = pk_cols.len() + 1;
            for (i, (arg_name, _)) in extra_args.iter().enumerate() {
                if arg_name.is_empty() {
                    continue;
                }
                write!(func_args, ", ${}", base_idx + i).ok();
            }

            let sql = format!(
                "SELECT \"{func_schema}\".\"{func_name}\"({func_args}) AS result \
                 FROM \"{func_schema}\".\"{func_table}\" t \
                 WHERE {where_clause}"
            );

            let rows = db
                .query_with_rls(&sql, &params, rls)
                .await
                .map_err(|e| async_graphql::Error::new(format!("database error: {e}")))?;

            if returns_set {
                let values: Vec<async_graphql::Value> = rows
                    .iter()
                    .filter_map(|row| {
                        let val = row.get("result")?;
                        if val.is_null() {
                            None
                        } else {
                            Some(serde_json_to_gql_value(val))
                        }
                    })
                    .collect();
                Ok(Some(FieldValue::value(async_graphql::Value::List(values))))
            } else {
                let result = rows
                    .first()
                    .and_then(|row| row.get("result"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                if result.is_null() {
                    Ok(None)
                } else {
                    Ok(Some(FieldValue::value(serde_json_to_gql_value(&result))))
                }
            }
        })
    });

    // Add extra arguments as GraphQL field arguments
    for (arg_name, arg_type) in &func.arguments {
        if arg_name.is_empty() {
            continue;
        }
        let (gql_arg_type, _) = map_udt_to_graphql(arg_type);
        let camel_arg = to_camel_case(arg_name);
        field = field.argument(InputValue::new(camel_arg, TypeRef::named(gql_arg_type)));
    }

    field
}

/// Build a root query field for a non-computed SQL function.
///
/// Scalar functions become simple fields; SETOF-table functions use the
/// connection pattern; SETOF-scalar functions return a list.
#[allow(clippy::too_many_lines)]
fn build_custom_function_query(
    func: &FunctionInfo,
    table_names: &HashSet<String>,
    db: &Arc<dyn Database>,
    options: &SchemaOptions,
) -> Field {
    let field_name = to_camel_case(&func.name);
    let func_name = func.name.clone();
    let func_schema = func.schema.clone();
    let arguments: Vec<(String, String)> = func.arguments.clone();
    let returns_set = func.returns_set;

    // Strip schema qualification from return type if present
    let raw_return = &func.return_type;
    let return_base = raw_return
        .split('.')
        .next_back()
        .unwrap_or(raw_return)
        .trim_matches('"');

    let is_table_return = table_names.contains(return_base);

    if returns_set && is_table_return {
        // SETOF table → connection type
        let table_type = to_pascal_case(&singularize(return_base));
        let connection_type_name = format!("{table_type}Connection");
        let default_page_size = options.default_page_size;
        let max_page_size = options.max_page_size;
        let db = Arc::clone(db);
        let return_base_owned = return_base.to_string();

        let mut field = Field::new(
            &field_name,
            TypeRef::named_nn(&connection_type_name),
            move |ctx| {
                let db = Arc::clone(&db);
                let func_name = func_name.clone();
                let func_schema = func_schema.clone();
                let arguments = arguments.clone();
                let _return_base = return_base_owned.clone();

                FieldFuture::new(async move {
                    let rls = ctx.data::<RlsContext>().ok();

                    // Collect function arguments
                    let mut params: Vec<serde_json::Value> = Vec::new();
                    for (arg_name, _) in &arguments {
                        if arg_name.is_empty() {
                            continue;
                        }
                        let camel_arg = to_camel_case(arg_name);
                        if let Ok(val) = ctx.args.try_get(&camel_arg) {
                            params.push(value_accessor_to_json(&val));
                        }
                    }

                    // Build function call parameter placeholders
                    let placeholders: Vec<String> = (1..=params.len())
                        .map(|i| format!("${i}"))
                        .collect();
                    let func_call = format!(
                        "\"{func_schema}\".\"{func_name}\"({})",
                        placeholders.join(", ")
                    );

                    // Parse pagination args
                    let page_args = parse_page_args(&ctx);

                    // Count query
                    let count_sql = format!(
                        "SELECT COUNT(*) AS count FROM {func_call} AS t"
                    );
                    let count_rows = db
                        .query_with_rls(&count_sql, &params, rls)
                        .await
                        .map_err(|e| async_graphql::Error::new(format!("database error: {e}")))?;

                    #[allow(clippy::cast_possible_wrap)]
                    let total_count = count_rows
                        .first()
                        .and_then(|r| r.get("count"))
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);

                    let window = connection::compute_window(page_args, default_page_size, max_page_size, total_count);

                    // Data query
                    let param_count = params.len();
                    let data_sql = format!(
                        "SELECT row_to_json(t.*) FROM {func_call} AS t \
                         LIMIT ${} OFFSET ${}",
                        param_count + 1,
                        param_count + 2,
                    );

                    let mut data_params = params;
                    #[allow(clippy::cast_possible_wrap)]
                    {
                        data_params.push(serde_json::json!(window.take as i64));
                        data_params.push(serde_json::json!(window.skip as i64));
                    }

                    let rows = db
                        .query_with_rls(&data_sql, &data_params, rls)
                        .await
                        .map_err(|e| async_graphql::Error::new(format!("database error: {e}")))?;

                    // Unwrap row_to_json wrappers and remap column names to camelCase
                    let rows: Vec<serde_json::Value> = rows
                        .into_iter()
                        .map(|row| {
                            let unwrapped = row
                                .get("row_to_json")
                                .cloned()
                                .unwrap_or(row);

                            if let serde_json::Value::Object(map) = unwrapped {
                                let remapped: serde_json::Map<String, serde_json::Value> = map
                                    .into_iter()
                                    .map(|(k, v)| (crate::introspect::column_field_name(&k), v))
                                    .collect();
                                serde_json::Value::Object(remapped)
                            } else {
                                unwrapped
                            }
                        })
                        .collect();

                    let response = connection::build_connection_response(
                        rows, window.skip, total_count,
                    );

                    Ok(Some(FieldValue::value(serde_json_to_gql_value(&response))))
                })
            },
        );

        // Add function arguments
        for (arg_name, arg_type) in &func.arguments {
            if arg_name.is_empty() {
                continue;
            }
            let (gql_arg_type, _) = map_udt_to_graphql(arg_type);
            let camel_arg = to_camel_case(arg_name);
            field = field.argument(InputValue::new(
                camel_arg,
                TypeRef::named_nn(gql_arg_type),
            ));
        }

        // Add pagination arguments
        field = field
            .argument(InputValue::new("first", TypeRef::named(TypeRef::INT)))
            .argument(InputValue::new("after", TypeRef::named(TypeRef::STRING)))
            .argument(InputValue::new("last", TypeRef::named(TypeRef::INT)))
            .argument(InputValue::new("before", TypeRef::named(TypeRef::STRING)))
            .argument(InputValue::new("offset", TypeRef::named(TypeRef::INT)));

        field
    } else if returns_set {
        // SETOF scalar → return list
        let (gql_type, _is_array) = map_udt_to_graphql(return_base);
        let db = Arc::clone(db);

        let mut field = Field::new(
            &field_name,
            TypeRef::named_nn_list(gql_type),
            move |ctx| {
                let db = Arc::clone(&db);
                let func_name = func_name.clone();
                let func_schema = func_schema.clone();
                let arguments = arguments.clone();

                FieldFuture::new(async move {
                    let rls = ctx.data::<RlsContext>().ok();

                    let mut params: Vec<serde_json::Value> = Vec::new();
                    for (arg_name, _) in &arguments {
                        if arg_name.is_empty() {
                            continue;
                        }
                        let camel_arg = to_camel_case(arg_name);
                        if let Ok(val) = ctx.args.try_get(&camel_arg) {
                            params.push(value_accessor_to_json(&val));
                        }
                    }

                    let placeholders: Vec<String> = (1..=params.len())
                        .map(|i| format!("${i}"))
                        .collect();

                    let sql = format!(
                        "SELECT \"{func_schema}\".\"{func_name}\"({}) AS result",
                        placeholders.join(", ")
                    );

                    let rows = db
                        .query_with_rls(&sql, &params, rls)
                        .await
                        .map_err(|e| async_graphql::Error::new(format!("database error: {e}")))?;

                    let values: Vec<async_graphql::Value> = rows
                        .iter()
                        .filter_map(|row| {
                            let val = row.get("result")?;
                            if val.is_null() {
                                None
                            } else {
                                Some(serde_json_to_gql_value(val))
                            }
                        })
                        .collect();

                    Ok(Some(FieldValue::value(async_graphql::Value::List(values))))
                })
            },
        );

        for (arg_name, arg_type) in &func.arguments {
            if arg_name.is_empty() {
                continue;
            }
            let (gql_arg_type, _) = map_udt_to_graphql(arg_type);
            let camel_arg = to_camel_case(arg_name);
            field = field.argument(InputValue::new(
                camel_arg,
                TypeRef::named_nn(gql_arg_type),
            ));
        }

        field
    } else {
        // Scalar return → simple field
        let (gql_type, _is_array) = map_udt_to_graphql(return_base);
        let db = Arc::clone(db);

        let mut field = Field::new(
            &field_name,
            TypeRef::named(gql_type),
            move |ctx| {
                let db = Arc::clone(&db);
                let func_name = func_name.clone();
                let func_schema = func_schema.clone();
                let arguments = arguments.clone();

                FieldFuture::new(async move {
                    let rls = ctx.data::<RlsContext>().ok();

                    let mut params: Vec<serde_json::Value> = Vec::new();
                    for (arg_name, _) in &arguments {
                        if arg_name.is_empty() {
                            continue;
                        }
                        let camel_arg = to_camel_case(arg_name);
                        if let Ok(val) = ctx.args.try_get(&camel_arg) {
                            params.push(value_accessor_to_json(&val));
                        }
                    }

                    let placeholders: Vec<String> = (1..=params.len())
                        .map(|i| format!("${i}"))
                        .collect();

                    let sql = format!(
                        "SELECT \"{func_schema}\".\"{func_name}\"({}) AS result",
                        placeholders.join(", ")
                    );

                    let rows = db
                        .query_with_rls(&sql, &params, rls)
                        .await
                        .map_err(|e| async_graphql::Error::new(format!("database error: {e}")))?;

                    let result = rows
                        .first()
                        .and_then(|row| row.get("result"))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);

                    if result.is_null() {
                        Ok(None)
                    } else {
                        Ok(Some(FieldValue::value(serde_json_to_gql_value(&result))))
                    }
                })
            },
        );

        for (arg_name, arg_type) in &func.arguments {
            if arg_name.is_empty() {
                continue;
            }
            let (gql_arg_type, _) = map_udt_to_graphql(arg_type);
            let camel_arg = to_camel_case(arg_name);
            field = field.argument(InputValue::new(
                camel_arg,
                TypeRef::named_nn(gql_arg_type),
            ));
        }

        field
    }
}

/// Build a singular query field: `user(id: UUID!): User`
fn build_singular_query(
    table: &crate::introspect::TableInfo,
    type_name: &str,
    pk_cols: &[&ColumnInfo],
    enum_names: &HashSet<String>,
    db: &Arc<dyn Database>,
) -> Field {
    let singular = singularize(&table.name);

    // For composite PKs, generate `userRoleByUserIdAndRoleId` style name
    let field_name = if pk_cols.len() == 1 {
        to_camel_case(&singular)
    } else {
        let suffix = pk_cols
            .iter()
            .map(|c| to_pascal_case(&c.name))
            .collect::<Vec<_>>()
            .join("And");
        format!("{}By{suffix}", to_camel_case(&singular))
    };

    // Collect PK arg metadata: (camelCase arg name, snake_case col name, graphql type)
    let pk_args: Vec<(String, String, String)> = pk_cols
        .iter()
        .map(|col| {
            let (gql_type, _) = resolve_column_type(col, enum_names);
            (crate::introspect::column_field_name(&col.name), col.name.clone(), gql_type)
        })
        .collect();

    let schema_name = table.schema.clone();
    let table_name = table.name.clone();
    let db = Arc::clone(db);
    let pk_args_clone = pk_args.clone();

    let mut field = Field::new(
        field_name,
        TypeRef::named(type_name),
        move |ctx| {
            let db = Arc::clone(&db);
            let schema_name = schema_name.clone();
            let table_name = table_name.clone();
            let pk_args = pk_args_clone.clone();

            FieldFuture::new(async move {
                let rls = ctx.data::<RlsContext>().ok();

                let mut filter_parts = Vec::with_capacity(pk_args.len());
                for (arg_name, col_name, _) in &pk_args {
                    let arg_value = ctx.args.try_get(arg_name)?;
                    let json_value = value_accessor_to_json(&arg_value);
                    filter_parts.push(FilterExpr::Eq {
                        column: col_name.clone(),
                        value: json_value, cast: None,
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
        },
    );

    for (arg_name, _, gql_type) in &pk_args {
        field = field.argument(InputValue::new(arg_name.as_str(), TypeRef::named_nn(gql_type.as_str())));
    }

    field
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
    let arg_names: Vec<String> = constraint.columns.iter().map(|c| crate::introspect::column_field_name(c)).collect();
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
                    value: json_value, cast: None,
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
        let arg_name = crate::introspect::column_field_name(&col.name);
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

                let page_args = parse_page_args(&ctx);

                // Convert condition JSON to FilterExpr
                let filter = condition_json
                    .as_ref()
                    .and_then(|c| condition_to_filter(c, &table_info.columns, &table_info.schema));

                // Convert orderBy JSON to OrderByExpr
                let order_by = order_json
                    .as_ref()
                    .map(|o| order_to_exprs(o, &table_info.columns))
                    .filter(|v| !v.is_empty());

                // Build lateral joins for has-many relationships
                let mut visited = HashSet::new();
                let lateral_selections = build_lateral_selections(&table_info, &all_tables_for_lateral, &mut visited);

                // Run the count query first so backward pagination (last/before)
                // can resolve its window against the total
                let mut count_plan = PlanNode::Scan {
                    schema: schema_name.clone(),
                    table: table_name.clone(),
                    filter: filter.clone(),
                    order_by: None,
                    limit: None,
                    offset: None,
                    selections: vec![],
                };

                apply_on_plan_hooks(&ctx, &mut count_plan);

                let total_count = execute_plan_count(&count_plan, &db, rls)
                    .await
                    .map_err(|e| async_graphql::Error::new(format!("database error: {e}")))?;

                let window = connection::compute_window(page_args, default_page_size, max_page_size, total_count);

                // Build plan for data query
                #[allow(clippy::cast_possible_wrap)]
                let mut data_plan = PlanNode::Scan {
                    schema: schema_name.clone(),
                    table: table_name.clone(),
                    filter: filter.clone(),
                    order_by: order_by.clone(),
                    limit: Some(window.take as i64),
                    offset: Some(window.skip as i64),
                    selections: lateral_selections,
                };

                apply_on_plan_hooks(&ctx, &mut data_plan);

                // Keep the filter for the aggregate query
                let agg_filter = filter;

                let rows = execute_plan(&data_plan, &db, rls)
                    .await
                    .map_err(|e| async_graphql::Error::new(format!("database error: {e}")))?;

                // Run aggregate query with the same filter (ignoring pagination)
                let mut agg_sql = aggregates::build_aggregate_sql(
                    &schema_name,
                    &table_name,
                    &table_info.columns,
                );
                let agg_params = agg_filter.as_ref().map_or_else(Vec::new, |expr| {
                    let (where_sql, where_params) =
                        crate::planner::compile_filter(expr, 1);
                    let _ = write!(agg_sql, " WHERE {where_sql}");
                    where_params
                });
                let agg_rows = db
                    .query_with_rls(&agg_sql, &agg_params, rls)
                    .await
                    .map_err(|e| async_graphql::Error::new(format!("database error: {e}")))?;
                let agg_json = agg_rows
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| serde_json::json!({"count": 0}));

                // Unwrap row_to_json wrappers
                let rows: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|row| {
                        row.get("row_to_json")
                            .cloned()
                            .unwrap_or_else(|| row.clone())
                    })
                    .collect();

                let mut response =
                    connection::build_connection_response(rows, window.skip, total_count);

                // Attach aggregate data to the connection response
                if let serde_json::Value::Object(ref mut map) = response {
                    map.insert("aggregates".to_owned(), agg_json);
                }

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
    .argument(InputValue::new("last", TypeRef::named(TypeRef::INT)))
    .argument(InputValue::new("before", TypeRef::named(TypeRef::STRING)))
    .argument(InputValue::new("offset", TypeRef::named(TypeRef::INT)))
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


/// Build nested relationship input types for a create mutation and register
/// them on the schema builder.
///
/// Returns the updated schema builder and the list of nested field descriptors
/// to attach to the `CreateInput`.
#[allow(clippy::too_many_arguments, clippy::implicit_hasher)]
#[allow(clippy::too_many_lines)]
fn build_nested_rel_inputs(
    mut schema_builder: async_graphql::dynamic::SchemaBuilder,
    table: &TableInfo,
    type_name: &str,
    enum_names: &HashSet<String>,
    all_tables: &[&TableInfo],
) -> (
    async_graphql::dynamic::SchemaBuilder,
    Vec<mutations::NestedRelField>,
    Vec<mutations::NestedRelInfo>,
) {
    let mut nested_fields: Vec<mutations::NestedRelField> = Vec::new();
    let mut nested_rels: Vec<mutations::NestedRelInfo> = Vec::new();

    // Belongs-to: FKs on this table pointing to other tables
    for fk in &table.foreign_keys {
        if fk.columns.len() != 1 || fk.foreign_columns.len() != 1 {
            continue;
        }

        let related_table = all_tables.iter().find(|t| {
            t.name == fk.foreign_table && t.schema == fk.foreign_schema
        });
        let Some(related_table) = related_table else {
            continue;
        };

        let related_type = to_pascal_case(&singularize(&fk.foreign_table));
        let rel_field_name = format!(
            "{}By{}",
            to_camel_case(&singularize(&fk.foreign_table)),
            to_pascal_case(&fk.columns[0]),
        );
        let pascal_rel_field = format!(
            "{}By{}",
            to_pascal_case(&singularize(&fk.foreign_table)),
            to_pascal_case(&fk.columns[0]),
        );

        let container_name = format!("{type_name}{pascal_rel_field}Input");
        let connect_name = format!("{type_name}{pascal_rel_field}ConnectInput");
        let related_create_input = format!("Create{related_type}Input");

        let related_pk_cols: Vec<ColumnInfo> = related_table
            .primary_key
            .as_ref()
            .map(|pk| {
                pk.columns
                    .iter()
                    .filter_map(|name| {
                        related_table.columns.iter().find(|c| &c.name == name).cloned()
                    })
                    .collect()
            })
            .unwrap_or_default();

        if related_pk_cols.is_empty() {
            continue;
        }

        let related_pk_column_names: Vec<String> =
            related_pk_cols.iter().map(|c| c.name.clone()).collect();

        schema_builder = schema_builder.register(
            mutations::build_nested_connect_input(&connect_name, &related_pk_cols, enum_names),
        );
        // Check if the related table has @omit(create)
        let related_omit_create = related_table
            .tags
            .get("omit")
            .is_some_and(|v| v.contains("create"));

        schema_builder = schema_builder.register(
            mutations::build_nested_belongs_to_input(
                &container_name,
                &related_create_input,
                &connect_name,
                related_omit_create,
            ),
        );

        nested_fields.push(mutations::NestedRelField {
            field_name: rel_field_name.clone(),
            input_type_name: container_name,
        });

        nested_rels.push(mutations::NestedRelInfo {
            field_name: rel_field_name,
            direction: mutations::NestedRelDirection::BelongsTo {
                fk_columns: fk.columns.clone(),
                related_pk_columns: related_pk_column_names,
            },
            related_table: (*related_table).clone(),
            related_schema: fk.foreign_schema.clone(),
        });
    }

    // Has-many: FKs on other tables pointing back to this table
    let parent_pk_columns: Vec<String> = table
        .primary_key
        .as_ref()
        .map(|pk| pk.columns.clone())
        .unwrap_or_default();

    for other_table in all_tables {
        for fk in &other_table.foreign_keys {
            if fk.columns.len() != 1 || fk.foreign_columns.len() != 1 {
                continue;
            }

            if fk.foreign_table != table.name || fk.foreign_schema != table.schema {
                continue;
            }

            // Skip has-many nested input when the child table has @omit(create)
            let child_omit_create = other_table
                .tags
                .get("omit")
                .is_some_and(|v| v.contains("create"));
            if child_omit_create {
                continue;
            }

            let child_type = to_pascal_case(&singularize(&other_table.name));
            // Mirror the read-side has-many relation naming: pluralize the
            // singular form so an already-plural table name is not doubled
            // (e.g. `posts` -> `postsByAuthorId`, not `postsesByAuthorId`)
            let rel_field_name = format!(
                "{}By{}",
                pluralize(&to_camel_case(&singularize(&other_table.name))),
                to_pascal_case(&fk.columns[0]),
            );
            let pascal_rel_field = format!(
                "{}By{}",
                to_pascal_case(&pluralize(&singularize(&other_table.name))),
                to_pascal_case(&fk.columns[0]),
            );

            let container_name = format!("{type_name}{pascal_rel_field}Input");
            let child_create_input = format!("Create{child_type}Input");

            schema_builder = schema_builder.register(
                mutations::build_nested_has_many_input(&container_name, &child_create_input),
            );

            nested_fields.push(mutations::NestedRelField {
                field_name: rel_field_name.clone(),
                input_type_name: container_name,
            });

            nested_rels.push(mutations::NestedRelInfo {
                field_name: rel_field_name,
                direction: mutations::NestedRelDirection::HasMany {
                    child_fk_columns: fk.columns.clone(),
                    parent_pk_columns: parent_pk_columns.clone(),
                },
                related_table: (*other_table).clone(),
                related_schema: other_table.schema.clone(),
            });
        }
    }

    (schema_builder, nested_fields, nested_rels)
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
    pk_cols: &[ColumnInfo],
    enum_names: &HashSet<String>,
    db: &Arc<dyn Database>,
    all_tables: &[&TableInfo],
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

    let plural_type_name = pluralize(type_name);
    let plural_field_name = pluralize(table_field_name);

    if !omit_create {
        let (sb, nested_fields, nested_rels) =
            build_nested_rel_inputs(schema_builder, table, type_name, enum_names, all_tables);
        schema_builder = sb;

        // Foreign-key columns covered by a belongs-to nested relationship are
        // made optional on the create input (the related row may be supplied
        // via the nested `create`/`connect` field instead of the raw key)
        let belongs_to_fk_columns: HashSet<String> = nested_rels
            .iter()
            .filter_map(|rel| match &rel.direction {
                mutations::NestedRelDirection::BelongsTo { fk_columns, .. } => {
                    Some(fk_columns.clone())
                }
                mutations::NestedRelDirection::HasMany { .. } => None,
            })
            .flatten()
            .collect();

        schema_builder =
            schema_builder.register(mutations::build_on_conflict_input(type_name));
        schema_builder = schema_builder.register(mutations::build_create_input(
            table,
            type_name,
            enum_names,
            &nested_fields,
            &belongs_to_fk_columns,
        ));
        schema_builder = schema_builder.register(build_mutation_payload(
            &format!("Create{type_name}"),
            table_field_name,
            type_name,
        ));
        mutation = mutation.field(build_create_mutation(table, type_name, table_field_name, db, &nested_rels));

        // Bulk create
        schema_builder = schema_builder
            .register(mutations::build_bulk_create_input(type_name, &plural_type_name));
        schema_builder = schema_builder.register(build_bulk_mutation_payload(
            &format!("Create{plural_type_name}Bulk"),
            &plural_field_name,
            type_name,
        ));
        mutation = mutation.field(build_bulk_create_mutation(table, type_name, &plural_field_name, db));
    }

    if !omit_update {
        schema_builder = schema_builder
            .register(mutations::build_update_input(table, type_name, pk_cols, enum_names));
        schema_builder = schema_builder
            .register(mutations::build_update_patch(table, type_name, pk_cols, enum_names));
        schema_builder = schema_builder.register(build_mutation_payload(
            &format!("Update{type_name}"),
            table_field_name,
            type_name,
        ));
        mutation =
            mutation.field(build_update_mutation(table, type_name, table_field_name, pk_cols, db));

        // Bulk update
        schema_builder = schema_builder
            .register(mutations::build_bulk_update_input(type_name, &plural_type_name));
        schema_builder = schema_builder.register(build_bulk_mutation_payload(
            &format!("Update{plural_type_name}Bulk"),
            &plural_field_name,
            type_name,
        ));
        mutation = mutation.field(build_bulk_update_mutation(table, type_name, &plural_field_name, db));
    }

    if !omit_delete {
        schema_builder = schema_builder
            .register(mutations::build_delete_input(table, type_name, pk_cols, enum_names));
        schema_builder = schema_builder.register(build_mutation_payload(
            &format!("Delete{type_name}"),
            table_field_name,
            type_name,
        ));
        mutation =
            mutation.field(build_delete_mutation(table, type_name, table_field_name, pk_cols, db));

        // Bulk delete
        schema_builder = schema_builder
            .register(mutations::build_bulk_delete_input(type_name, &plural_type_name));
        schema_builder = schema_builder.register(build_bulk_mutation_payload(
            &format!("Delete{plural_type_name}Bulk"),
            &plural_field_name,
            type_name,
        ));
        mutation = mutation.field(build_bulk_delete_mutation(table, type_name, &plural_field_name, db));
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

/// Build a bulk mutation payload object type with `clientMutationId`,
/// `affectedRows`, and a list entity field.
fn build_bulk_mutation_payload(
    prefix: &str,
    entity_field_name: &str,
    entity_type_name: &str,
) -> Object {
    let payload_name = format!("{prefix}Payload");
    let field_name = entity_field_name.to_string();
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
        "affectedRows",
        TypeRef::named_nn(TypeRef::INT),
        |ctx| {
            FieldFuture::new(async move {
                let parent = ctx.parent_value.try_to_value()?;
                if let async_graphql::Value::Object(map) = parent {
                    Ok(map.get("affectedRows").cloned().map(FieldValue::value))
                } else {
                    Ok(Some(FieldValue::value(async_graphql::Value::Number(0.into()))))
                }
            })
        },
    ));

    payload = payload.field(Field::new(
        field_name,
        TypeRef::named_nn_list_nn(&entity_type),
        move |ctx| {
            let field_name = field_name_clone.clone();
            FieldFuture::new(async move {
                let parent = ctx.parent_value.try_to_value()?;
                if let async_graphql::Value::Object(map) = parent {
                    match map.get(field_name.as_str()).cloned() {
                        Some(async_graphql::Value::List(items)) => {
                            Ok(Some(FieldValue::list(items.into_iter().map(FieldValue::value))))
                        }
                        _ => Ok(Some(FieldValue::list(std::iter::empty::<FieldValue<'_>>()))),
                    }
                } else {
                    Ok(Some(FieldValue::list(std::iter::empty::<FieldValue<'_>>())))
                }
            })
        },
    ));

    payload
}

/// Build a `create{Types}` bulk create mutation field.
#[allow(clippy::too_many_lines)]
fn build_bulk_create_mutation(
    table: &TableInfo,
    type_name: &str,
    entity_field_name: &str,
    db: &Arc<dyn Database>,
) -> Field {
    let plural_type = pluralize(type_name);
    let field_name = format!("create{plural_type}");
    let input_type_name = format!("Create{plural_type}BulkInput");
    let payload_type_name = format!("Create{plural_type}BulkPayload");

    let schema_name = table.schema.clone();
    let table_info = table.clone();
    let entity_field = entity_field_name.to_string();
    let db = Arc::clone(db);

    Field::new(
        &field_name,
        TypeRef::named(&payload_type_name),
        move |ctx| {
            let db = Arc::clone(&db);
            let schema_name = schema_name.clone();
            let table_info = table_info.clone();
            let entity_field = entity_field.clone();

            FieldFuture::new(async move {
                let rls = ctx.data::<RlsContext>().ok();
                let input_value = ctx.args.try_get("input")?;
                let input_json = gql_value_to_json(input_value.as_value());

                let client_mutation_id = input_json
                    .get("clientMutationId")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                let objects = input_json
                    .get("objects")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default();

                let mut results = Vec::with_capacity(objects.len());

                for obj in &objects {
                    let (sql, params) =
                        mutations::create_to_sql(obj, &table_info, &schema_name);

                    let rows = db.query_with_rls(&sql, &params, rls).await.map_err(|e| {
                        async_graphql::Error::new(format!("database error: {e}"))
                    })?;

                    let row = rows
                        .into_iter()
                        .next()
                        .and_then(|r| r.get("row_to_json").cloned().or(Some(r)))
                        .unwrap_or(serde_json::Value::Null);

                    results.push(serde_json_to_gql_value(&row));
                }

                let affected_rows = results.len();

                let mut payload = async_graphql::indexmap::IndexMap::new();
                payload.insert(
                    async_graphql::Name::new("clientMutationId"),
                    serde_json_to_gql_value(&client_mutation_id),
                );
                payload.insert(
                    async_graphql::Name::new("affectedRows"),
                    async_graphql::Value::from(i64::try_from(affected_rows).unwrap_or_default()),
                );
                payload.insert(
                    async_graphql::Name::new(&entity_field),
                    async_graphql::Value::List(results),
                );

                Ok(Some(FieldValue::value(async_graphql::Value::Object(payload))))
            })
        },
    )
    .argument(InputValue::new("input", TypeRef::named_nn(&input_type_name)))
}

/// Build an `update{Types}` bulk update mutation field.
#[allow(clippy::too_many_lines)]
fn build_bulk_update_mutation(
    table: &TableInfo,
    type_name: &str,
    entity_field_name: &str,
    db: &Arc<dyn Database>,
) -> Field {
    let plural_type = pluralize(type_name);
    let field_name = format!("update{plural_type}");
    let input_type_name = format!("Update{plural_type}BulkInput");
    let payload_type_name = format!("Update{plural_type}BulkPayload");

    let schema_name = table.schema.clone();
    let table_info = table.clone();
    let entity_field = entity_field_name.to_string();
    let db = Arc::clone(db);

    Field::new(
        &field_name,
        TypeRef::named(&payload_type_name),
        move |ctx| {
            let db = Arc::clone(&db);
            let schema_name = schema_name.clone();
            let table_info = table_info.clone();
            let entity_field = entity_field.clone();

            FieldFuture::new(async move {
                let rls = ctx.data::<RlsContext>().ok();
                let input_value = ctx.args.try_get("input")?;
                let input_json = gql_value_to_json(input_value.as_value());

                let client_mutation_id = input_json
                    .get("clientMutationId")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                let where_condition = input_json
                    .get("where")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                let patch = input_json
                    .get("patch")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                // Convert condition to filter expression and compile to SQL
                let filter_expr =
                    condition_to_filter(&where_condition, &table_info.columns, &table_info.schema);

                let (filter_sql, filter_params) = filter_expr
                    .as_ref()
                    .map_or_else(|| ("TRUE".to_string(), Vec::new()), |expr| crate::planner::compile_filter(expr, 1));

                let (sql, params) = mutations::bulk_update_to_sql(
                    &patch,
                    &filter_sql,
                    &filter_params,
                    &table_info,
                    &schema_name,
                );

                let rows = db.query_with_rls(&sql, &params, rls).await.map_err(|e| {
                    async_graphql::Error::new(format!("database error: {e}"))
                })?;

                let results: Vec<async_graphql::Value> = rows
                    .iter()
                    .map(|r| {
                        let row = r.get("row_to_json").cloned().unwrap_or_else(|| r.clone());
                        serde_json_to_gql_value(&row)
                    })
                    .collect();

                let affected_rows = results.len();

                let mut payload = async_graphql::indexmap::IndexMap::new();
                payload.insert(
                    async_graphql::Name::new("clientMutationId"),
                    serde_json_to_gql_value(&client_mutation_id),
                );
                payload.insert(
                    async_graphql::Name::new("affectedRows"),
                    async_graphql::Value::from(i64::try_from(affected_rows).unwrap_or_default()),
                );
                payload.insert(
                    async_graphql::Name::new(&entity_field),
                    async_graphql::Value::List(results),
                );

                Ok(Some(FieldValue::value(async_graphql::Value::Object(payload))))
            })
        },
    )
    .argument(InputValue::new("input", TypeRef::named_nn(&input_type_name)))
}

/// Build a `delete{Types}` bulk delete mutation field.
#[allow(clippy::too_many_lines)]
fn build_bulk_delete_mutation(
    table: &TableInfo,
    type_name: &str,
    entity_field_name: &str,
    db: &Arc<dyn Database>,
) -> Field {
    let plural_type = pluralize(type_name);
    let field_name = format!("delete{plural_type}");
    let input_type_name = format!("Delete{plural_type}BulkInput");
    let payload_type_name = format!("Delete{plural_type}BulkPayload");

    let schema_name = table.schema.clone();
    let table_info = table.clone();
    let entity_field = entity_field_name.to_string();
    let db = Arc::clone(db);

    Field::new(
        &field_name,
        TypeRef::named(&payload_type_name),
        move |ctx| {
            let db = Arc::clone(&db);
            let schema_name = schema_name.clone();
            let table_info = table_info.clone();
            let entity_field = entity_field.clone();

            FieldFuture::new(async move {
                let rls = ctx.data::<RlsContext>().ok();
                let input_value = ctx.args.try_get("input")?;
                let input_json = gql_value_to_json(input_value.as_value());

                let client_mutation_id = input_json
                    .get("clientMutationId")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                let where_condition = input_json
                    .get("where")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                let filter_expr =
                    condition_to_filter(&where_condition, &table_info.columns, &table_info.schema);

                let (filter_sql, filter_params) = filter_expr
                    .as_ref()
                    .map_or_else(|| ("TRUE".to_string(), Vec::new()), |expr| crate::planner::compile_filter(expr, 1));

                let (sql, params) = mutations::bulk_delete_to_sql(
                    &filter_sql,
                    &filter_params,
                    &table_info,
                    &schema_name,
                );

                let rows = db.query_with_rls(&sql, &params, rls).await.map_err(|e| {
                    async_graphql::Error::new(format!("database error: {e}"))
                })?;

                let results: Vec<async_graphql::Value> = rows
                    .iter()
                    .map(|r| {
                        let row = r.get("row_to_json").cloned().unwrap_or_else(|| r.clone());
                        serde_json_to_gql_value(&row)
                    })
                    .collect();

                let affected_rows = results.len();

                let mut payload = async_graphql::indexmap::IndexMap::new();
                payload.insert(
                    async_graphql::Name::new("clientMutationId"),
                    serde_json_to_gql_value(&client_mutation_id),
                );
                payload.insert(
                    async_graphql::Name::new("affectedRows"),
                    async_graphql::Value::from(i64::try_from(affected_rows).unwrap_or_default()),
                );
                payload.insert(
                    async_graphql::Name::new(&entity_field),
                    async_graphql::Value::List(results),
                );

                Ok(Some(FieldValue::value(async_graphql::Value::Object(payload))))
            })
        },
    )
    .argument(InputValue::new("input", TypeRef::named_nn(&input_type_name)))
}

/// Build a `create{Type}` mutation field.
#[allow(clippy::too_many_lines)]
fn build_create_mutation(
    table: &TableInfo,
    type_name: &str,
    table_field_name: &str,
    db: &Arc<dyn Database>,
    nested_rels: &[mutations::NestedRelInfo],
) -> Field {
    let field_name = format!("create{type_name}");
    let input_type_name = format!("Create{type_name}Input");
    let payload_type_name = format!("Create{type_name}Payload");

    let schema_name = table.schema.clone();
    let table_info = table.clone();
    let table_field = table_field_name.to_string();
    let db = Arc::clone(db);
    let nested_rels = nested_rels.to_vec();

    Field::new(
        &field_name,
        TypeRef::named(&payload_type_name),
        move |ctx| {
            let db = Arc::clone(&db);
            let schema_name = schema_name.clone();
            let table_info = table_info.clone();
            let table_field = table_field.clone();
            let nested_rels = nested_rels.clone();

            FieldFuture::new(async move {
                let rls = ctx.data::<RlsContext>().ok();
                let input_value = ctx.args.try_get("input")?;
                let mut input_json = gql_value_to_json(input_value.as_value());

                let client_mutation_id = input_json
                    .get("clientMutationId")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                // Check if any nested relationship fields are present
                let has_nested = nested_rels.iter().any(|rel| {
                    input_json.get(&rel.field_name).is_some_and(|v| !v.is_null())
                });

                let row = if has_nested {
                    // Use a transaction for nested operations. If any step
                    // returns Err, the transaction is implicitly rolled back
                    // when `tx` is dropped (SqlxTransaction rollback-on-drop).
                    let mut tx = db.begin().await.map_err(|e| {
                        async_graphql::Error::new(format!("transaction error: {e}"))
                    })?;

                    if let Some(rls) = rls {
                        tx.set_rls(rls).await.map_err(|e| {
                            async_graphql::Error::new(format!("RLS error: {e}"))
                        })?;
                    }

                    let input_obj = input_json.as_object_mut().ok_or_else(|| {
                        async_graphql::Error::new("input must be an object")
                    })?;

                    // Phase 1 & 2: Process belongs-to relationships
                    for rel in &nested_rels {
                        let nested_val = input_obj.remove(&rel.field_name);
                        let Some(nested_val) = nested_val else {
                            continue;
                        };
                        if nested_val.is_null() {
                            continue;
                        }

                        match &rel.direction {
                            mutations::NestedRelDirection::BelongsTo {
                                fk_columns,
                                related_pk_columns,
                            } => {
                                let nested_obj = nested_val.as_object().ok_or_else(|| {
                                    async_graphql::Error::new(format!(
                                        "nested field `{}` must be an object",
                                        rel.field_name
                                    ))
                                })?;

                                if let Some(connect_val) = nested_obj.get("connect") {
                                    // Phase 1: Connect — validate existence, extract PK, set FK
                                    let connect_obj = connect_val.as_object().ok_or_else(|| {
                                        async_graphql::Error::new("connect must be an object")
                                    })?;

                                    for (fk_col, pk_col) in fk_columns.iter().zip(related_pk_columns.iter()) {
                                        let pk_field = crate::introspect::column_field_name(pk_col);
                                        let pk_value = connect_obj.get(&pk_field).cloned().ok_or_else(|| {
                                            async_graphql::Error::new(format!(
                                                "connect input missing required field `{pk_field}`"
                                            ))
                                        })?;

                                        // Validate the referenced record exists
                                        let check_sql = format!(
                                            "SELECT 1 FROM {}.{} WHERE {} = $1",
                                            mutations::quote_ident(&rel.related_schema),
                                            mutations::quote_ident(&rel.related_table.name),
                                            mutations::quote_ident(pk_col),
                                        );
                                        let check_rows = tx.query(&check_sql, std::slice::from_ref(&pk_value)).await.map_err(|e| {
                                            async_graphql::Error::new(format!("database error: {e}"))
                                        })?;
                                        if check_rows.is_empty() {
                                            return Err(async_graphql::Error::new(format!(
                                                "record not found in `{}` with `{}` = {}",
                                                rel.related_table.name, pk_col, pk_value,
                                            )));
                                        }

                                        let fk_field = crate::introspect::column_field_name(fk_col);
                                        input_obj.insert(fk_field, pk_value);
                                    }
                                } else if let Some(create_val) = nested_obj.get("create") {
                                    // Phase 2: Create — INSERT related record, extract PK
                                    let (sql, params) = mutations::create_to_sql(
                                        create_val,
                                        &rel.related_table,
                                        &rel.related_schema,
                                    );

                                    let rows = tx.query(&sql, &params).await.map_err(|e| {
                                        async_graphql::Error::new(format!("database error: {e}"))
                                    })?;

                                    let related_row = rows
                                        .into_iter()
                                        .next()
                                        .and_then(|r| r.get("row_to_json").cloned().or(Some(r)))
                                        .ok_or_else(|| {
                                            async_graphql::Error::new(
                                                "nested create returned no row"
                                            )
                                        })?;

                                    for (fk_col, pk_col) in fk_columns.iter().zip(related_pk_columns.iter()) {
                                        let pk_value = related_row.get(pk_col).cloned().ok_or_else(|| {
                                            async_graphql::Error::new(format!(
                                                "nested create result missing PK column `{pk_col}`"
                                            ))
                                        })?;
                                        let fk_field = crate::introspect::column_field_name(fk_col);
                                        input_obj.insert(fk_field, pk_value);
                                    }
                                }
                            }
                            mutations::NestedRelDirection::HasMany { .. } => {
                                // Re-insert; has-many is processed after main insert
                                input_obj.insert(rel.field_name.clone(), nested_val);
                            }
                        }
                    }

                    // Phase 3: INSERT the main record
                    let main_input = serde_json::Value::Object(input_obj.clone());
                    let (sql, params) =
                        mutations::create_to_sql(&main_input, &table_info, &schema_name);

                    let rows = tx.query(&sql, &params).await.map_err(|e| {
                        async_graphql::Error::new(format!("database error: {e}"))
                    })?;

                    let main_row = rows
                        .into_iter()
                        .next()
                        .and_then(|r| r.get("row_to_json").cloned().or(Some(r)))
                        .ok_or_else(|| {
                            async_graphql::Error::new("create returned no row")
                        })?;

                    // Phase 4: Process has-many relationships
                    for rel in &nested_rels {
                        let nested_val = input_obj.get(&rel.field_name);
                        let Some(nested_val) = nested_val else {
                            continue;
                        };
                        if nested_val.is_null() {
                            continue;
                        }

                        if let mutations::NestedRelDirection::HasMany {
                            child_fk_columns,
                            parent_pk_columns,
                        } = &rel.direction
                        {
                            let nested_obj = nested_val.as_object().ok_or_else(|| {
                                async_graphql::Error::new(format!(
                                    "nested field `{}` must be an object",
                                    rel.field_name
                                ))
                            })?;

                            if let Some(create_arr) = nested_obj.get("create") {
                                let items = create_arr.as_array().ok_or_else(|| {
                                    async_graphql::Error::new("has-many create must be an array")
                                })?;

                                for item in items {
                                    let mut child_input = item.clone();
                                    let child_obj = child_input.as_object_mut().ok_or_else(|| {
                                        async_graphql::Error::new(
                                            "has-many create item must be an object"
                                        )
                                    })?;

                                    // Set FK columns to parent's PK values
                                    for (child_fk, parent_pk) in child_fk_columns.iter().zip(parent_pk_columns.iter()) {
                                        let pk_value = main_row.get(parent_pk).cloned().ok_or_else(|| {
                                            async_graphql::Error::new(format!(
                                                "parent row missing PK column `{parent_pk}`"
                                            ))
                                        })?;
                                        let fk_field = crate::introspect::column_field_name(child_fk);
                                        child_obj.insert(fk_field, pk_value);
                                    }

                                    let (sql, params) = mutations::create_to_sql(
                                        &serde_json::Value::Object(child_obj.clone()),
                                        &rel.related_table,
                                        &rel.related_schema,
                                    );

                                    tx.query(&sql, &params).await.map_err(|e| {
                                        async_graphql::Error::new(format!("database error: {e}"))
                                    })?;
                                }
                            }
                        }
                    }

                    // Commit transaction
                    tx.commit().await.map_err(|e| {
                        async_graphql::Error::new(format!("commit error: {e}"))
                    })?;

                    main_row
                } else {
                    // No nested fields — use existing non-transactional path
                    let (sql, params) =
                        mutations::create_to_sql(&input_json, &table_info, &schema_name);

                    let rows = db.query_with_rls(&sql, &params, rls).await.map_err(|e| {
                        async_graphql::Error::new(format!("database error: {e}"))
                    })?;

                    rows.into_iter()
                        .next()
                        .and_then(|r| r.get("row_to_json").cloned().or(Some(r)))
                        .unwrap_or(serde_json::Value::Null)
                };

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
    pk_cols: &[ColumnInfo],
    db: &Arc<dyn Database>,
) -> Field {
    let field_name = format!("update{type_name}");
    let input_type_name = format!("Update{type_name}Input");
    let payload_type_name = format!("Update{type_name}Payload");

    let schema_name = table.schema.clone();
    let table_info = table.clone();
    let table_field = table_field_name.to_string();
    // Store (camelCase field name, snake_case column name) pairs
    let pk_mappings: Vec<(String, String)> = pk_cols
        .iter()
        .map(|c| (crate::introspect::column_field_name(&c.name), c.name.clone()))
        .collect();
    let db = Arc::clone(db);

    Field::new(
        &field_name,
        TypeRef::named(&payload_type_name),
        move |ctx| {
            let db = Arc::clone(&db);
            let schema_name = schema_name.clone();
            let table_info = table_info.clone();
            let table_field = table_field.clone();
            let pk_mappings = pk_mappings.clone();

            FieldFuture::new(async move {
                let rls = ctx.data::<RlsContext>().ok();
                let input_value = ctx.args.try_get("input")?;
                let input_json = gql_value_to_json(input_value.as_value());

                let client_mutation_id = input_json
                    .get("clientMutationId")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                let pk_columns: Vec<(String, serde_json::Value)> = pk_mappings
                    .iter()
                    .map(|(field_name, col_name)| {
                        let value = input_json
                            .get(field_name)
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        (col_name.clone(), value)
                    })
                    .collect();

                let patch = input_json
                    .get("patch")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                let (sql, params) = mutations::update_to_sql(
                    &pk_columns,
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
    pk_cols: &[ColumnInfo],
    db: &Arc<dyn Database>,
) -> Field {
    let field_name = format!("delete{type_name}");
    let input_type_name = format!("Delete{type_name}Input");
    let payload_type_name = format!("Delete{type_name}Payload");

    let schema_name = table.schema.clone();
    let table_info = table.clone();
    let table_field = table_field_name.to_string();
    let pk_mappings: Vec<(String, String)> = pk_cols
        .iter()
        .map(|c| (crate::introspect::column_field_name(&c.name), c.name.clone()))
        .collect();
    let db = Arc::clone(db);

    Field::new(
        &field_name,
        TypeRef::named(&payload_type_name),
        move |ctx| {
            let db = Arc::clone(&db);
            let schema_name = schema_name.clone();
            let table_info = table_info.clone();
            let table_field = table_field.clone();
            let pk_mappings = pk_mappings.clone();

            FieldFuture::new(async move {
                let rls = ctx.data::<RlsContext>().ok();
                let input_value = ctx.args.try_get("input")?;
                let input_json = gql_value_to_json(input_value.as_value());

                let client_mutation_id = input_json
                    .get("clientMutationId")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                let pk_columns: Vec<(String, serde_json::Value)> = pk_mappings
                    .iter()
                    .map(|(field_name, col_name)| {
                        let value = input_json
                            .get(field_name)
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        (col_name.clone(), value)
                    })
                    .collect();

                let (sql, params) = mutations::delete_to_sql(
                    &pk_columns,
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

// TODO: @defer/@stream incremental delivery support
// async-graphql (v7) does not support @defer or @stream directives in its
// dynamic schema API (or at all). These are still draft GraphQL spec
// directives and most libraries haven't implemented them. When async-graphql
// adds native support, taproot should expose incremental delivery by:
// 1. Adding @defer on slow/expensive fields (e.g. nested relationships)
// 2. Adding @stream on list fields (e.g. connections) for streaming results
// 3. Using multipart HTTP responses or SSE as the transport
// Track: https://github.com/async-graphql/async-graphql/issues
