use serde::Serialize;

/// Public build metadata. This never implies pairing or an encrypted connection.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub protocol_version: u32,
    pub app_version: &'static str,
    pub stage: &'static str,
    pub capabilities: Vec<&'static str>,
}

pub fn scaffold_status() -> Status {
    Status {
        protocol_version: 1,
        app_version: env!("CARGO_PKG_VERSION"),
        stage: "scaffold",
        capabilities: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_matches_shared_contract_without_remote_capabilities() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../protocol/fixtures/scaffold-status.json"
        ))
        .expect("valid shared fixture");
        assert_eq!(serde_json::to_value(scaffold_status()).unwrap(), fixture);
    }
}
