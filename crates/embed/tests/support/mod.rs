//! Shared harness for the provider-pool tests.
//!
//! Everything here is deterministic by construction: no network, no wall-clock
//! sleeps (the pool's `Sleeper` seam is replaced by a recorder), no `$HOME`
//! dependency, no randomness.
//!
//! Each integration test binary compiles this module separately, so helpers a
//! given binary happens not to use are expected — not dead code.
#![allow(dead_code)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use local_rag_embed::{
    EmbedError, EmbedRequest, Embedder, FinishReason, GenError, GenRequest, GenResponse, Generator,
    HashingEmbedder, Sleeper, Vector,
};
use local_rag_store::{RepresentationKey, RepresentationKind};

/// A `Sleeper` that records the delays it was asked for instead of sleeping.
#[derive(Debug, Default)]
pub struct RecordingSleeper {
    delays: Mutex<Vec<u64>>,
}

impl RecordingSleeper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Delays requested so far, in order.
    pub fn delays(&self) -> Vec<u64> {
        self.delays.lock().expect("sleeper lock").clone()
    }
}

impl Sleeper for RecordingSleeper {
    fn sleep_ms(&self, ms: u64) {
        self.delays.lock().expect("sleeper lock").push(ms);
    }
}

/// One scripted provider response.
#[derive(Debug, Clone)]
pub enum Step {
    /// Succeed, tagging the vectors with `body` so a test can tell *which*
    /// scripted success answered.
    Ok(String),
    /// Fail transiently, optionally with a server-supplied delay hint.
    Retryable(String, Option<u64>),
    /// Fail permanently.
    Permanent(String),
}

/// A programmable `Embedder`: replays `steps`, then repeats `persistent`
/// forever (or, with no persistent step, keeps replaying the last one).
pub struct ScriptedEmbedder {
    name: String,
    key: RepresentationKey,
    steps: Mutex<std::collections::VecDeque<Step>>,
    persistent: Option<Step>,
    calls: AtomicUsize,
    last_request: Mutex<Option<EmbedRequest>>,
}

impl ScriptedEmbedder {
    pub fn new(name: &str, steps: Vec<Step>) -> Self {
        ScriptedEmbedder {
            name: name.to_string(),
            key: HashingEmbedder::new(RepresentationKind::CodeRaw).key(),
            steps: Mutex::new(steps.into()),
            persistent: None,
            calls: AtomicUsize::new(0),
            last_request: Mutex::new(None),
        }
    }

    /// A provider that always answers with `step`.
    pub fn persistent(name: &str, step: Step) -> Self {
        ScriptedEmbedder {
            name: name.to_string(),
            key: HashingEmbedder::new(RepresentationKind::CodeRaw).key(),
            steps: Mutex::new(Default::default()),
            persistent: Some(step),
            calls: AtomicUsize::new(0),
            last_request: Mutex::new(None),
        }
    }

    /// Override the representation key (used by the registry-match tests).
    pub fn with_key(mut self, key: RepresentationKey) -> Self {
        self.key = key;
        self
    }

    /// How many times `embed` was invoked.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// The `EmbedRequest` this provider actually received on its most recent
    /// call — what a test inspects to prove policy-driven payload transforms
    /// (redaction, T16-01) actually reached the provider, not just that it
    /// was called.
    pub fn last_request(&self) -> Option<EmbedRequest> {
        self.last_request.lock().expect("last_request lock").clone()
    }
}

impl Embedder for ScriptedEmbedder {
    fn embed(&self, req: EmbedRequest) -> Result<Vec<Vector>, EmbedError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_request.lock().expect("last_request lock") = Some(req.clone());
        let step = {
            let mut steps = self.steps.lock().expect("script lock");
            steps
                .pop_front()
                .or_else(|| self.persistent.clone())
                .unwrap_or_else(|| {
                    panic!("{}: script exhausted with no persistent step", self.name)
                })
        };
        match step {
            Step::Ok(body) => Ok(tagged_vectors(&body, &req)),
            Step::Retryable(message, after) => Err(EmbedError::Retryable {
                message,
                retry_after_ms: after,
            }),
            Step::Permanent(message) => Err(EmbedError::permanent(message)),
        }
    }

    fn key(&self) -> RepresentationKey {
        self.key.clone()
    }
}

/// The vectors a scripted success returns: deterministic, and dependent on both
/// the response body and the input text, so a test can assert *which* response
/// won and that results stayed positional.
pub fn tagged_vectors(body: &str, req: &EmbedRequest) -> Vec<Vector> {
    let embedder = HashingEmbedder::new(req.kind);
    req.texts
        .iter()
        .map(|t| embedder.embed_one(&format!("{body} {t}")))
        .collect()
}

/// A batch of code-shaped texts.
pub fn batch() -> Vec<String> {
    vec![
        "fn parse(input: &str) -> Result<Ast, Error>".to_string(),
        "export function handler(req, res) { return res.json({}) }".to_string(),
        "class Repository { find(id) { return this.rows.get(id) } }".to_string(),
    ]
}

/// One scripted generator response — the [`Step`] twin for [`Generator`].
#[derive(Debug, Clone)]
pub enum GenStep {
    Ok(String),
    Retryable(String, Option<u64>),
    Permanent(String),
}

/// A programmable `Generator`, mirroring [`ScriptedEmbedder`]: replays
/// `steps`, then repeats `persistent` forever; records the last
/// [`GenRequest`] it received so a test can inspect the actual message
/// content a provider was handed (redaction, T16-01).
pub struct ScriptedGenerator {
    name: String,
    steps: Mutex<std::collections::VecDeque<GenStep>>,
    persistent: Option<GenStep>,
    calls: AtomicUsize,
    last_request: Mutex<Option<GenRequest>>,
}

impl ScriptedGenerator {
    pub fn new(name: &str, steps: Vec<GenStep>) -> Self {
        ScriptedGenerator {
            name: name.to_string(),
            steps: Mutex::new(steps.into()),
            persistent: None,
            calls: AtomicUsize::new(0),
            last_request: Mutex::new(None),
        }
    }

    /// A provider that always answers with `step`.
    pub fn persistent(name: &str, step: GenStep) -> Self {
        ScriptedGenerator {
            name: name.to_string(),
            steps: Mutex::new(Default::default()),
            persistent: Some(step),
            calls: AtomicUsize::new(0),
            last_request: Mutex::new(None),
        }
    }

    /// How many times `generate` was invoked.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// The `GenRequest` this provider actually received on its most recent
    /// call.
    pub fn last_request(&self) -> Option<GenRequest> {
        self.last_request.lock().expect("last_request lock").clone()
    }
}

impl Generator for ScriptedGenerator {
    fn generate(&self, req: GenRequest) -> Result<GenResponse, GenError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_request.lock().expect("last_request lock") = Some(req.clone());
        let step = {
            let mut steps = self.steps.lock().expect("script lock");
            steps
                .pop_front()
                .or_else(|| self.persistent.clone())
                .unwrap_or_else(|| {
                    panic!("{}: script exhausted with no persistent step", self.name)
                })
        };
        match step {
            GenStep::Ok(text) => Ok(GenResponse {
                text,
                finish_reason: FinishReason::Stop,
                tokens_generated: None,
            }),
            GenStep::Retryable(message, after) => Err(GenError::Retryable {
                message,
                retry_after_ms: after,
            }),
            GenStep::Permanent(message) => Err(GenError::permanent(message)),
        }
    }
}

pub mod store;
