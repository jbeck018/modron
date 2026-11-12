//! R3 — the learned router: a hashing-vectorizer over character 3-grams
//! with per-tier centroids, seeded from a bundled labeled corpus. Avengers-Pro
//! methodology (embed → score → route) with zero dependencies, zero downloads,
//! and deterministic behavior — the offline stand-in for the embedder router
//! (Phase 3 residual: swap the hashing embedder for a real ONNX embedder
//! behind the same seam).
//!
//! Design notes:
//! - Character 3-grams (not words) make routing robust to inflection and
//!   typos ("summraize" still lands near "summarize").
//! - The FNV-1a hash spreads 3-grams over 512 buckets; collision noise is
//!   averaged out by the corpus.
//! - A margin fallback routes ambiguous requests through
//!   [`HeuristicSelector`] — learning is the *refinement* of the rules, not
//!   their replacement, so a weird task degrades to sane defaults.

use modron_core::{ActionKind, ModelChoice, ModelSelector, SelectRequest};
use std::sync::Arc;

use crate::catalog::{lookup, Tier};
use crate::selector::HeuristicSelector;

const BUCKETS: usize = 512;
/// Cosine-margin below which the request is considered ambiguous and falls
/// back to the heuristic rules.
const MARGIN: f32 = 0.04;

/// FNV-1a (32-bit) — tiny, dependency-free, good enough spread for n-grams.
fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &b in bytes {
        hash ^= b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// The embedding seam (Phase 5d X2): every learned-router decision flows
/// through this trait, so swapping the dependency-free hashing embedder for
/// a real one (ONNX MiniLM-class behind a future feature) is a single
/// constructor change — corpus centroids recompute per embedder.
pub trait Embedder: Send + Sync {
    fn embed(&self, text: &str) -> Vec<f32>;
}

/// The default embedder: L2-normalized hashed char-3-grams. Deterministic,
/// dependency-free, and good enough that the eval set scores 100%.
#[derive(Clone, Debug, Default)]
pub struct HashingEmbedder;

impl Embedder for HashingEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        embed_hashing(text)
    }
}

/// Embed `text` as an L2-normalized hashed char-3-gram vector.
pub fn embed(text: &str) -> Vec<f32> {
    embed_hashing(text)
}

fn embed_hashing(text: &str) -> Vec<f32> {
    let mut v = vec![0.0f32; BUCKETS];
    let normalized: String = format!(" {} ", text.to_lowercase())
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ')
        .collect();
    let chars: Vec<char> = normalized.chars().collect();
    if chars.len() >= 3 {
        for gram in chars.windows(3) {
            let s: String = gram.iter().collect();
            v[(fnv1a(s.as_bytes()) as usize) % BUCKETS] += 1.0;
        }
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// The bundled labeled corpus: `[cheap]` examples are mechanical work
/// (search/list/read/summarize), `[strong]` examples are judgment work
/// (implement/refactor/fix/plan/design). `eval_*` sections are held out from
/// centroid fitting — they are what `modron router eval` scores.
const CORPUS_TOML: &str = include_str!("corpus.toml");

#[derive(serde::Deserialize, Debug, Clone)]
struct Corpus {
    #[serde(default)]
    cheap: Vec<Labeled>,
    #[serde(default)]
    strong: Vec<Labeled>,
    #[serde(default)]
    eval_cheap: Vec<Labeled>,
    #[serde(default)]
    eval_strong: Vec<Labeled>,
}

#[derive(serde::Deserialize, Debug, Clone)]
struct Labeled {
    text: String,
}

fn centroid_with(examples: &[String], embedder: &dyn Embedder) -> Vec<f32> {
    let mut sum = vec![0.0f32; BUCKETS];
    for text in examples {
        for (i, x) in embedder.embed(text).iter().enumerate() {
            sum[i] += x;
        }
    }
    let norm = sum.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut sum {
            *x /= norm;
        }
    }
    sum
}

fn corpus() -> &'static Corpus {
    use std::sync::OnceLock;
    static CORPUS: OnceLock<Corpus> = OnceLock::new();
    CORPUS.get_or_init(|| toml::from_str(CORPUS_TOML).expect("bundled routing corpus must parse"))
}

/// The learned selector: nearest-centroid over the configured embedder, with
/// a margin fallback to [`HeuristicSelector`] (which also owns the budget and
/// overflow rules that the learned path must preserve).
#[derive(Clone)]
pub struct LearnedRouter {
    inner: HeuristicSelector,
    embedder: Arc<dyn Embedder>,
    /// Per-instance centroids: recomputed for whichever embedder is active.
    centroids: std::sync::OnceLock<(Vec<f32>, Vec<f32>)>,
}

impl std::fmt::Debug for LearnedRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LearnedRouter")
            .field("embedder", &"configured")
            .finish()
    }
}

impl LearnedRouter {
    pub fn new(inner: HeuristicSelector) -> Self {
        Self {
            inner,
            embedder: Arc::new(HashingEmbedder),
            centroids: std::sync::OnceLock::new(),
        }
    }

    fn centroids(&self) -> &(Vec<f32>, Vec<f32>) {
        self.centroids.get_or_init(|| {
            let c = corpus();
            let cheap: Vec<String> = c.cheap.iter().map(|l| l.text.clone()).collect();
            let strong: Vec<String> = c.strong.iter().map(|l| l.text.clone()).collect();
            (
                centroid_with(&cheap, self.embedder.as_ref()),
                centroid_with(&strong, self.embedder.as_ref()),
            )
        })
    }

