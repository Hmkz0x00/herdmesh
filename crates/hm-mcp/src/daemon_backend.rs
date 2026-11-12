//! [`Backend`] that forwards every call to the local daemon over IPC.
//! The daemon owns identity, roles, signing and fitting; this side only
//! tags calls with the agent's name.

use async_trait::async_trait;
use hm_daemon::ipc::{Call, Client, ErrCode, Items, Request, Response, Token};
use hm_proto::RoomId;

use crate::{Backend, BackendError};

/// One local agent's connection to the daemon.
#[derive(Debug, Clone)]
pub struct DaemonBackend {
    client: Client,
    agent: String,
    room: Option<RoomId>,
}

impl DaemonBackend {
    /// `socket` is the daemon's socket name (see `hm_daemon::ipc::socket_name`).
    pub fn new(socket: impl Into<String>, token: Token, agent: impl Into<String>) -> Self {
        Self {
            client: Client::new(socket, token),
            agent: agent.into(),
            room: None,
        }
    }

    /// Talk to a specific room instead of the daemon's default.
    pub fn in_room(mut self, room: RoomId) -> Self {
        self.room = Some(room);
        self
    }

    async fn call(&self, call: Call) -> Result<Items, BackendError> {
        let req = Request {
            agent: self.agent.clone(),
            room: self.room,
            call,
        };
        match self.client.request(&req).await {
            Ok(Response::Ok(items)) => Ok(items),
            Ok(Response::Err(ErrCode::Forbidden)) => Err(BackendError::NotPermitted),
            _ => Err(BackendError::Unavailable),
        }
    }
}

fn clamp(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[async_trait]
impl Backend for DaemonBackend {
    async fn room_card(&self) -> Result<Items, BackendError> {
        self.call(Call::RoomCard).await
    }
    async fn brief(&self, max_tokens: usize) -> Result<Items, BackendError> {
        self.call(Call::Brief {
            max_tokens: clamp(max_tokens),
        })
        .await
    }
    async fn inbox(&self, ack: bool) -> Result<Items, BackendError> {
        self.call(Call::Inbox { ack }).await
    }
    async fn ask(&self, to: String, question: String) -> Result<Items, BackendError> {
        self.call(Call::Ask { to, question }).await
    }
    async fn answer(&self, ask_id: String, text: String) -> Result<Items, BackendError> {
        self.call(Call::Answer { ask_id, text }).await
    }
    async fn handoff(
        &self,
        to: Option<String>,
        summary: String,
        refs: Vec<String>,
        next_steps: Vec<String>,
    ) -> Result<Items, BackendError> {
        self.call(Call::Handoff {
            to,
            summary,
            refs,
            next_steps,
        })
        .await
    }
    async fn claim(&self, paths: Vec<String>, ttl_min: u32) -> Result<Items, BackendError> {
        self.call(Call::Claim { paths, ttl_min }).await
    }
    async fn release(&self, paths: Vec<String>) -> Result<Items, BackendError> {
        self.call(Call::Release { paths }).await
    }
    async fn task_add(
        &self,
        task: String,
        title: String,
        paths: Vec<String>,
    ) -> Result<Items, BackendError> {
        self.call(Call::TaskAdd { task, title, paths }).await
    }
    async fn task_update(
        &self,
        task: String,
        status: String,
        note: Option<String>,
    ) -> Result<Items, BackendError> {
        self.call(Call::TaskUpdate { task, status, note }).await
    }
    async fn task_list(&self, filter: Option<String>) -> Result<Items, BackendError> {
        self.call(Call::TaskList { filter }).await
    }
    async fn decide(
        &self,
        title: String,
        rationale: String,
        refs: Vec<String>,
    ) -> Result<Items, BackendError> {
        self.call(Call::Decide {
            title,
            rationale,
            refs,
        })
        .await
    }
    async fn get_event(&self, id: String) -> Result<Items, BackendError> {
        self.call(Call::GetEvent { id }).await
    }
    async fn search(&self, query: String, limit: usize) -> Result<Items, BackendError> {
        self.call(Call::Search {
            query,
            limit: clamp(limit),
        })
        .await
    }
}
