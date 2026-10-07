use super::reward_amount_label;

#[test]
fn reward_preview_uses_the_configured_denomination() {
    assert_eq!(reward_amount_label(100, "uamplifier"), "100 uamplifier");
    assert_eq!(reward_amount_label(100, "uaxl"), "0.000100 AXL (100 uaxl)");
    assert_eq!(
        reward_amount_label(5_553_500_000, "uaxl"),
        "5553.500000 AXL (5553500000 uaxl)"
    );
}