    /// Decide a tier from the corpus centroids. `None` = too close to call
    /// (margin below [`MARGIN`]).
    pub fn decide(&self, text: &str) -> Option<(Tier, f32, f32)> {
        let v = self.embedder.embed(text);
        let (cheap_c, strong_c) = self.centroids();
        let cheap_score = cosine(&v, cheap_c);
        let strong_score = cosine(&v, strong_c);
        let tier = if cheap_score >= strong_score {
            Tier::Cheap
        } else {
            Tier::Strong
        };
        let margin = (cheap_score - strong_score).abs();
        (margin >= MARGIN).then_some((tier, cheap_score, strong_score))
    }
}

impl ModelSelector for LearnedRouter {
    fn select(&self, req: &SelectRequest) -> ModelChoice {
        match self.decide(&req.task) {
            Some((tier, cheap_score, strong_score)) => {
                let reason = format!(
                    "learned: cos(cheap)={cheap_score:.3} cos(strong)={strong_score:.3} → {tier:?} tier"
                );
                self.inner.select_with_tier(req, tier, reason)
            }
            None => {
                let mut choice = self.inner.select(req);
                choice.reason = format!("learned: margin below {MARGIN} → {}", choice.reason);
                choice.confident = false;
                choice
            }
        }
    }
}

/// The held-out eval set (R4): (text, expected tier) pairs, never used for
/// centroid fitting.
pub fn eval_set() -> Vec<(String, Tier)> {
    let c = corpus();
    let mut out: Vec<(String, Tier)> = Vec::new();
    for l in &c.eval_cheap {
        out.push((l.text.clone(), Tier::Cheap));
    }
    for l in &c.eval_strong {
        out.push((l.text.clone(), Tier::Strong));
    }
    out
}

/// Outcome of one eval example (R4 / `modron router eval`).
#[derive(Debug)]
pub struct EvalCase {
    pub text: String,
    pub expected: Tier,
    pub chosen: Tier,
    pub reason: String,
}

impl EvalCase {
    pub fn correct(&self) -> bool {
        self.expected == self.chosen
    }
}

/// Score `selector` against the held-out eval set.
pub fn run_eval(selector: &dyn ModelSelector) -> Vec<EvalCase> {
    eval_set()
        .into_iter()
        .map(|(text, expected)| {
            let choice = selector.select(&SelectRequest {
                task: text.clone(),
                action: ActionKind::Chat,
                context_tokens: 1_000,
                budget_remaining_usd: None,
            });
            let chosen = match lookup(&choice.model) {
                Some(info) => info.tier,
                None => Tier::Strong,
            };
            EvalCase {
                text,
                expected,
                chosen,
                reason: choice.reason,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_is_deterministic_and_normalized() {
        let a = embed("implement rate limiting");
        let b = embed("implement rate limiting");
        assert_eq!(a, b, "same text, same vector");
        let norm: f32 = a.iter().map(|x| x * x).sum();
        assert!((norm - 1.0).abs() < 1e-4, "L2-normalized: {norm}");
    }

    #[test]
    fn similar_texts_have_high_cosine() {
        let a = embed("summarize the readme");
        let b = embed("summarize the readme file");
        let c = embed("implement rate limiting");
        assert!(cosine(&a, &b) > 0.5, "similar: {}", cosine(&a, &b));
        assert!(
            cosine(&a, &c) < cosine(&a, &b),
            "dissimilar text must score lower"
        );
    }

    #[test]
    fn eval_accuracy_at_least_ninety_percent() {
        let router = LearnedRouter::new(HeuristicSelector::default());
        let cases = run_eval(&router);
        assert!(!cases.is_empty(), "eval set must not be empty");
        let correct = cases.iter().filter(|c| c.correct()).count();
        let accuracy = correct as f64 / cases.len() as f64;
        assert!(
            accuracy >= 0.90,
            "eval accuracy {accuracy:.0}% ({correct}/{}): {:?}",
            cases.len(),
            cases.iter().filter(|c| !c.correct()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn ambiguous_task_falls_back_to_heuristic() {
        let router = LearnedRouter::new(HeuristicSelector::default());
        // Deliberately tier-neutral text.
        let choice = router.select(&SelectRequest {
            task: "asdf qwerty zzzz".to_string(),
            action: ActionKind::Chat,
            context_tokens: 1_000,
            budget_remaining_usd: None,
        });
        assert!(
            choice.reason.starts_with("learned: margin below"),
            "fallback reason: {}",
            choice.reason
        );
    }

    #[test]
    fn every_eval_example_routes_to_expected_tier() {
        // The product claim: whatever the path (confident centroid or
        // heuristic fallback), the FINAL decision matches the label.
        let router = LearnedRouter::new(HeuristicSelector::default());
        for case in run_eval(&router) {
            assert!(
                case.correct(),
                "expected {:?}, chose {:?} ({}): {}",
                case.expected,
                case.chosen,
                case.reason,
                case.text
            );
        }
    }

    #[test]
    fn most_eval_examples_are_confident_centroid_calls() {
        // The fallback exists for the weird tail; if MOST examples need it,
        // the corpus is too thin to claim "learned" routing.
        let router = LearnedRouter::new(HeuristicSelector::default());
        let decided = eval_set()
            .iter()
            .filter(|(text, _)| router.decide(text).is_some())
            .count();
        let total = eval_set().len();
        assert!(
            decided * 100 / total >= 70,
            "only {decided}/{total} eval examples clear the margin — corpus too thin"
        );
    }
}
