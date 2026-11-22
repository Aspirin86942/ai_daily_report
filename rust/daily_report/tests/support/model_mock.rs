use serde_json::{json, Value};
pub fn reply(request: &Value) -> Value {
    let text = request["messages"][1]["content"].as_str().unwrap();
    if let Ok(input) = serde_json::from_str::<Value>(text) {
        if request["messages"][0]["content"]
            .as_str()
            .is_some_and(|s| s.contains("schema_version=5"))
        {
            let sources = input["sources"].as_array().unwrap();
            let source = sources
                .iter()
                .find(|s| {
                    s["text"]
                        .as_str()
                        .is_some_and(|s| s.contains("100") && s.contains("120"))
                })
                .unwrap_or(&sources[0]);
            let kind = if source["source_kind"] == "user_statement" {
                "user_statement"
            } else if source["text_unverified"] == true {
                "uncertain"
            } else {
                "observed"
            };
            return json!({"schema_version":5,"items":[
                {"title":"报价仍待审批","text":"金额由 100 元调整为 120 元；预算内且复核通过后生效，仍待审批。","kind":"attention","statement_kind":kind,"source_ids":[source["source_id"]]},
                {"title":"跟进建议","text":"建议确认审批责任人与截止日期；材料尚未给出明确截止时间。","kind":"next_step","statement_kind":"suggestion","source_ids":[source["source_id"]]}
            ]});
        }
        if let Some(facts) = input["facts"].as_array() {
            let ids: Vec<_> = facts.iter().map(|f| f["fact_id"].clone()).collect();
            let sources: std::collections::BTreeSet<_> = facts
                .iter()
                .flat_map(|f| {
                    f["fact"]["source_ids"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap().to_owned())
                })
                .collect();
            let cited =
                json!({"text":"材料记载采购方案，尚待评审。","fact_ids":ids,"source_ids":sources});
            return json!({"schema_version":3,"overview":[cited],"matters":[{"title":"采购方案","background":null,"details":[cited],"status":null}],"pending":[]});
        }
        let facts:Vec<_>=input["sources"].as_array().unwrap().iter().map(|s|json!({"topic":"采购方案","background":null,"detail":"材料记载采购方案，尚待评审。","conditions":[],"status":null,"source_ids":[s["source_id"]],"uncertainty":null})).collect();
        return json!({"schema_version":3,"facts":facts,"pending":[]});
    }
    if text.contains("year_month") {
        serde_json::from_str(include_str!("../fixtures/monthly-v1.json")).unwrap()
    } else {
        serde_json::from_str(include_str!("../fixtures/weekly-v1.json")).unwrap()
    }
}
