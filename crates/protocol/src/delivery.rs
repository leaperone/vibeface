//! Delivery requests shared by the Node and native client.

use serde::{Deserialize, Serialize};

// Serde's internally tagged unit decoder ignores map entries even with deny_unknown_fields.
fn empty_operation<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<(), D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Empty {}
    Empty::deserialize(deserializer).map(|_| ())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeliveryRead {
    #[serde(deserialize_with = "empty_operation")]
    Status,
    #[serde(deserialize_with = "empty_operation")]
    List,
    #[serde(deserialize_with = "empty_operation")]
    Providers,
    #[serde(deserialize_with = "empty_operation")]
    Budgets,
    Get {
        order_id: String,
    },
    FindIntent {
        intent_id: String,
    },
    Read {
        order_id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    Charity,
    Paid,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryAcceptance {
    Recoverable,
    Strict,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryPrepare {
    pub intent_id: String,
    pub receiver_peer: String,
    pub provider_peer: String,
    pub mode: DeliveryMode,
    pub acceptance: DeliveryAcceptance,
    pub name: String,
    pub content_base64: String,
    pub max_service_amount: String,
    pub max_gas_amount: String,
    pub budget_id: Option<String>,
}

/// The SDK records this intent to reconcile the original batch across retries.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryFlushIntent {
    pub budget_id: String,
    pub batch_id: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryBudgetFlush {
    pub budget_id: String,
}

/// Delivery actions on the Node wire use a flush payload containing only the budget ID.
pub type DeliveryWireAction = DeliveryAction<DeliveryBudgetFlush>;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeliveryAction<Flush = DeliveryFlushIntent> {
    Configure {
        enabled: bool,
        charity_enabled: bool,
        paid_enabled: bool,
    },
    Prepare(DeliveryPrepare),
    Confirm {
        intent_id: String,
    },
    Accept {
        order_id: String,
    },
    Reconcile {
        order_id: String,
    },
    ConfirmBatch {
        intent_ids: [String; 2],
    },
    AcceptBatch {
        order_ids: [String; 2],
    },
    FlushBudget(Flush),
}
