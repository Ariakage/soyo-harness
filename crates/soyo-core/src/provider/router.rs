//! Route execution and independent review requests to explicitly configured profiles.
use super::{failure, routing::RoutingCoordinator};
use crate::{
    boundary::{Cancellation, Model, ModelRequest, ModelStream, Operation},
    contract::{BoundaryError, ModelPurpose, Session},
};
use std::{collections::BTreeMap, sync::Arc};

/// Immutable profile registry for one execution session. Review may select a
/// different registered profile; when opt-in routing (`F-16`) is disabled,
/// execution cannot switch its profile mid-run.
pub struct ProfileRouter {
    execution_profile: String,
    profiles: BTreeMap<String, Arc<dyn Model>>,
    routing: Option<Arc<RoutingCoordinator>>,
}
impl ProfileRouter {
    /// Construct a registry with an existing execution profile. Missing or
    /// duplicate profiles must be rejected by the caller while building the map.
    pub fn new(
        execution_profile: String,
        profiles: BTreeMap<String, Arc<dyn Model>>,
    ) -> Result<Self, BoundaryError> {
        if execution_profile.is_empty()
            || !profiles.contains_key(&execution_profile)
            || profiles.keys().any(String::is_empty)
        {
            return Err(failure(
                "invalid_config",
                "execution profile is missing or invalid",
            ));
        }
        Ok(Self {
            execution_profile,
            profiles,
            routing: None,
        })
    }

    /// Attach an opt-in routing coordinator (`F-16`).
    pub fn with_routing(mut self, routing: Arc<RoutingCoordinator>) -> Self {
        self.routing = Some(routing);
        self
    }

    /// Inspect the attached routing coordinator, if any.
    pub fn routing(&self) -> Option<&Arc<RoutingCoordinator>> {
        self.routing.as_ref()
    }
}
impl crate::context_builder::RequestCounter for ProfileRouter {
    fn count(
        &self,
        request: &ModelRequest,
        identity: &crate::context_builder::CountingIdentity,
        fingerprint: &str,
    ) -> Result<crate::context_builder::CountEvidence, BoundaryError> {
        self.profiles
            .get(&request.profile)
            .and_then(|model| model.request_counter())
            .ok_or_else(|| {
                failure(
                    "context_counter",
                    "Selected profile has no local wire counter",
                )
            })?
            .count(request, identity, fingerprint)
    }
}
impl Model for ProfileRouter {
    fn request_counter(&self) -> Option<&dyn crate::context_builder::RequestCounter> {
        Some(self)
    }

    fn model_routing(&self) -> Option<&RoutingCoordinator> {
        self.routing.as_deref()
    }

    fn validate_session(&self, session: &Session) -> Result<(), BoundaryError> {
        if session.profile != self.execution_profile {
            return Err(failure(
                "invalid_config",
                "execution profile differs from the session",
            ));
        }
        self.profiles
            .get(&self.execution_profile)
            .ok_or_else(|| failure("invalid_config", "execution profile is unavailable"))?
            .validate_session(session)
    }
    fn start<'a>(
        &'a self,
        request: &'a ModelRequest,
        cancel: &'a dyn Cancellation,
    ) -> Operation<'a, Box<dyn ModelStream>> {
        Box::pin(async move {
            let routing_enabled = self
                .routing
                .as_ref()
                .is_some_and(|routing| routing.is_enabled());
            if !routing_enabled
                && request.purpose != ModelPurpose::Review
                && request.profile != self.execution_profile
            {
                return Err(failure(
                    "invalid_config",
                    "execution profile cannot change during a session",
                ));
            }
            if request.purpose != ModelPurpose::Execution && !request.tools.is_empty() {
                return Err(failure(
                    "invalid_config",
                    "review and compaction requests cannot contain tools",
                ));
            }
            let model = self.profiles.get(&request.profile).ok_or_else(|| {
                failure("invalid_config", "requested model profile is unavailable")
            })?;
            model.start(request, cancel).await
        })
    }
}
