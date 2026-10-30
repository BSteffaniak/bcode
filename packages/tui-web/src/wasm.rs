//! WASM entrypoint. The JS owner must serialize calls and await presentation.
use wasm_bindgen::prelude::*;

use bmux_tui::damage::Damage;
use bmux_web_renderer::CanvasRenderer;

use crate::SessionSurface;

/// One browser-local session. Async rendering takes ownership of the surface,
/// so reentrant calls fail closed rather than borrowing mutable state over await.
#[wasm_bindgen]
pub struct BrowserSession {
    surface: Option<SessionSurface>,
    canvas: Option<CanvasRenderer>,
    disposed: bool,
}

fn error(message: &str) -> JsValue {
    JsValue::from_str(message)
}

#[wasm_bindgen]
impl BrowserSession {
    /// Bind to an explicitly supplied session and existing canvas.
    ///
    /// # Errors
    /// Rejects invalid identities, cell budgets, metrics or missing Canvas.
    #[wasm_bindgen(constructor)]
    pub fn new(
        session: String,
        canvas_id: &str,
        width: u16,
        height: u16,
        cell_width: f64,
        cell_height: f64,
    ) -> Result<Self, JsValue> {
        Ok(Self {
            surface: Some(SessionSurface::new(session, width, height).map_err(error)?),
            canvas: Some(CanvasRenderer::new(canvas_id, cell_width, cell_height)?),
            disposed: false,
        })
    }

    /// Accept the original response bytes as UTF-8; JS must not parse/reencode u64s.
    ///
    /// # Errors
    /// Rejects disposed/busy state or invalid/stale semantic observations.
    pub fn accept_snapshot(&mut self, json: &str) -> Result<bool, JsValue> {
        self.surface()?.accept_snapshot(json).map_err(error)
    }

    /// A normalized BMUX bridge event. Pointer coordinates are cell coordinates.
    ///
    /// # Errors
    /// Rejects disposed/busy state or malformed/oversized browser input.
    pub fn input(&mut self, json: &str) -> Result<bool, JsValue> {
        self.surface()?.input(json).map_err(error)
    }

    /// Drain bounded semantic host work after input; never executes platform work.
    ///
    /// # Errors
    /// Rejects disposed/busy state or serialization failure.
    pub fn take_intents(&mut self) -> Result<String, JsValue> {
        self.surface()?.take_intents().map_err(error)
    }

    /// Accessible metadata from the last successfully committed BMUX scene.
    ///
    /// # Errors
    /// Rejects disposed/busy state or serialization failure.
    pub fn accessibility(&mut self) -> Result<String, JsValue> {
        self.surface()?.accessibility().map_err(error)
    }

    /// Resize cells and physical pixel metrics after the previous render settles.
    ///
    /// # Errors
    /// Rejects disposed/busy state, invalid metrics or excessive cell budgets.
    pub fn resize(
        &mut self,
        width: u16,
        height: u16,
        cell_width: f64,
        cell_height: f64,
        font: String,
        baseline: f64,
    ) -> Result<(), JsValue> {
        if self.disposed {
            return Err(error("surface disposed"));
        }
        self.canvas
            .as_mut()
            .ok_or_else(|| error("presentation in progress"))?
            .set_metrics(cell_width, cell_height, font, baseline)?;
        self.surface()?.resize(width, height).map_err(error)
    }

    /// Paint actual BMUX components directly to Canvas, without frame decoding.
    ///
    /// # Errors
    /// Rejects disposed/busy state, presentation limits or Canvas output failure.
    // Browser DOM futures are single-thread-local by platform definition.
    #[allow(clippy::future_not_send)]
    pub async fn present(&mut self) -> Result<bool, JsValue> {
        if self.disposed {
            return Err(error("surface disposed"));
        }
        let mut surface = self
            .surface
            .take()
            .ok_or_else(|| error("presentation in progress"))?;
        let Some(mut canvas) = self.canvas.take() else {
            self.surface = Some(surface);
            return Err(error("presentation in progress"));
        };
        surface.prepare();
        let inner = &mut surface.inner;
        let result = surface
            .local
            .present_canvas(&mut canvas, Damage::Full, |cx| inner.paint(cx))
            .await;
        if matches!(result, Ok(true)) {
            surface.commit_output();
        }
        self.surface = Some(surface);
        self.canvas = Some(canvas);
        result
    }

    /// Capture a submission identity generated once by browser crypto.randomUUID.
    /// If an operation remains unresolved, returns its exact original payload.
    ///
    /// # Errors
    /// Rejects disposed/busy state, invalid identity, empty or oversized drafts.
    pub fn prepare_submission(&mut self, identity: String) -> Result<String, JsValue> {
        let surface = self.surface()?;
        let draft = surface.draft().to_owned();
        surface
            .inner
            .semantic
            .prepare_submission(identity, draft)
            .map_err(error)
    }

    /// Read the real BMUX editor source for host accessibility and diagnostics.
    ///
    /// # Errors
    /// Rejects disposed/busy state.
    pub fn draft(&mut self) -> Result<String, JsValue> {
        Ok(self.surface()?.draft().to_owned())
    }

    /// Configure bounded explicit browser application key overrides.
    ///
    /// # Errors
    /// Rejects disposed/busy state or malformed configuration.
    pub fn configure_bindings(&mut self, json: &str) -> Result<(), JsValue> {
        self.surface()?.configure_bindings(json).map_err(error)
    }

