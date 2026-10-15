//! The invoice state machine. `transition` is the single source of truth for which API actions are
//! allowed from which status; anything not listed is rejected with 409 invalid_state_transition.
//!
//!   draft --finalize--> open --(attempt succeeded)--> paid
//!   draft --void------> void
//!   open  --void------> void
//!   open  --mark_uncollectible--> uncollectible --void--> void
//!
//! Terminal: paid, void. A failed payment attempt leaves the invoice open.

use crate::error::ApiError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvoiceStatus {
    Draft,
    Open,
    Paid,
    Void,
    Uncollectible,
}

impl InvoiceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Open => "open",
            Self::Paid => "paid",
            Self::Void => "void",
            Self::Uncollectible => "uncollectible",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "draft" => Self::Draft,
            "open" => Self::Open,
            "paid" => Self::Paid,
            "void" => Self::Void,
            "uncollectible" => Self::Uncollectible,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Finalize,
    Void,
    MarkUncollectible,
    /// Starting a payment. It does not change the status by itself; a succeeded attempt moves
    /// open -> paid in `payments::finish_attempt` (conditional on the invoice still being open).
    Pay,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Finalize => "finalize",
            Self::Void => "void",
            Self::MarkUncollectible => "mark_uncollectible",
            Self::Pay => "pay",
        }
    }
}

pub fn transition(from: InvoiceStatus, action: Action) -> Result<InvoiceStatus, ApiError> {
    use InvoiceStatus::*;
    match (from, action) {
        (Draft, Action::Finalize) => Ok(Open),
        (Draft | Open | Uncollectible, Action::Void) => Ok(Void),
        (Open, Action::MarkUncollectible) => Ok(Uncollectible),
        (Open, Action::Pay) => Ok(Open),
        (Paid, Action::Pay) => Err(ApiError::InvoiceAlreadyPaid),
        _ => Err(ApiError::InvalidStateTransition { from: from.as_str().into(), action: action.as_str() }),
    }
}
