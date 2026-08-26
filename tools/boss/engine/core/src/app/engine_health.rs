//! Shared project-scoped engine-health attention plumbing.

use std::sync::Arc;

use super::ServerState;

/// Project-scoped engine-health attention, the durable global-notification
/// shape available to the app (attentions require a work-item association).
pub(crate) async fn raise_engine_health_attention(
    server_state: &Arc<ServerState>,
    group_key_prefix: &str,
    source_doc_path: &str,
    prompt_text: String,
) {
    let products = match server_state.work_db.list_products() {
        Ok(products) => products,
        Err(err) => {
            tracing::warn!(
                ?err,
                group_key_prefix,
                "engine health: failed to list products for attention"
            );
            return;
        }
    };
    for product in products.into_iter().filter(|product| product.status == "active") {
        let projects = match server_state.work_db.list_projects(&product.id, None) {
            Ok(projects) => projects,
            Err(err) => {
                tracing::warn!(
                    product_id = %product.id,
                    ?err,
                    group_key_prefix,
                    "engine health: failed to list projects for attention"
                );
                continue;
            }
        };
        for project in projects {
            let input = boss_protocol::CreateAttentionInput::builder()
                .kind("question")
                .group_key(format!("{group_key_prefix}|{}", project.id))
                .association_project_id(project.id.clone())
                .source_kind("manual")
                .source_doc_path(source_doc_path)
                .question_type("prompt")
                .prompt_text(prompt_text.clone())
                .build();
            match server_state.work_db.reconcile_attentions(vec![input]) {
                Ok(Some((group, attentions))) => {
                    for attention in attentions {
                        server_state
                            .publisher
                            .publish_frontend_event_on_product(
                                &group.product_id,
                                boss_protocol::FrontendEvent::AttentionCreated {
                                    attention,
                                    group: group.clone(),
                                },
                            )
                            .await;
                    }
                }
                Ok(None) => {}
                Err(err) => tracing::warn!(
                    project_id = %project.id,
                    ?err,
                    group_key_prefix,
                    "engine health: failed to create attention"
                ),
            }
        }
    }
}
