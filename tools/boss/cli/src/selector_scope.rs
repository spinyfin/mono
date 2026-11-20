//! Product scope for CLI selectors, including script defaults and list filters.

use crate::*;

pub(crate) fn environment_product() -> Option<String> {
    std::env::var("BOSS_PRODUCT")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// List filters must resolve before querying edges or children: those queries
/// cannot distinguish an unknown selector from a real item with no matches.
/// A qualified selector pins the anchor's product independently of the listing.
pub(crate) async fn resolve_filter_id(
    client: &mut BossClient,
    selector: &str,
    product_id: &str,
) -> Result<String, CliError> {
    let scoped = match parse_work_item_selector(selector) {
        WorkItemSelector::ShortId(n) => format!("{product_id}/{n}"),
        _ => selector.to_owned(),
    };
    Ok(get_work_item(client, &scoped).await?.primary_id().to_owned())
}

/// Qualify the selector without looking up its row, so restore and dependency
/// operations can still address tombstones through the engine.
pub(crate) async fn scope_environment_selector(client: &mut BossClient, id: &str) -> Result<String, CliError> {
    if let (WorkItemSelector::ShortId(n), Some(product)) = (parse_work_item_selector(id), environment_product()) {
        let products = list_products(client).await?;
        return Ok(format!("{}/{n}", match_products(&products, &product)?.id));
    }
    Ok(id.to_owned())
}

/// Diagnostics are best-effort and use the selected-product response directly,
/// so app-closed global resolution neither prints a notice nor lists products.
/// A successful dependency mutation can pass no product id: the engine already
/// resolved its bare prerequisite through the selection, including tombstones.
pub(crate) async fn report_implicit_resolution(client: &mut BossClient, id: &str, resolved_product: Option<&str>) {
    let WorkItemSelector::ShortId(n) = parse_work_item_selector(id) else {
        return;
    };
    let Ok(FrontendEvent::SelectedProductResult {
        state: boss_protocol::SelectedProductState::Selected {
            product_id, name, slug, ..
        },
    }) = client.send_request(&FrontendRequest::GetSelectedProduct).await
    else {
        return;
    };
    if resolved_product.is_some_and(|resolved| resolved != product_id) {
        return;
    }
    eprintln!(
        "Resolved {id} in product {name} (implicit scope); use {slug}/{n} or BOSS_PRODUCT={slug} to pin the product."
    );
}

pub(crate) async fn get_work_item(client: &mut BossClient, id: &str) -> Result<WorkItem, CliError> {
    let scoped = scope_environment_selector(client, id).await?;
    let item = rpc_call!(
        client,
        FrontendRequest::GetWorkItem { id: scoped.clone() },
        "work item fetch",
        FrontendEvent::WorkItemResult { item } => item,
    )?;
    // A qualified selector (including BOSS_PRODUCT) needs no implicit notice.
    report_implicit_resolution(client, &scoped, Some(item.product_id())).await;
    Ok(item)
}
