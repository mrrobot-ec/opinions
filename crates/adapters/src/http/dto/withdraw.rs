//! Withdrawal HTTP DTOs (W1).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use application::ports::{Combo, WithdrawalReceipt};

#[derive(Debug, Deserialize, ToSchema)]
pub struct WithdrawRequestDto {
    pub user_id: Uuid,
    pub amount_micro: i64,
    pub dest: String,
    /// Required second entry of the destination; both values must canonicalize identically.
    pub confirm_dest: String,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct WithdrawalReceiptDto {
    pub id: Option<Uuid>,
    pub user_id: Uuid,
    pub dest: String,
    pub amount_micro: i64,
    pub combo: Option<String>,
    pub hold_tx_id: Option<Uuid>,
    pub replayed: bool,
    pub refused: bool,
    pub refuse_code: Option<String>,
    pub refuse_message: Option<String>,
}

impl From<WithdrawalReceipt> for WithdrawalReceiptDto {
    fn from(receipt: WithdrawalReceipt) -> Self {
        Self {
            id: receipt.id.map(|id| id.0),
            user_id: receipt.user.0,
            dest: receipt.dest,
            amount_micro: receipt.amount_micro,
            combo: receipt.combo.and_then(Combo::label).map(ToOwned::to_owned),
            hold_tx_id: receipt.hold_tx_id,
            replayed: receipt.replayed,
            refused: receipt.refused,
            refuse_code: receipt.refuse_code,
            refuse_message: receipt.refuse_message,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct DecideRequestDto {
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct WithdrawalDecisionDto {
    pub id: Uuid,
    pub combo: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use application::model::UserId;

    #[test]
    fn receipt_dto_maps_refusal() {
        let dto = WithdrawalReceiptDto::from(WithdrawalReceipt {
            id: None,
            user: UserId(Uuid::nil()),
            dest: "d".into(),
            amount_micro: 1,
            combo: None,
            hold_tx_id: None,
            replayed: true,
            refused: true,
            refuse_code: Some("paused".into()),
            refuse_message: Some("paused".into()),
        });
        assert!(dto.refused);
        assert!(dto.replayed);
    }
}
