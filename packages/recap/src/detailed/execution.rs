//! Pre-reserved bounded waves of independent model calls; no spawned tasks or retries.
use std::{
    future::{Future, poll_fn},
    pin::Pin,
    task::Poll,
};

use serde_json::Value;

use super::{
    AnalysisRequest, AnalysisResponse, AnalysisStage, CompositionError, ImageAsset, Operation,
    POLICY, Result, check, check_cancelled,
};

type SendFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type PendingCall<'a> = SendFuture<'a, Result<AnalysisResponse>>;
type CallIterator<'a> = Box<dyn Iterator<Item = Result<PreparedCall>> + Send + 'a>;

pub(super) struct PreparedCall {
    request: AnalysisRequest,
    input_bytes: usize,
    image_bytes: usize,
    in_flight_bytes: usize,
}

impl PreparedCall {
    pub fn new(
        stage: AnalysisStage,
        input: Value,
        images: Vec<ImageAsset>,
        task: &str,
        max_response_bytes: usize,
    ) -> Result<Self> {
        let instructions = format!("{POLICY}\n{task}");
        let input_bytes = serde_json::to_vec(&input)?
            .len()
            .checked_add(instructions.len())
            .ok_or(CompositionError::Budget)?;
        let image_bytes = images.iter().try_fold(0usize, |total, image| {
            total
                .checked_add(image.bytes.len())
                .ok_or(CompositionError::Budget)
        })?;
        let in_flight_bytes = input_bytes
            .checked_add(image_bytes)
            .and_then(|total| total.checked_add(max_response_bytes))
            .ok_or(CompositionError::Budget)?;
        Ok(Self {
            request: AnalysisRequest {
                stage,
                input,
                instructions,
                images,
                max_response_bytes,
            },
            input_bytes,
            image_bytes,
            in_flight_bytes,
        })
    }
}

