use broker_robinhood::RobinhoodClient;
use broker_robinhood::RobinhoodClientError;
use broker_robinhood::client::RobinhoodAccount;
use comfy_table::{
    Attribute, Cell, CellAlignment, Color, ContentArrangement, Table, presets::UTF8_BORDERS_ONLY,
};
use console::set_colors_enabled;
use thiserror::Error;

use crate::creds;

type Result<T> = std::result::Result<T, PositionsError>;

#[derive(Debug, Error)]
pub enum PositionsError {
    #[error(transparent)]
    Credentials(#[from] creds::CredentialsError),
    #[error(transparent)]
    RobinhoodClient(#[from] RobinhoodClientError),
    #[error("no default Robinhood account found")]
    MissingDefaultAccount,
    #[error("Robinhood account `{account}` not found")]
    UnknownAccount { account: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PositionRow {
    account_number: String,
    symbol: String,
    quantity: String,
}

pub async fn run(username: Option<&str>, account: &str) -> Result<()> {
    set_colors_enabled(true);

    let (_, access_token) = creds::load_access_token(username)?;

    let client = RobinhoodClient::new()?;
    let accounts = client.fetch_accounts(&access_token).await?;

    if accounts.is_empty() {
        println!("No Robinhood accounts found.");
        return Ok(());
    }

    let selected_accounts = select_accounts(accounts, account)?;

    let mut rows = Vec::new();
    for account in selected_accounts {
        let mut positions = client
            .fetch_positions(&access_token, &account.account_number)
            .await?;
        positions.sort_by(|left, right| left.symbol.cmp(&right.symbol));

        for position in positions {
            rows.push(PositionRow {
                account_number: account.account_number.clone(),
                symbol: position.symbol,
                quantity: format_quantity(position.quantity),
            });
        }
    }

    if rows.is_empty() {
        println!("No open positions found.");
        return Ok(());
    }

    rows.sort_by(|left, right| {
        left.account_number
            .cmp(&right.account_number)
            .then_with(|| left.symbol.cmp(&right.symbol))
    });

    println!("{}", render_positions_table(&rows));

    Ok(())
}

fn select_accounts(accounts: Vec<RobinhoodAccount>, account: &str) -> Result<Vec<RobinhoodAccount>> {
    if account == "default" {
        let default = accounts
            .into_iter()
            .find(|candidate| candidate.is_default)
            .ok_or(PositionsError::MissingDefaultAccount)?;
        return Ok(vec![default]);
    }

    let selected = accounts
        .into_iter()
        .filter(|candidate| candidate.account_number == account)
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err(PositionsError::UnknownAccount {
            account: account.to_string(),
        });
    }

    Ok(selected)
}

fn render_positions_table(rows: &[PositionRow]) -> String {
    let mut table = Table::new();
    table
        .load_preset(UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec![
            Cell::new("Account").add_attribute(Attribute::Bold),
            Cell::new("Symbol").add_attribute(Attribute::Bold),
            Cell::new("Quantity")
                .add_attribute(Attribute::Bold)
                .set_alignment(CellAlignment::Right),
        ]);

    for row in rows {
        table.add_row(vec![
            Cell::new(&row.account_number).fg(Color::Cyan),
            Cell::new(&row.symbol).fg(Color::Yellow),
            Cell::new(&row.quantity)
                .fg(Color::Green)
                .set_alignment(CellAlignment::Right),
        ]);
    }

    table.to_string()
}

fn format_quantity(quantity: f64) -> String {
    if quantity.abs() < f64::EPSILON {
        return "0".to_string();
    }

    let sign = if quantity < 0.0 { "-" } else { "" };
    let mut text = format!("{:.6}", quantity.abs());
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }

    let mut parts = text.split('.');
    let integer_part = parts.next().unwrap_or_default();
    let fractional_part = parts.next();
    let with_grouping = format_integer_with_grouping(integer_part);

    match fractional_part {
        Some(fractional) if !fractional.is_empty() => {
            format!("{sign}{with_grouping}.{fractional}")
        }
        _ => format!("{sign}{with_grouping}"),
    }
}

fn format_integer_with_grouping(integer: &str) -> String {
    let chars = integer.chars().collect::<Vec<_>>();
    let mut grouped = String::with_capacity(chars.len() + chars.len() / 3);

    for (index, ch) in chars.iter().enumerate() {
        if index > 0 && (chars.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(*ch);
    }

    grouped
}

#[cfg(test)]
mod tests {
    use console::strip_ansi_codes;

    use broker_robinhood::client::RobinhoodAccount;

    use super::{PositionRow, format_quantity, render_positions_table, select_accounts};

    #[test]
    fn format_quantity_groups_and_trims_precision() {
        assert_eq!(format_quantity(1500.0), "1,500");
        assert_eq!(format_quantity(1618.57743), "1,618.57743");
        assert_eq!(format_quantity(-10000.5), "-10,000.5");
    }

    #[test]
    fn format_quantity_handles_zero() {
        assert_eq!(format_quantity(0.0), "0");
    }

    #[test]
    fn render_positions_table_contains_headers_and_rows() {
        let rows = vec![
            PositionRow {
                account_number: "116748102690".to_string(),
                symbol: "AMZN".to_string(),
                quantity: "1,618.57743".to_string(),
            },
            PositionRow {
                account_number: "5QT29231".to_string(),
                symbol: "V".to_string(),
                quantity: "1,500".to_string(),
            },
        ];

        let table = strip_ansi_codes(&render_positions_table(&rows)).to_string();

        assert!(table.contains("Account"));
        assert!(table.contains("Symbol"));
        assert!(table.contains("Quantity"));
        assert!(table.contains("116748102690"));
        assert!(table.contains("AMZN"));
        assert!(table.contains("1,618.57743"));
        assert!(table.contains("5QT29231"));
        assert!(table.contains("V"));
        assert!(table.contains("1,500"));
    }

    #[test]
    fn select_accounts_resolves_default_alias() {
        let accounts = vec![
            RobinhoodAccount {
                account_number: "1234".to_string(),
                brokerage_account_type: None,
                is_default: false,
            },
            RobinhoodAccount {
                account_number: "5678".to_string(),
                brokerage_account_type: None,
                is_default: true,
            },
        ];

        let selected = select_accounts(accounts, "default").expect("default account should resolve");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].account_number, "5678");
    }

    #[test]
    fn select_accounts_returns_unknown_account_error() {
        let accounts = vec![RobinhoodAccount {
            account_number: "1234".to_string(),
            brokerage_account_type: None,
            is_default: true,
        }];

        let error = select_accounts(accounts, "9999").expect_err("unknown account should error");
        assert_eq!(error.to_string(), "Robinhood account `9999` not found");
    }
}
