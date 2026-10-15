use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use uuid::Uuid;

/// Every error leaves the API as `{"error": {"code", "message", "details"}}`.
#[derive(Debug)]
pub enum ApiError {
    Validation(String),
    IdempotencyKeyRequired,
    Unauthorized,
    NotFound(&'static str),
    PaymentFailed { code: String, attempt: Value },
    InvalidStateTransition { from: String, action: &'static str },
    InvoiceAlreadyPaid,
    PaymentInProgress,
    ConcurrentModification,
    RequestInProgress,
    CustomerAlreadyExists { id: Uuid },
    IdempotencyKeyReuse,
    Unprocessable { code: &'static str, message: String },
    MethodNotAllowed,
    Internal,
}

impl ApiError {
    pub fn parts(&self) -> (StatusCode, Value) {
        let (status, code, message, details): (StatusCode, &str, String, Value) = match self {
            Self::Validation(msg) => (StatusCode::BAD_REQUEST, "validation_error", msg.clone(), Value::Null),
            Self::IdempotencyKeyRequired => (
                StatusCode::BAD_REQUEST,
                "idempotency_key_required",
                "The Idempotency-Key header is required for this endpoint".into(),
                Value::Null,
            ),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized", "Missing or invalid API key".into(), Value::Null),
            Self::NotFound(resource) => (StatusCode::NOT_FOUND, "not_found", format!("No such {resource}"), Value::Null),
            Self::PaymentFailed { code, attempt } => (
                StatusCode::PAYMENT_REQUIRED,
                code.as_str(),
                "The payment was not successful".into(),
                json!({ "payment_attempt": attempt }),
            ),
            Self::InvalidStateTransition { from, action } => (
                StatusCode::CONFLICT,
                "invalid_state_transition",
                format!("Cannot {action} an invoice in status '{from}'"),
                json!({ "from": from, "action": action }),
            ),
            Self::InvoiceAlreadyPaid => (StatusCode::CONFLICT, "invoice_already_paid", "This invoice is already paid".into(), Value::Null),
            Self::PaymentInProgress => (
                StatusCode::CONFLICT,
                "payment_in_progress",
                "A payment attempt for this invoice is still in progress".into(),
                Value::Null,
            ),
            Self::ConcurrentModification => (
                StatusCode::CONFLICT,
                "concurrent_modification",
                "The invoice was modified by a concurrent request; retry".into(),
                Value::Null,
            ),
            Self::RequestInProgress => (
                StatusCode::CONFLICT,
                "request_in_progress",
                "A request with this Idempotency-Key is still in progress".into(),
                Value::Null,
            ),
            Self::CustomerAlreadyExists { id } => (
                StatusCode::CONFLICT,
                "customer_already_exists",
                "A customer with this email already exists".into(),
                json!({ "customer_id": id }),
            ),
            Self::IdempotencyKeyReuse => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "idempotency_key_reuse",
                "This Idempotency-Key was already used with a different request".into(),
                Value::Null,
            ),
            Self::Unprocessable { code, message } => (StatusCode::UNPROCESSABLE_ENTITY, code, message.clone(), Value::Null),
            Self::MethodNotAllowed => {
                (StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed", "This method is not allowed on this route".into(), Value::Null)
            }
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error", "Internal server error".into(), Value::Null),
        };
        (status, json!({ "error": { "code": code, "message": message, "details": details } }))
    }

    /// Errors whose answer can change for the same request are not stored under an Idempotency-Key,
    /// so the client can retry with the same key. `InvalidStateTransition` is one of them: paying a
    /// draft fails now but succeeds once the draft is finalized. Permanent answers (e.g.
    /// `InvoiceAlreadyPaid`, since paid is terminal) are stored and replayed.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::ConcurrentModification
                | Self::PaymentInProgress
                | Self::RequestInProgress
                | Self::InvalidStateTransition { .. }
                | Self::Internal
                | Self::Unauthorized
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = self.parts();
        (status, Json(body)).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        tracing::error!(error = %err, "database error");
        Self::Internal
    }
}