    /// Take an explicitly requested canonical source clipboard payload.
    ///
    /// # Errors
    /// Rejects disposed/busy state.
    pub fn take_clipboard(&mut self) -> Result<Option<String>, JsValue> {
        Ok(self.surface()?.take_clipboard())
    }

    /// Exact retry payload, never a regenerated identity.
    ///
    /// # Errors
    /// Rejects disposed/busy state.
    pub fn retry_submission(&mut self) -> Result<Option<String>, JsValue> {
        Ok(self.surface()?.inner.semantic.retry_submission())
    }

    /// Clear only the draft corresponding to a verified durable receipt.
    ///
    /// # Errors
    /// Rejects disposed/busy state, malformed responses or unmatched receipts.
    pub fn settle_submission(&mut self, json: &str) -> Result<(), JsValue> {
        self.surface()?.settle_submission(json).map_err(error)
    }

    /// Prepare one bounded semantic history request from the accepted snapshot.
    ///
    /// # Errors
    /// Rejects absent snapshots or unsupported directions.
    pub fn prepare_history(&mut self, direction: &str) -> Result<String, JsValue> {
        self.surface()?
            .inner
            .semantic
            .prepare_history(direction)
            .map_err(error)
    }

    /// Prepare a canonical artifact image, retaining only an async generation token.
    ///
    /// # Errors
    /// Rejects disposed/busy state.
    pub fn prepare_image(&mut self, item_id: &str, reference: &str) -> Result<bool, JsValue> {
        Ok(self.surface()?.prepare_image(item_id, reference))
    }

    /// Admit decoded bounded RGBA pixels through shared generation checks.
    ///
    /// # Errors
    /// Rejects disposed/busy state; stale or invalid pixels return false.
    pub fn accept_image(
        &mut self,
        bytes: Vec<u8>,
        width: u32,
        height: u32,
    ) -> Result<bool, JsValue> {
        Ok(self.surface()?.accept_image(bytes, width, height))
    }

    /// Prepare a bounded canonical artifact range from an accepted semantic reference.
    ///
    /// # Errors
    /// Rejects disposed, stale or unsupported artifact references and invalid bounds.
    pub fn prepare_artifact(
        &mut self,
        item_id: &str,
        reference_key: &str,
        offset: u64,
        length: u32,
    ) -> Result<String, JsValue> {
        self.surface()?
            .inner
            .semantic
            .prepare_artifact(item_id, reference_key, offset, length)
            .map_err(error)
    }

    /// Accept a bounded history response with directional convergence checks.
    ///
    /// # Errors
    /// Rejects stale, malformed or non-progressing history windows.
    pub fn accept_history(&mut self, json: &str, direction: &str) -> Result<bool, JsValue> {
        self.surface()?
            .accept_history(json, direction)
            .map_err(error)
    }

    /// Validate one explicit pending control against accepted application state.
    ///
    /// # Errors
    /// Rejects stale identities, unsupported actions and oversized payloads.
    pub fn prepare_control(&mut self, json: &str) -> Result<String, JsValue> {
        self.surface()?
            .inner
            .semantic
            .prepare_control(json)
            .map_err(error)
    }

    /// Acknowledge a refreshed unknown control without replaying it.
    ///
    /// # Errors
    /// Rejects disposed or busy presentation state.
    pub fn acknowledge_control(&mut self) -> Result<(), JsValue> {
        self.surface()?.acknowledge_control();
        Ok(())
    }

    /// Verify the outcome matches the exact control sent by this host.
    ///
    /// # Errors
    /// Rejects malformed or unrelated responses; does not automatically retry.
    pub fn accept_control(&mut self, action: &str, response: &str) -> Result<(), JsValue> {
        self.surface()?
            .inner
            .semantic
            .accept_control(action, response)
            .map_err(error)
    }

    /// Export only bounded caller-owned draft/selection and unresolved action state.
    ///
    /// # Errors
    /// Rejects disposed/busy state or oversized recovery.
    pub fn export_recovery(&mut self) -> Result<String, JsValue> {
        self.surface()?.export_recovery().map_err(error)
    }

    /// Restore into a fresh surface without observations or interaction geometry.
    ///
    /// # Errors
    /// Rejects disposed/busy state or invalid recovery.
    pub fn restore_recovery(&mut self, json: &str) -> Result<(), JsValue> {
        self.surface()?.restore_recovery(json).map_err(error)
    }

    /// Cancel browser capture before focus loss or disposal.
    ///
    /// # Errors
    /// Rejects disposed/busy state.
    pub fn cancel_pointer(&mut self) -> Result<(), JsValue> {
        self.surface()?.cancel_pointer();
        Ok(())
    }

    /// Call after queued presentation settles; JS must abort fetches and remove
    /// bridge listeners first. This releases retained plugin/semantic snapshots.
    ///
    /// # Errors
    /// Rejects disposal while a presentation is in progress.
    pub fn dispose(&mut self) -> Result<(), JsValue> {
        if self.disposed {
            return Ok(());
        }
        self.surface()?.cancel_pointer();
        self.surface = None;
        self.canvas = None;
        self.disposed = true;
        Ok(())
    }
}

impl BrowserSession {
    fn surface(&mut self) -> Result<&mut SessionSurface, JsValue> {
        if self.disposed {
            return Err(error("surface disposed"));
        }
        self.surface
            .as_mut()
            .ok_or_else(|| error("presentation in progress"))
    }
}
