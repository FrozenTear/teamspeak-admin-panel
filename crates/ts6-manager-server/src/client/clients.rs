//! Typed REST client for live-client writes.
//!
//! Move-user is `POST /api/servers/{configId}/vs/{sid}/clients/{clid}/move`
//! (`clientmove`). Channel ↑/↓ reorder is a different command
//! (`channeledit` `channel_order`) and does not belong here.

use std::sync::Arc;

use ts6_manager_shared::control::MoveRequest;

use crate::client::api::{self, ApiError};
use crate::client::session::RefreshGate;

/// TeamSpeak `clientmove` error when `clid` is already in the target channel.
pub const ALREADY_MEMBER_OF_CHANNEL: i64 = 770;

pub async fn move_client(
    gate: Arc<RefreshGate>,
    config_id: i64,
    sid: i64,
    clid: i64,
    cid: i64,
) -> Result<(), ApiError> {
    let path = format!("/api/servers/{config_id}/vs/{sid}/clients/{clid}/move");
    let body = MoveRequest {
        cid,
        channel_password: None,
    };
    api::authorized_post_json::<_, ()>(&gate, &api::api_base(), &path, Some(&body)).await
}

/// `true` only for upstream error 770 on a real `clientmove`.
/// Other TeamSpeak failures stay failures — this must not be used to hide
/// a broken channel-reorder call.
pub fn is_already_member_of_channel(err: &ApiError) -> bool {
    matches!(
        err,
        ApiError::BadGateway {
            code: Some(ALREADY_MEMBER_OF_CHANNEL),
            ..
        }
    )
}

/// How a move-user attempt should be shown. `AlreadyThere` is the soft
/// handle for error 770 (and for a picker that targets the client's
/// current channel). It is not a success and not a danger toast.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientMoveOutcome {
    Moved,
    AlreadyThere,
    Failed(ApiError),
}

impl From<Result<(), ApiError>> for ClientMoveOutcome {
    fn from(result: Result<(), ApiError>) -> Self {
        match result {
            Ok(()) => Self::Moved,
            Err(err) if is_already_member_of_channel(&err) => Self::AlreadyThere,
            Err(err) => Self::Failed(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gateway(code: i64, details: &str) -> ApiError {
        ApiError::BadGateway {
            error: "TeamSpeak API Error".into(),
            code: Some(code),
            details: Some(details.into()),
        }
    }

    #[test]
    fn error_770_is_already_in_channel() {
        let err = gateway(770, "already member of channel");
        assert!(is_already_member_of_channel(&err));
        assert_eq!(
            ClientMoveOutcome::from(Err(err)),
            ClientMoveOutcome::AlreadyThere
        );
    }

    #[test]
    fn other_upstream_codes_stay_failures() {
        let err = gateway(2568, "invalid channel order");
        assert!(!is_already_member_of_channel(&err));
        assert!(matches!(
            ClientMoveOutcome::from(Err(err)),
            ClientMoveOutcome::Failed(_)
        ));
    }

    #[test]
    fn message_text_without_code_770_is_not_soft_handled() {
        let err = ApiError::BadGateway {
            error: "TeamSpeak API Error".into(),
            code: None,
            details: Some("already member of channel".into()),
        };
        assert!(!is_already_member_of_channel(&err));
        assert!(matches!(
            ClientMoveOutcome::from(Err(err)),
            ClientMoveOutcome::Failed(_)
        ));
    }

    #[test]
    fn ok_is_moved() {
        assert_eq!(ClientMoveOutcome::from(Ok(())), ClientMoveOutcome::Moved);
    }
}
