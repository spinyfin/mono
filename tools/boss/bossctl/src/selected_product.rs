//! `bossctl selected-product` — which product the Boss UI's chooser is
//! currently set to.
//!
//! Short ids (`T<n>`) are per-product and most `boss` read verbs require
//! `--product`, so a coordinator answering a question about a short id
//! has to know which product the operator is looking at. Guessing is not
//! a safe fallback: a wrong-product lookup *succeeds*, returning a real
//! row with a real status and a real PR for the wrong work item.
//!
//! This verb therefore either answers or fails — never approximates. The
//! engine holds the answer (the app reports its chooser state); all the
//! work here is rendering it and making "no answer" impossible to
//! mistake for one: a non-zero exit in both output modes, a specific
//! reason on stderr, and a machine-readable `status` in `--json`.

use anyhow::{Context, Result, bail};
use boss_protocol::{FrontendEvent, FrontendRequest, SelectedProductState};

use super::connect;

pub(crate) async fn selected_product(socket_path: &Option<String>, json: bool) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::GetSelectedProduct)
        .await
        .context("sending GetSelectedProduct")?;
    let state = match response {
        FrontendEvent::SelectedProductResult { state } => state,
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected selected-product: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    };
    report(&state, json)
}

/// Render `state` and decide the exit disposition. Split from the socket
/// round-trip so every unavailable case is unit-testable without an
/// engine — which matters more here than usual, since the unavailable
/// cases *are* the feature.
fn report(state: &SelectedProductState, json: bool) -> Result<()> {
    if json {
        // Printed even when unavailable: the `status` field is how a
        // caller distinguishes "app not running" from "nothing selected"
        // from "product deleted", and it is worth more than a clean
        // stdout on the failure path.
        println!("{}", serde_json::to_string(state).expect("state serializes"));
    } else if let SelectedProductState::Selected {
        product_id, name, slug, ..
    } = state
    {
        println!("{name}  ({slug})");
        println!("  product id: {product_id}");
    }

    match state.unavailable_reason() {
        None => Ok(()),
        Some(reason) => bail!("no selected product: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selected() -> SelectedProductState {
        SelectedProductState::Selected {
            product_id: "prod_abc".into(),
            name: "Flunge".into(),
            slug: "flunge".into(),
            reported_at: 1_700_000_000,
        }
    }

    #[test]
    fn selected_state_exits_ok_in_both_modes() {
        assert!(report(&selected(), false).is_ok());
        assert!(report(&selected(), true).is_ok());
    }

    /// Every unavailable case must fail, in both output modes — a
    /// coordinator that treats a zero exit as "I know the product" would
    /// otherwise silently resolve short ids against a guess.
    #[test]
    fn every_unavailable_state_fails_in_both_modes() {
        for state in [
            SelectedProductState::AppNotConnected,
            SelectedProductState::NoSelection,
            SelectedProductState::ProductUnknown {
                product_id: "prod_gone".into(),
            },
        ] {
            assert!(report(&state, false).is_err(), "human mode should fail for {state:?}",);
            assert!(report(&state, true).is_err(), "json mode should fail for {state:?}");
        }
    }

    /// The three unavailable cases must not collapse into one message:
    /// "Boss isn't running" and "you have no product selected" call for
    /// different operator actions.
    #[test]
    fn unavailable_reasons_are_distinguishable() {
        let messages: Vec<String> = [
            SelectedProductState::AppNotConnected,
            SelectedProductState::NoSelection,
            SelectedProductState::ProductUnknown {
                product_id: "prod_gone".into(),
            },
        ]
        .iter()
        .map(|state| state.unavailable_reason().expect("unavailable"))
        .collect();

        for (i, a) in messages.iter().enumerate() {
            for b in messages.iter().skip(i + 1) {
                assert_ne!(a, b, "unavailable reasons must differ");
            }
        }
        assert!(
            messages[2].contains("prod_gone"),
            "product_unknown should name the id it could not resolve: {}",
            messages[2],
        );
    }

    /// `--json` carries the discriminator a caller branches on, and the
    /// selected case carries all three `--product` selectors.
    #[test]
    fn json_carries_status_discriminator() {
        let value = serde_json::to_value(selected()).unwrap();
        assert_eq!(value["status"], "selected");
        assert_eq!(value["product_id"], "prod_abc");
        assert_eq!(value["name"], "Flunge");
        assert_eq!(value["slug"], "flunge");

        let value = serde_json::to_value(SelectedProductState::AppNotConnected).unwrap();
        assert_eq!(value["status"], "app_not_connected");
        assert!(value.get("product_id").is_none());
    }
}
