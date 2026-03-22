use async_trait::async_trait;
use lattice_execution::{ExecutionError, SubgraphError, SubgraphResponse};

pub use lattice_execution::{SubgraphExecutor, SubgraphRequest};

use crate::embed::TaprootSubgraph;

/// Wrap a `TaprootSubgraph` to implement Lattice's `SubgraphExecutor` trait.
pub struct TaprootExecutor {
    subgraph: TaprootSubgraph,
}

impl TaprootExecutor {
    /// Create a new executor from a `TaprootSubgraph`.
    #[must_use]
    pub const fn new(subgraph: TaprootSubgraph) -> Self {
        Self { subgraph }
    }
}

#[async_trait]
impl SubgraphExecutor for TaprootExecutor {
    async fn execute_graphql(
        &self,
        request: SubgraphRequest,
    ) -> Result<SubgraphResponse, ExecutionError> {
        let result = self
            .subgraph
            .execute(
                &request.query,
                request.operation_name.as_deref(),
                request.variables,
            )
            .await;

        Ok(SubgraphResponse {
            data: result.get("data").cloned(),
            errors: result.get("errors").and_then(|e| {
                e.as_array().map(|arr| {
                    arr.iter()
                        .filter_map(|err| {
                            err.get("message")
                                .and_then(|m| m.as_str())
                                .map(|msg| SubgraphError {
                                    message: msg.to_string(),
                                })
                        })
                        .collect()
                })
            }),
        })
    }
}
