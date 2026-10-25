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
        // A hidden/unmeasured embed has no browser frame. The shared frame
        // contract rejects empty cell grids; resume painting after a real resize.
        if self.surface()?.local.presentation().area().is_empty() {
            return Ok(false);
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
