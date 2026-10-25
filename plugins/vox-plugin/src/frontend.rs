//! Direct client-local adapter, intentionally separate from native daemon exports.

use crate::service::{self, LocalControl};
use bcode_plugin_sdk::{ServiceRequest, ServiceResponse};
use std::{marker::PhantomData, rc::Rc};

/// Consume an explicitly running standalone Vox service on this client machine.
///
/// No daemon discovery, process launch, cloud fallback, or transport retry is
/// performed. Local service ownership and frame validation belong to Vox IPC.
#[derive(Default)]
pub struct LocalService;

impl LocalControl for LocalService {
    fn action(&mut self, action: bcode_vox::contracts::Action) -> bcode_vox::Result<()> {
        bcode_vox::service::action(action).map(|_| ())
    }

    fn status(&self) -> bcode_vox::Result<bcode_vox::contracts::Status> {
        bcode_vox::service::status()
    }
}

impl LocalControl for bcode_vox::runtime::Handle {
    fn action(&mut self, action: bcode_vox::contracts::Action) -> bcode_vox::Result<()> {
        Self::action(self, action).map(|_| ())
    }

    fn status(&self) -> bcode_vox::Result<bcode_vox::contracts::Status> {
        Ok(Self::status(self))
    }
}

/// Client-owned service binding. Construction does not start recording.
///
/// The host must create this only in its local frontend process, with an
/// explicitly configured local runtime. This type is neither `Send` nor `Sync`:
/// it is not a transport endpoint or a daemon attachment. No payload can create
/// it, and the native plugin never retains it. The runtime still owns its
/// capture/recognition worker and enforces a single operation.
pub struct LocalFrontend<C: LocalControl> {
    control: C,
    _client_thread: PhantomData<Rc<()>>,
}

impl<C: LocalControl> LocalFrontend<C> {
    /// Bind an already configured local runtime without touching the microphone.
    #[must_use]
    pub const fn new(control: C) -> Self {
        Self {
            control,
            _client_thread: PhantomData,
        }
    }

    /// Invoke from an explicit user command/action, never from model tool routing.
    ///
    /// The host must check enabled-plugin ownership before selecting this adapter.
    /// Requests are never forwarded to a daemon or another attached client.
    #[must_use]
    pub fn invoke(&mut self, request: &ServiceRequest) -> ServiceResponse {
        service::dispatch(request, Some(&mut self.control))
    }

    /// Cancel on frontend shutdown; report failure instead of silently claiming
    /// capture stopped. Explicit shutdown avoids hidden IO in `Drop`.
    ///
    /// # Errors
    /// Returns a normalized service error when the local runtime rejects
    /// cancellation, the standalone service is unreachable, or its transport,
    /// ownership, framing, or compatibility validation fails. A transport failure
    /// may follow accepted cancellation; this method never retries it.
    pub fn shutdown(&mut self) -> Result<(), ServiceResponse> {
        let request = ServiceRequest {
            interface_id: service::INTERFACE_ID.into(),
            operation: "action".into(),
            payload: br#"{"version":1,"action":"cancel"}"#.to_vec(),
        };
        let response = self.invoke(&request);
        if response.error.is_some() {
            Err(response)
        } else {
            Ok(())
        }
    }
}