impl Operation<'_> {
    fn reserve(&mut self, wave: &[PreparedCall]) -> Result<()> {
        self.cancelled()?;
        let limits = &self.policy.allowances;
        let mut calls = self.calls;
        let mut input = self.input_bytes;
        let mut images = self.image_bytes;
        for call in wave {
            if call.input_bytes > limits.max_input_bytes
                || call.image_bytes > limits.max_image_bytes
            {
                return Err(CompositionError::Budget);
            }
            calls = calls.checked_add(1).ok_or(CompositionError::Budget)?;
            input = input
                .checked_add(call.input_bytes)
                .ok_or(CompositionError::Budget)?;
            images = images
                .checked_add(call.image_bytes)
                .ok_or(CompositionError::Budget)?;
        }
        let total_input = limits
            .max_input_bytes
            .checked_mul(limits.max_model_calls)
            .ok_or(CompositionError::Budget)?;
        let total_images = limits
            .max_image_bytes
            .checked_mul(limits.max_model_calls)
            .ok_or(CompositionError::Budget)?;
        if calls > limits.max_model_calls || input > total_input || images > total_images {
            return Err(CompositionError::Budget);
        }
        self.calls = calls;
        self.input_bytes = input;
        self.image_bytes = images;
        Ok(())
    }

    fn execute_wave(
        &self,
        wave: Vec<PreparedCall>,
    ) -> SendFuture<'_, Result<Vec<Option<Result<AnalysisResponse>>>>> {
        Box::pin(async move {
            let stages: Vec<_> = wave.iter().map(|call| call.request.stage).collect();
            for stage in &stages {
                let (phase, index) = progress(*stage);
                self.cancel.emit(crate::diagnostics::Event::Phase { phase });
                self.cancel
                    .emit(crate::diagnostics::Event::AnalysisProgress {
                        phase,
                        completed: self.completed[index],
                        total: None,
                    });
            }
            let executor = self.executor;
            let cancel = self.cancel;
            let mut futures: Vec<PendingCall<'_>> = wave
                .into_iter()
                .map(|call| {
                    Box::pin(async move {
                        check_cancelled(cancel)?;
                        cancel.emit(crate::diagnostics::Event::InferenceStarted);
                        let response = executor.execute_analysis(call.request, cancel).await?;
                        check_cancelled(cancel)?;
                        Ok(response)
                    }) as PendingCall<'_>
                })
                .collect();
            let mut results: Vec<Option<Result<AnalysisResponse>>> =
                (0..futures.len()).map(|_| None).collect();
            // Drain dispatched work even after an error. Dropping a future does not
            // prove that provider work stopped. No subsequent wave is admitted.
            poll_fn(|cx| {
                for (future, result) in futures.iter_mut().zip(&mut results) {
                    if result.is_none()
                        && let Poll::Ready(value) = future.as_mut().poll(cx)
                    {
                        *result = Some(value);
                    }
                }
                if results.iter().all(Option::is_some) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            self.cancelled()?;
            Ok(results)
        })
    }

    pub(super) fn run<'a>(
        &'a mut self,
        mut calls: CallIterator<'a>,
    ) -> SendFuture<'a, Result<Vec<AnalysisResponse>>> {
        Box::pin(async move {
            let mut pending = None;
            let mut responses = Vec::new();
            loop {
                self.cancelled()?;
                let mut wave = Vec::new();
                let mut bytes = 0usize;
                while wave.len() < self.policy.max_concurrent_requests {
                    let Some(call) = pending.take().or_else(|| calls.next()) else {
                        break;
                    };
                    let call = call?;
                    if call.in_flight_bytes > self.policy.max_in_flight_bytes {
                        return Err(CompositionError::Budget);
                    }
                    let total = bytes
                        .checked_add(call.in_flight_bytes)
                        .ok_or(CompositionError::Budget)?;
                    if total > self.policy.max_in_flight_bytes {
                        pending = Some(Ok(call));
                        break;
                    }
                    bytes = total;
                    wave.push(call);
                }
                if wave.is_empty() {
                    break;
                }
                self.reserve(&wave)?;
                let stages: Vec<_> = wave.iter().map(|call| call.request.stage).collect();
                let results = self.execute_wave(wave).await?;
                for (result, stage) in results.into_iter().zip(stages) {
                    let response = result.expect("all wave futures settled")?;
                    check(response.stage() == stage, "analysis stage mismatch")?;
                    let size = match &response {
                        AnalysisResponse::ExtractionInventory(value) => serde_json::to_vec(value)?,
                        AnalysisResponse::Outline(value) => serde_json::to_vec(value)?,
                        AnalysisResponse::ChapterComposition(value) => serde_json::to_vec(value)?,
                        AnalysisResponse::SourceAudit(value) => serde_json::to_vec(value)?,
                        AnalysisResponse::SourceRepair(value) => serde_json::to_vec(value)?,
                        AnalysisResponse::VisualInspection(value) => serde_json::to_vec(value)?,
                    }
                    .len();
                    if size > self.policy.allowances.max_response_bytes {
                        return Err(CompositionError::Budget);
                    }
                    let (phase, index) = progress(stage);
                    self.completed[index] = self.completed[index].saturating_add(1);
                    self.cancel
                        .emit(crate::diagnostics::Event::AnalysisProgress {
                            phase,
                            completed: self.completed[index],
                            total: None,
                        });
                    responses.push(response);
                }
            }
            Ok(responses)
        })
    }
}

const fn progress(stage: AnalysisStage) -> (crate::diagnostics::Phase, usize) {
    use crate::diagnostics::Phase;
    match stage {
        AnalysisStage::ExtractionInventory
        | AnalysisStage::VisualInspection
        | AnalysisStage::Outline => (Phase::Analysis, 0),
        AnalysisStage::ChapterComposition => (Phase::Writing, 1),
        AnalysisStage::SourceAudit => (Phase::Audit, 2),
        AnalysisStage::SourceRepair => (Phase::Repair, 3),
    }
}
