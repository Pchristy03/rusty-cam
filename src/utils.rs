use serde::{Deserialize, Serialize};

pub fn to_json(text: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Failed to parse JSON: {}", e);
            serde_json::Value::Null
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields, untagged, rename_all = "lowercase")]
pub enum SignalMessage {
    Register {
        from: String,
    },
    Offer {
        sdp: String,
        from: String,
        offer_to: String,
    },
    Answer {
        sdp: String,
        from: String,
        answer_to: String,
    },
    Candidate {
        candidate: String,
        from: String,
        to: String,
    },
    Video {
        from: String,
        to: String,
    },
    Ping {
        t: String,
    },
}
