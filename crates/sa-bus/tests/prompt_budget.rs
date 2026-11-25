use sa_engine::guides;
use serde_json::Value;

macro_rules! guide_lengths {
    ($($guide:ident),+ $(,)?) => {
        [$( (stringify!($guide), guides::$guide.len()) ),+]
    };
}

#[test]
fn guide_and_tool_json_stay_within_the_reviewed_budget() {
    let guides = guide_lengths!(
        BASE,
        CONCISE_HANDOFFS,
        CONTINUOUS,
        LEGACY_AUTO,
        DISCOVERY,
        MCP,
        BUS,
        RECOVERY,
        REROUTE,
        ECONOMY,
        POLICY,
        KNOWLEDGE,
        CHECKS,
    );
    // The guide module declares its public constants as `pub const NAME: ...`.
    // If one is added, require it to be included in the measured inventory.
    let mut declared: Vec<_> = include_str!("../../sa-engine/src/guides.rs")
        .lines()
        .filter_map(|line| {
            let (name, _) = line
                .trim_start()
                .strip_prefix("pub const ")?
                .split_once(':')?;
            Some(name.trim())
        })
        .collect();
    let mut measured = guides.map(|(name, _)| name);
    declared.sort_unstable();
    measured.sort_unstable();
    assert_eq!(
        declared, measured,
        "update the measured guide inventory when adding a public guide"
    );

    let guide_bytes: usize = guides.iter().map(|(_, bytes)| bytes).sum();
    let tool_bytes = serde_json::to_vec(&sa_bus::tools()).unwrap().len();
    let budget: Value = serde_json::from_str(include_str!("../prompt-budget.json")).unwrap();
    let mut over_budget = Vec::new();
    for (component, bytes) in [
        ("guides", guide_bytes),
        ("tools", tool_bytes),
        ("total", guide_bytes + tool_bytes),
    ] {
        let limit = budget[component]
            .as_u64()
            .and_then(|limit| usize::try_from(limit).ok())
            .unwrap_or_else(|| panic!("invalid {component} budget in prompt-budget.json"));
        if bytes > limit {
            over_budget.push(format!(
                "{component}: measured {bytes} bytes, budget {limit} bytes"
            ));
        }
    }
    assert!(
        over_budget.is_empty(),
        "{}\nGuide bytes by constant: {guides:?}\nFor deliberate growth, edit crates/sa-bus/prompt-budget.json in the same change and record the reason in the commit.",
        over_budget.join("\n")
    );
}
