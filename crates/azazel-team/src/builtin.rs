//! The agent definitions that ship with azazel.

/// `(file name, text)` of every built-in definition, in name order.
pub const BUILTIN_AGENTS: &[(&str, &str)] = &[
    ("artist.md", include_str!("../agents/artist.md")),
    ("build.md", include_str!("../agents/build.md")),
    ("coder.md", include_str!("../agents/coder.md")),
    ("designer.md", include_str!("../agents/designer.md")),
    ("plan.md", include_str!("../agents/plan.md")),
    ("planner.md", include_str!("../agents/planner.md")),
    ("playtester.md", include_str!("../agents/playtester.md")),
    ("researcher.md", include_str!("../agents/researcher.md")),
    ("reviewer.md", include_str!("../agents/reviewer.md")),
    ("scout.md", include_str!("../agents/scout.md")),
    ("tester.md", include_str!("../agents/tester.md")),
    ("verifier.md", include_str!("../agents/verifier.md")),
];

#[cfg(test)]
mod tests {
    use azazel_core::agents::{AgentDef, AgentDefs, AgentMode, DefSource, discover};

    use super::*;

    const NAMES: [&str; 12] = [
        "artist",
        "build",
        "coder",
        "designer",
        "plan",
        "planner",
        "playtester",
        "researcher",
        "reviewer",
        "scout",
        "tester",
        "verifier",
    ];

    fn builtins() -> AgentDefs {
        discover(BUILTIN_AGENTS, &[])
    }

    /// The patterns under which `tool` is granted by name; empty when it is not listed.
    fn patterns<'a>(def: &'a AgentDef, tool: &str) -> Vec<Option<&'a str>> {
        def.tools
            .iter()
            .flatten()
            .filter(|rule| rule.tool == tool)
            .map(|rule| rule.pattern.as_deref())
            .collect()
    }

    #[test]
    fn every_builtin_parses_and_the_set_is_the_documented_ten_and_two_primaries() {
        let defs = builtins();
        assert!(defs.invalid().is_empty(), "{:?}", defs.invalid());
        let names: Vec<String> = defs.list().iter().map(|def| def.name.clone()).collect();
        assert_eq!(names, NAMES);
        let primaries: Vec<String> = defs.primaries().iter().map(|def| def.name.clone()).collect();
        assert_eq!(primaries, ["build", "plan"]);
        for def in defs.list() {
            assert_eq!(def.source, DefSource::Builtin, "{}", def.name);
            assert!(!def.body.is_empty(), "{} has no system prompt", def.name);
            if def.mode == AgentMode::Primary {
                // The lead keeps the model and the turns the session gives it.
                assert!(def.model.is_none() && def.max_turns.is_none(), "{}", def.name);
                continue;
            }
            assert!(def.model.is_some(), "{} names no model role", def.name);
            assert!(def.max_turns.is_some(), "{} has no turn limit", def.name);
            // Every built-in needs shell, web or write tools, which plan mode denies.
            assert!(!def.plan_mode, "{} is pinned to plan mode", def.name);
        }
    }

    #[test]
    fn the_plan_agent_is_today_s_plan_mode_and_build_is_unlimited() {
        let defs = builtins();
        let plan = defs.primary("plan").unwrap();
        assert!(plan.plan_mode);
        assert!(plan.tools.is_none() && plan.disallowed_tools.is_empty());
        assert!(plan.color.is_some());
        let build = defs.primary("build").unwrap();
        assert!(!build.plan_mode && build.tools.is_none() && build.color.is_none());
        assert!(defs.subagent("plan").is_none() && defs.subagent("build").is_none());
    }

    #[test]
    fn read_only_agents_are_granted_no_write_tools() {
        let defs = builtins();
        for name in ["scout", "reviewer", "researcher", "playtester"] {
            let def = defs.get(name).unwrap();
            assert!(def.tools.is_some(), "{name} is granted every tool");
            for tool in ["write", "edit"] {
                assert!(patterns(&def, tool).is_empty(), "{name} is granted {tool}");
            }
        }
        let reviewer = defs.get("reviewer").unwrap();
        assert_eq!(
            patterns(&reviewer, "shell"),
            [Some("git diff:*"), Some("git log:*"), Some("git show:*")]
        );
    }

    #[test]
    fn agents_spawn_only_other_builtins() {
        let defs = builtins();
        for def in defs.list() {
            for spawned in &def.spawns {
                assert!(
                    defs.get(spawned).is_some(),
                    "{} spawns unknown `{spawned}`",
                    def.name
                );
            }
            let expected: &[&str] = match def.name.as_str() {
                "coder" | "designer" => &["scout"],
                _ => &[],
            };
            assert_eq!(def.spawns, expected, "{}", def.name);
        }
    }
}
