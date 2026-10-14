use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::error::ApiError;

pub const MAX_LABELS: usize = 20;
const MAX_LEN: usize = 63;

pub const LABEL_RULE: &str = "Keys and values: up to 63 characters, letters, digits, \"-\", \"_\" and \".\"; key is required.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Label {
    pub key: String,
    pub value: String,
}

fn key_ok(key: &str) -> bool {
    (1..=MAX_LEN).contains(&key.len())
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn value_ok(value: &str) -> bool {
    value.chars().count() <= MAX_LEN
        && value
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub fn validate(labels: &[Label]) -> Result<(), ApiError> {
    let invalid = |detail: &str| ApiError::Validation(format!("{LABEL_RULE} {detail}"));
    if labels.len() > MAX_LABELS {
        return Err(invalid("At most 20 labels are allowed."));
    }
    let mut seen = HashSet::new();
    for label in labels {
        if !key_ok(&label.key) || !value_ok(&label.value) {
            return Err(invalid(""));
        }
        if !seen.insert(label.key.as_str()) {
            return Err(invalid("Keys must be unique."));
        }
    }
    Ok(())
}
