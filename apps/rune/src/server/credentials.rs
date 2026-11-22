use std::time::{Duration, Instant};

use subtle::ConstantTimeEq;
use uuid::Uuid;

const PAIRING_LIFETIME: Duration = Duration::from_secs(10 * 60);
pub(super) const SESSION_LIFETIME: Duration = Duration::from_secs(8 * 60 * 60);
const TICKET_LIFETIME: Duration = Duration::from_secs(30);

struct Token {
    value: Uuid,
    expires: Instant,
}

impl Token {
    fn new(expires: Instant) -> Self {
        Self {
            value: Uuid::new_v4(),
            expires,
        }
    }

    fn accepts(&self, value: Uuid, now: Instant) -> bool {
        now < self.expires && self.value.as_bytes().ct_eq(value.as_bytes()).into()
    }
}

enum State {
    Unclaimed(Token),
    Claimed {
        session: Token,
        permissions: Permissions,
        ticket: Option<Token>,
    },
}

pub(super) struct Credentials(State);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Permissions {
    pub orchestration_operate: bool,
    pub settings_write: bool,
    pub filesystem_read: bool,
    pub preview_operate: bool,
    pub source_control_write: bool,
    pub terminal_read: bool,
    pub terminal_operate: bool,
}

impl Permissions {
    pub(super) fn scopes(self) -> String {
        let mut scopes = vec!["orchestration:read"];
        if self.orchestration_operate {
            scopes.push("orchestration:operate");
        }
        if self.settings_write {
            scopes.push("settings:write");
        }
        if self.filesystem_read {
            scopes.push("filesystem:read");
        }
        if self.preview_operate {
            scopes.push("preview:operate");
        }
        if self.terminal_read {
            scopes.push("terminal:read");
        }
        if self.terminal_operate {
            scopes.push("terminal:operate");
        }
        if self.source_control_write {
            scopes.push("source-control:write");
        }
        scopes.join(" ")
    }
}

impl Credentials {
    pub(super) fn new(now: Instant) -> (Self, Uuid) {
        let pairing = Token::new(now + PAIRING_LIFETIME);
        let code = pairing.value;
        (Self(State::Unclaimed(pairing)), code)
    }

    pub(super) fn exchange(
        &mut self,
        code: Uuid,
        permissions: Permissions,
        now: Instant,
    ) -> Option<Uuid> {
        let State::Unclaimed(pairing) = &self.0 else {
            return None;
        };
        if !pairing.accepts(code, now) {
            return None;
        }
        let session = Token::new(now + SESSION_LIFETIME);
        let value = session.value;
        self.0 = State::Claimed {
            session,
            permissions,
            ticket: None,
        };
        Some(value)
    }

    pub(super) fn issue_ticket(
        &mut self,
        access_token: Uuid,
        now: Instant,
    ) -> Option<(Uuid, Duration)> {
        let State::Claimed {
            session, ticket, ..
        } = &mut self.0
        else {
            return None;
        };
        if !session.accepts(access_token, now) {
            return None;
        }
        let token = Token::new(session.expires.min(now + TICKET_LIFETIME));
        let issued = (token.value, token.expires - now);
        *ticket = Some(token);
        Some(issued)
    }

    pub(super) fn authorizes(&self, access_token: Uuid, now: Instant) -> bool {
        self.session_permissions(access_token, now).is_some()
    }

    pub(super) fn session_permissions(
        &self,
        access_token: Uuid,
        now: Instant,
    ) -> Option<(Instant, Permissions)> {
        match &self.0 {
            State::Claimed {
                session,
                permissions,
                ..
            } if session.accepts(access_token, now) => Some((session.expires, *permissions)),
            _ => None,
        }
    }

    pub(super) fn consume_ticket(
        &mut self,
        value: Uuid,
        now: Instant,
    ) -> Option<(Instant, Permissions)> {
        let State::Claimed {
            session,
            permissions,
            ticket,
        } = &mut self.0
        else {
            return None;
        };
        if !ticket.as_ref()?.accepts(value, now) {
            return None;
        }
        *ticket = None;
        Some((session.expires, *permissions))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_and_socket_tokens_can_each_be_spent_once() {
        let now = Instant::now();
        let (mut credentials, pairing) = Credentials::new(now);
        let permissions = Permissions {
            orchestration_operate: true,
            settings_write: true,
            filesystem_read: false,
            preview_operate: false,
            source_control_write: false,
            terminal_read: false,
            terminal_operate: true,
        };
        assert!(credentials.issue_ticket(pairing, now).is_none());
        assert!(
            credentials
                .exchange(Uuid::nil(), Permissions::default(), now)
                .is_none()
        );

        let session = credentials.exchange(pairing, permissions, now).unwrap();
        assert!(
            credentials
                .exchange(pairing, Permissions::default(), now)
                .is_none()
        );
        let (ticket, _) = credentials.issue_ticket(session, now).unwrap();
        assert!(credentials.consume_ticket(Uuid::nil(), now).is_none());
        assert_eq!(
            credentials.consume_ticket(ticket, now),
            Some((now + SESSION_LIFETIME, permissions))
        );
        assert!(credentials.consume_ticket(ticket, now).is_none());
        assert!(credentials.issue_ticket(session, now).is_some());
    }

    #[test]
    fn expired_secrets_have_no_authority() {
        let now = Instant::now();
        let (mut credentials, pairing) = Credentials::new(now);
        assert!(
            credentials
                .exchange(pairing, Permissions::default(), now + PAIRING_LIFETIME)
                .is_none()
        );

        let (mut credentials, pairing) = Credentials::new(now);
        let session = credentials
            .exchange(pairing, Permissions::default(), now)
            .unwrap();
        let (ticket, _) = credentials.issue_ticket(session, now).unwrap();
        assert!(
            credentials
                .consume_ticket(ticket, now + TICKET_LIFETIME)
                .is_none()
        );
        assert!(
            credentials
                .issue_ticket(session, now + SESSION_LIFETIME)
                .is_none()
        );
    }
}
