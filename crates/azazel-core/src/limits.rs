//! The run's cost cap: a warning once spend reaches `limits.warn_cost_usd`, and no further
//! requests from any agent once it reaches `limits.max_cost_usd`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;

use crate::ext::{Event, EventHandler, EventKind, ExtCtx, ExtError, Extension, Level, Outcome, Registrar};
use crate::ui::Ui;

/// What the run has spent so far in USD, over every agent. Models without a known price add
/// nothing, so they never trip the cap.
#[derive(Clone)]
pub struct Spend(pub Arc<dyn Fn() -> f64 + Send + Sync>);

/// The cost cap of the run, for work outside agent turns (memory extraction, summaries) that
/// should not spend past it either.
pub struct Cap(pub f64);

/// Whether the run has spent its cost cap; false when there is no cap.
pub fn reached(services: &azazel_agent::Services) -> bool {
    match (services.get::<Cap>(), services.get::<Spend>()) {
        (Some(cap), Some(spend)) => cap.0 > 0.0 && (spend.0)() >= cap.0,
        _ => false,
    }
}

/// Registers the cap. A limit of 0 is no limit.
pub struct LimitsExt {
    pub max_cost_usd: f64,
    pub warn_cost_usd: f64,
}

impl Extension for LimitsExt {
    fn id(&self) -> &str {
        "limits"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.service(Arc::new(Cap(self.max_cost_usd)));
        if self.max_cost_usd > 0.0 || self.warn_cost_usd > 0.0 {
            r.on(
                EventKind::TurnStart,
                Arc::new(CostCap {
                    max: self.max_cost_usd,
                    warn: self.warn_cost_usd,
                    warned: AtomicBool::new(false),
                }),
            );
        }
        Ok(())
    }
}

struct CostCap {
    max: f64,
    warn: f64,
    warned: AtomicBool,
}

#[async_trait]
impl EventHandler for CostCap {
    async fn handle(&self, ev: &mut Event, ctx: &ExtCtx) -> Outcome {
        if !matches!(ev, Event::TurnStart { .. }) {
            return Outcome::Continue;
        }
        let Some(spend) = ctx.services.get::<Spend>() else {
            return Outcome::Continue;
        };
        let spent = (spend.0)();
        if self.max > 0.0 && spent >= self.max {
            return Outcome::Block {
                reason: format!(
                    "the cost limit of ${:.2} is reached (${spent:.2} spent); raise it with --max-cost or \
                     [limits] max_cost_usd",
                    self.max
                ),
            };
        }
        if self.warn > 0.0 && spent >= self.warn && !self.warned.swap(true, Ordering::SeqCst) {
            let text = format!("${spent:.2} spent, past the warning at ${:.2}", self.warn);
            match ctx.services.get::<Ui>() {
                Some(ui) => ui.0.notify(Level::Warn, &text),
                None => tracing::warn!("{text}"),
            }
        }
        Outcome::Continue
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use azazel_agent::{AgentInfo, Services};

    use super::*;
    use crate::ext::{EventBus, Registry};
    use crate::ui::{ApprovalDecision, ApprovalRequest, PlanDecision, Question, UiBridge, UiError};

    #[derive(Default)]
    struct Notes(Mutex<Vec<String>>);

    #[async_trait]
    impl UiBridge for Notes {
        async fn ask(&self, _: &Question) -> Result<Vec<String>, UiError> {
            Err(UiError::NoUser)
        }
        async fn confirm(&self, _: &str) -> Result<bool, UiError> {
            Err(UiError::NoUser)
        }
        async fn select(&self, _: &str, _: &[String]) -> Result<usize, UiError> {
            Err(UiError::NoUser)
        }
        async fn input(&self, _: &str) -> Result<String, UiError> {
            Err(UiError::NoUser)
        }
        fn notify(&self, _: Level, text: &str) {
            self.0.lock().unwrap().push(text.to_string());
        }
        async fn approval(&self, _: ApprovalRequest) -> Option<ApprovalDecision> {
            None
        }
        async fn review_plan(&self, _: &std::path::Path, _: &str) -> Result<PlanDecision, UiError> {
            Err(UiError::NoUser)
        }
    }

    fn turn() -> Event {
        Event::TurnStart {
            agent: AgentInfo {
                id: "lead".into(),
                def: "lead".into(),
                parent: None,
                depth: 0,
                session_id: "s".into(),
            },
        }
    }

    #[tokio::test]
    async fn warns_once_then_blocks_every_turn_past_the_cap() {
        let spent = Arc::new(Mutex::new(0.0_f64));
        let services = Arc::new(Services::new());
        let reading = spent.clone();
        services.insert(Arc::new(Spend(Arc::new(move || *reading.lock().unwrap()))));
        let notes = Arc::new(Notes::default());
        services.insert(Arc::new(Ui(notes.clone())));
        let registry = Registry::new(ExtCtx {
            services,
            cwd: ".".into(),
            session_id: "s".into(),
        });
        registry
            .load(&LimitsExt {
                max_cost_usd: 5.0,
                warn_cost_usd: 1.0,
            })
            .unwrap();
        let bus: &EventBus = &registry.bus;

        assert_eq!(bus.emit(&mut turn()).await, Outcome::Continue);
        *spent.lock().unwrap() = 1.5;
        assert_eq!(bus.emit(&mut turn()).await, Outcome::Continue);
        assert_eq!(bus.emit(&mut turn()).await, Outcome::Continue);
        assert_eq!(
            *notes.0.lock().unwrap(),
            ["$1.50 spent, past the warning at $1.00"]
        );

        *spent.lock().unwrap() = 5.02;
        let Outcome::Block { reason } = bus.emit(&mut turn()).await else {
            panic!("the cap did not block");
        };
        assert_eq!(
            reason,
            "the cost limit of $5.00 is reached ($5.02 spent); raise it with --max-cost or [limits] max_cost_usd"
        );
    }
}
