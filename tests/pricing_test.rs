use plan_executor::pricing::{calculate_cost, ModelPricing, PricingTable};

fn make_table() -> PricingTable {
    let mut table = PricingTable::new();
    table.insert("claude-sonnet-4-6".to_string(), ModelPricing {
        input_per_mtok: 3.0,
        output_per_mtok: 15.0,
        cache_write_per_mtok: 3.75,
        cache_read_per_mtok: 0.3,
    });
    table
}

#[test]
fn test_cost_calculation() {
    let table = make_table();
    // 1M input + 1M output = $3 + $15 = $18
    let cost = calculate_cost(&table, "claude-sonnet-4-6", 1_000_000, 1_000_000, 0, 0).unwrap();
    assert!((cost - 18.0).abs() < 0.001);
}

#[test]
fn test_cost_prefix_match() {
    let table = make_table();
    // Model with suffix like "[1m]" should match via prefix
    let cost = calculate_cost(&table, "claude-sonnet-4-6[1m]", 1_000_000, 0, 0, 0).unwrap();
    assert!((cost - 3.0).abs() < 0.001);
}

#[test]
fn test_unknown_model_returns_none() {
    let table = make_table();
    let cost = calculate_cost(&table, "unknown-model", 1_000_000, 0, 0, 0);
    assert!(cost.is_none());
}
