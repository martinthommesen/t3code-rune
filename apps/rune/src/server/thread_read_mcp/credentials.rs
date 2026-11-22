use std::collections::{HashMap, VecDeque};
use std::io;
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use uuid::Uuid;

const LIVENESS: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_SESSIONS_PER_PROVIDER: usize = 32;

#[derive(Clone)]
pub(in crate::server) struct ProviderScope {
    pub(in crate::server) thread_id: String,
    pub(in crate::server) provider_instance_id: String,
    pub(in crate::server) provider_session_id: String,
}

#[derive(Default)]
pub(super) struct Credentials {
    records: HashMap<[u8; 32], Record>,
}

struct Record {
    scope: ProviderScope,
    last_alive: Instant,
    expired: bool,
    sessions: VecDeque<Uuid>,
}

impl Credentials {
    pub(super) fn issue(&mut self, scope: ProviderScope, now: Instant) -> io::Result<String> {
        let mut random = [0_u8; 32];
        SystemRandom::new()
            .fill(&mut random)
            .map_err(|_| io::Error::other("The provider credential could not be generated."))?;
        let token = URL_SAFE_NO_PAD.encode(random);
        self.revoke_thread(&scope.thread_id);
        self.records.insert(
            hash(&token),
            Record {
                scope,
                last_alive: now,
                expired: false,
                sessions: VecDeque::new(),
            },
        );
        Ok(format!("Bearer {token}"))
    }

    pub(super) fn touch(&mut self, thread_id: &str, now: Instant) {
        for record in self
            .records
            .values_mut()
            .filter(|record| record.scope.thread_id == thread_id)
        {
            record.last_alive = now;
            record.expired = false;
        }
    }

    pub(super) fn revoke_thread(&mut self, thread_id: &str) {
        self.records
            .retain(|_, record| record.scope.thread_id != thread_id);
    }

    pub(super) fn revoke_session(&mut self, provider_session_id: &str) {
        self.records
            .retain(|_, record| record.scope.provider_session_id != provider_session_id);
    }

    pub(super) fn resolve(&mut self, token: &str, now: Instant) -> Option<ProviderScope> {
        self.active(token, now).map(|record| record.scope.clone())
    }

    pub(super) fn initialize(&mut self, token: &str, now: Instant) -> Option<Uuid> {
        let record = self.active(token, now)?;
        if record.sessions.len() == MAX_SESSIONS_PER_PROVIDER {
            record.sessions.pop_front();
        }
        let session = Uuid::new_v4();
        record.sessions.push_back(session);
        Some(session)
    }

    pub(super) fn has_session(&mut self, token: &str, session: Uuid, now: Instant) -> bool {
        self.active(token, now)
            .is_some_and(|record| record.sessions.contains(&session))
    }

    pub(super) fn close_session(&mut self, token: &str, session: Uuid, now: Instant) -> bool {
        let Some(record) = self.active(token, now) else {
            return false;
        };
        let Some(index) = record
            .sessions
            .iter()
            .position(|candidate| *candidate == session)
        else {
            return false;
        };
        record.sessions.remove(index);
        true
    }

    fn active(&mut self, token: &str, now: Instant) -> Option<&mut Record> {
        let record = self.records.get_mut(&hash(token))?;
        if now.saturating_duration_since(record.last_alive) > LIVENESS {
            record.expired = true;
        }
        if record.expired {
            return None;
        }
        record.last_alive = now;
        Some(record)
    }
}

fn hash(token: &str) -> [u8; 32] {
    digest::digest(&digest::SHA256, token.as_bytes())
        .as_ref()
        .try_into()
        .expect("SHA-256 uses 32 bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_activity_from_the_host_can_refresh_an_expired_registration() {
        let now = Instant::now();
        let mut credentials = Credentials::default();
        let scope = ProviderScope {
            thread_id: "thread-1".into(),
            provider_instance_id: "codex".into(),
            provider_session_id: "provider-1".into(),
        };
        let header = credentials.issue(scope.clone(), now).unwrap();
        let token = header.strip_prefix("Bearer ").unwrap();
        let expired = now + LIVENESS + Duration::from_secs(1);
        assert!(credentials.resolve(token, expired).is_none());
        assert!(credentials.resolve(token, expired).is_none());
        credentials.touch("thread-1", expired);
        assert!(credentials.resolve(token, expired).is_some());
        let renewed = credentials.issue(scope, expired).unwrap();
        assert_ne!(header, renewed);
        assert!(credentials.resolve(token, expired).is_none());
        credentials.revoke_session("provider-1");
        assert!(
            credentials
                .resolve(renewed.strip_prefix("Bearer ").unwrap(), expired)
                .is_none()
        );
    }
}
