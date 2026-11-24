//! Stable socket-client SDK for the `soyo helper` daemon (F-13).
//!
//! This crate is the supported embedding surface decided in D-105: a thin,
//! stable facade over `soyo_ipc::HelperClient`, talking the frozen wire
//! contract (`PROTOCOL_VERSION` + `PUBLISHED_METHODS`, D-106) rather than any
//! internal Rust API. Runs, sessions and tasks still execute inside the
//! helper — the SDK never reproduces the agent loop or its permission/sandbox
//! boundaries.
//!
//! Disconnect semantics follow D-107: dropping or [`SoyoClient::detach`]ing a
//! connection does **not** cancel an in-flight run — runs belong to sessions,
//! not connections — and [`SoyoClient::attach`] re-binds to a session to resume
//! event observation.

mod client;
mod error;

pub use client::{RunEventStream, RunHandle, SoyoClient};
pub use error::SdkError;

// Re-export the wire DTOs and endpoint types the facade exposes, so consumers
// program against one crate while the schema stays single-sourced in soyo-ipc.
pub use soyo_ipc::{
    socket::HelperTarget,
    types::{
        ActiveSessionDto, AttachmentTokenDto, BootstrapStateDto, DesktopStreamEnvelope,
        HelperInfoDto, PermissionModeDto, PersonalContextDto, ResumeSessionDto,
        ResumeSessionResultDto, RunPromptParams, RunStateDto, SessionItemDto, SessionPageDto,
        SubmitPromptParams, SubmitPromptResultDto, TaskDto, TaskJobDto, TaskRunSummaryDto,
        UsageDto, WorkModeDto,
    },
};

/// Wire protocol version this SDK speaks (`soyo_ipc::PROTOCOL_VERSION`).
pub const PROTOCOL_VERSION: u32 = soyo_ipc::PROTOCOL_VERSION;

/// Frozen method surface this SDK's facade covers (D-106).
pub const PUBLISHED_METHODS: &[&str] = soyo_ipc::protocol::PUBLISHED_METHODS;
